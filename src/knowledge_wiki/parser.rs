use sha2::{Digest, Sha256};

use super::types::{FrontMatter, RelationCard, RelationSource};

pub const WIKI_ROOT: &str = "wiki/";

const INDEXABLE_SUFFIXES: &[&str] = &[
    "notes/",
    "sources/",
    "summaries/",
    "entities/",
    "relations/",
    "schema/entities/",
    "schema/relations/",
];

pub fn is_indexable_wiki_path(path: &str) -> bool {
    is_indexable_wiki_path_under(path, WIKI_ROOT)
}

pub fn is_indexable_wiki_path_under(path: &str, wiki_root: &str) -> bool {
    let normalized = normalize_path(path);
    let root = normalize_wiki_root(wiki_root).unwrap_or_else(|_| WIKI_ROOT.to_string());
    normalized.ends_with(".md")
        && !normalized.contains("..")
        && INDEXABLE_SUFFIXES
            .iter()
            .any(|suffix| normalized.starts_with(&format!("{root}{suffix}")))
}

pub fn normalize_wiki_root(root: &str) -> Result<String, String> {
    let normalized = normalize_path(root);
    if normalized.is_empty() {
        return Err("wiki_root cannot be empty".to_string());
    }
    if normalized.starts_with('/') || normalized.contains("..") {
        return Err("wiki_root must be a relative workspace path without `..`".to_string());
    }
    Ok(format!("{}/", normalized.trim_end_matches('/')))
}

pub fn content_sha256(content: &str) -> String {
    let digest = Sha256::digest(content.as_bytes());
    hex::encode(digest)
}

pub fn relation_id(subject: &str, predicate: &str, object: &str) -> String {
    let material = format!(
        "{}\0{}\0{}",
        subject.trim().to_lowercase(),
        predicate.trim().to_lowercase(),
        object.trim().to_lowercase()
    );
    let digest = Sha256::digest(material.as_bytes());
    format!("rel_{}", &hex::encode(digest)[..24])
}

pub fn parse_frontmatter(content: &str) -> (FrontMatter, &str) {
    let Some(rest) = content.strip_prefix("---\n") else {
        return (FrontMatter::default(), content);
    };
    let Some(end) = rest.find("\n---\n") else {
        return (FrontMatter::default(), content);
    };
    let fm = &rest[..end];
    let body = &rest[end + "\n---\n".len()..];
    (
        FrontMatter {
            fields: parse_simple_yaml(fm),
        },
        body,
    )
}

pub fn frontmatter_scalar(fm: &FrontMatter, key: &str) -> Option<String> {
    scalar(fm, key)
}

pub fn frontmatter_string_list(fm: &FrontMatter, key: &str) -> Vec<String> {
    match fm.fields.get(key) {
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
            .filter(|s| !s.is_empty())
            .collect(),
        Some(serde_json::Value::String(s)) => s
            .split(',')
            .map(|item| item.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

pub fn frontmatter_i32(fm: &FrontMatter, key: &str) -> Option<i32> {
    scalar(fm, key).and_then(|v| v.parse::<i32>().ok())
}

pub fn chunk_markdown(path: &str, content: &str, max_chars: usize) -> Vec<(String, i32, String)> {
    let (_, body) = parse_frontmatter(content);
    let mut chunks = Vec::new();
    let mut current = String::new();
    let limit = max_chars.max(256);

    for block in body.split("\n\n") {
        let block = block.trim();
        if block.is_empty() {
            continue;
        }
        let projected = if current.is_empty() {
            block.len()
        } else {
            current.len() + 2 + block.len()
        };
        if projected > limit && !current.is_empty() {
            push_chunk(path, &mut chunks, std::mem::take(&mut current));
        }
        if block.len() > limit {
            for piece in split_long_block(block, limit) {
                push_chunk(path, &mut chunks, piece);
            }
        } else if current.is_empty() {
            current.push_str(block);
        } else {
            current.push_str("\n\n");
            current.push_str(block);
        }
    }
    if !current.trim().is_empty() {
        push_chunk(path, &mut chunks, current);
    }
    chunks
}

pub fn parse_relation_card(path: &str, content: &str) -> Result<Option<RelationCard>, String> {
    parse_relation_card_under(path, content, WIKI_ROOT)
}

pub fn parse_relation_card_under(
    path: &str,
    content: &str,
    wiki_root: &str,
) -> Result<Option<RelationCard>, String> {
    let root = normalize_wiki_root(wiki_root)?;
    if !normalize_path(path).starts_with(&format!("{root}relations/")) {
        return Ok(None);
    }
    let (fm, _) = parse_frontmatter(content);
    let Some(subject) = scalar(&fm, "subject") else {
        return Ok(None);
    };
    let Some(predicate) = scalar(&fm, "predicate") else {
        return Ok(None);
    };
    let Some(object) = scalar(&fm, "object") else {
        return Ok(None);
    };
    if subject.trim().is_empty() || predicate.trim().is_empty() || object.trim().is_empty() {
        return Err(format!(
            "relation card {path} has an empty subject/predicate/object"
        ));
    }
    let confidence = scalar(&fm, "confidence")
        .and_then(|v| v.parse::<f32>().ok())
        .unwrap_or(0.5)
        .clamp(0.0, 1.0);
    let status = scalar(&fm, "status").unwrap_or_else(|| "proposed".to_string());
    let schema_version = scalar(&fm, "schema_version").and_then(|v| v.parse::<i32>().ok());
    let sources = parse_sources(&fm);

    Ok(Some(RelationCard {
        subject,
        predicate,
        object,
        confidence,
        status,
        sources,
        schema_version,
    }))
}

fn push_chunk(path: &str, chunks: &mut Vec<(String, i32, String)>, content: String) {
    let idx = chunks.len() as i32;
    let material = format!("{path}\0{idx}\0{}", content_sha256(&content));
    let digest = Sha256::digest(material.as_bytes());
    let chunk_id = format!("mw_{}", &hex::encode(digest)[..24]);
    chunks.push((chunk_id, idx, content));
}

fn split_long_block(block: &str, limit: usize) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut current = String::new();
    for ch in block.chars() {
        current.push(ch);
        if current.len() >= limit {
            pieces.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        pieces.push(current);
    }
    pieces
}

fn scalar(fm: &FrontMatter, key: &str) -> Option<String> {
    fm.fields.get(key).and_then(|v| match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    })
}

fn parse_sources(fm: &FrontMatter) -> Vec<RelationSource> {
    let Some(serde_json::Value::Array(items)) = fm.fields.get("sources") else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let obj = item.as_object()?;
            let path = obj.get("path")?.as_str()?.to_string();
            Some(RelationSource {
                path,
                chunk_id: obj
                    .get("chunk_id")
                    .and_then(|v| v.as_str())
                    .map(ToOwned::to_owned),
                quote: obj
                    .get("quote")
                    .and_then(|v| v.as_str())
                    .map(ToOwned::to_owned),
            })
        })
        .collect()
}

