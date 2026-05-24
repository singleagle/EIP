use std::collections::HashSet;
use std::sync::Arc;

use crate::db::Database;
use crate::error::DatabaseError;
use crate::knowledge_wiki::{DEFAULT_KNOWLEDGE_WIKI_NAMESPACE, WikiSearchHit};

const DEFAULT_LIMIT: usize = 5;
const DEFAULT_BUDGET_CHARS: usize = 4_000;

pub struct WikiContextProvider {
    db: Arc<dyn Database>,
    namespace: String,
    limit: usize,
    budget_chars: usize,
}

impl WikiContextProvider {
    pub fn new(db: Arc<dyn Database>) -> Self {
        Self {
            db,
            namespace: DEFAULT_KNOWLEDGE_WIKI_NAMESPACE.to_string(),
            limit: DEFAULT_LIMIT,
            budget_chars: DEFAULT_BUDGET_CHARS,
        }
    }

    pub async fn context_for_prompt(&self, prompt: &str) -> Result<Option<String>, DatabaseError> {
        let hits = self
            .db
            .knowledge_wiki_search(&self.namespace, prompt, self.limit.saturating_mul(4))
            .await?;
        let hits = select_hits(hits, self.limit, self.budget_chars);
        if hits.is_empty() {
            return Ok(None);
        }
        Ok(Some(format_context(&self.namespace, &hits)))
    }
}

fn select_hits(hits: Vec<WikiSearchHit>, limit: usize, budget_chars: usize) -> Vec<WikiSearchHit> {
    let mut selected = Vec::new();
    let mut seen = HashSet::new();
    let mut used = 0usize;
    for mut hit in hits {
        if hit.status.as_deref() == Some("disabled") {
            continue;
        }
        let key = format!("{}:{}", hit.kind, hit.path);
        if !seen.insert(key) {
            continue;
        }
        let max_chars = hit
            .max_inject_chars
            .and_then(|v| usize::try_from(v).ok())
            .filter(|v| *v > 0)
            .unwrap_or(1_200);
        hit.preview = truncate_chars(&hit.preview, max_chars);
        let projected = used.saturating_add(hit.preview.chars().count());
        if projected > budget_chars && !selected.is_empty() {
            break;
        }
        used = projected;
        selected.push(hit);
        if selected.len() >= limit {
            break;
        }
    }
    selected
}

fn format_context(namespace: &str, hits: &[WikiSearchHit]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "<knowledge_wiki_context namespace=\"{}\" trust=\"operator-maintained\">\n",
        escape_attr(namespace)
    ));
    out.push_str(
        "Use this operator-maintained business knowledge as background and rule context. \
         Do not invent facts beyond the cited wiki snippets.\n",
    );
    for hit in hits {
        out.push_str(&format!(
            "\n<entry kind=\"{}\" path=\"{}\" priority=\"{}\"",
            escape_attr(&hit.kind),
            escape_attr(&hit.path),
            hit.priority
        ));
        if let Some(doc_type) = &hit.doc_type {
            out.push_str(&format!(" type=\"{}\"", escape_attr(doc_type)));
        }
        out.push_str(">\n");
        out.push_str(&escape_text(&hit.preview));
        out.push_str("\n</entry>\n");
    }
    out.push_str("</knowledge_wiki_context>");
    out
}

fn truncate_chars(input: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in input.chars().enumerate() {
        if idx >= max_chars {
            out.push_str("...");
            break;
        }
        out.push(ch);
    }
    out
}

fn escape_attr(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn escape_text(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