fn parse_simple_yaml(input: &str) -> serde_json::Map<String, serde_json::Value> {
    let mut out = serde_json::Map::new();
    let lines: Vec<&str> = input.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if line.trim().is_empty() || line.trim_start().starts_with('#') || line.starts_with(' ') {
            i += 1;
            continue;
        }
        let Some((key, rest)) = line.split_once(':') else {
            i += 1;
            continue;
        };
        let key = key.trim().to_string();
        let rest = rest.trim();
        if rest.is_empty() {
            let mut arr = Vec::new();
            i += 1;
            while i < lines.len() {
                let child = lines[i];
                if !child.starts_with("  - ") {
                    break;
                }
                let raw = child.trim_start_matches("  - ").trim();
                if raw.is_empty() {
                    i += 1;
                    continue;
                }
                if let Some((k, v)) = raw.split_once(':') {
                    let mut obj = serde_json::Map::new();
                    obj.insert(k.trim().to_string(), scalar_value(v.trim()));
                    i += 1;
                    while i < lines.len() && lines[i].starts_with("    ") {
                        if let Some((ck, cv)) = lines[i].trim().split_once(':') {
                            obj.insert(ck.trim().to_string(), scalar_value(cv.trim()));
                        }
                        i += 1;
                    }
                    arr.push(serde_json::Value::Object(obj));
                } else {
                    arr.push(scalar_value(raw));
                    i += 1;
                }
            }
            out.insert(key, serde_json::Value::Array(arr));
            continue;
        }
        out.insert(key, scalar_value(rest));
        i += 1;
    }
    out
}

fn scalar_value(raw: &str) -> serde_json::Value {
    let trimmed = raw.trim();
    if trimmed.eq_ignore_ascii_case("true") {
        return serde_json::Value::Bool(true);
    }
    if trimmed.eq_ignore_ascii_case("false") {
        return serde_json::Value::Bool(false);
    }
    if let Ok(n) = trimmed.parse::<i64>() {
        return serde_json::json!(n);
    }
    if let Ok(n) = trimmed.parse::<f64>() {
        return serde_json::json!(n);
    }
    let unquoted = trimmed
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(trimmed)
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
        .unwrap_or_else(|| {
            trimmed
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .unwrap_or(trimmed)
        });
    serde_json::Value::String(unquoted.to_string())
}

fn normalize_path(path: &str) -> String {
    path.trim().trim_start_matches('/').replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relation_card_parses_spo_and_sources() {
        let content = r#"---
subject: org:acme
predicate: purchased
object: product:ironclaw
confidence: 0.82
status: proposed
schema_version: 2
sources:
  - path: wiki/sources/email/acme.md
    chunk_id: mw_abc
    quote: "Acme purchased licenses"
---
# Acme purchased IronClaw
"#;
        let card = parse_relation_card("wiki/relations/acme.md", content)
            .unwrap()
            .unwrap();
        assert_eq!(card.subject, "org:acme");
        assert_eq!(card.predicate, "purchased");
        assert_eq!(card.object, "product:ironclaw");
        assert_eq!(card.sources.len(), 1);
        assert_eq!(card.schema_version, Some(2));
    }

    #[test]
    fn indexable_paths_are_scoped_to_wiki() {
        assert!(is_indexable_wiki_path("wiki/notes/a.md"));
        assert!(is_indexable_wiki_path("wiki/relations/a.md"));
        assert!(!is_indexable_wiki_path("MEMORY.md"));
        assert!(!is_indexable_wiki_path("wiki/../AGENTS.md"));
    }

    #[test]
    fn indexable_paths_support_custom_root() {
        assert!(is_indexable_wiki_path_under(
            "knowledge/wiki/notes/a.md",
            "knowledge/wiki"
        ));
        assert!(
            parse_relation_card_under(
                "knowledge/wiki/relations/acme.md",
                "---\nsubject: a\npredicate: b\nobject: c\n---\n",
                "knowledge/wiki"
            )
            .unwrap()
            .is_some()
        );
        assert!(!is_indexable_wiki_path_under(
            "wiki/notes/a.md",
            "knowledge/wiki"
        ));
    }
}
