//! Markdown-backed business knowledge wiki tool.
//!
//! Markdown remains the editable source of truth under the configured wiki
//! root, while DB rows provide recoverable indexes for chunks and SPO relation
//! cards. The default root is `${IRONCLAW_BASE_DIR}/wiki`, usually
//! `~/.ironclaw/wiki`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;

use crate::context::JobContext;
use crate::db::Database;
use crate::knowledge_wiki::{
    DEFAULT_KNOWLEDGE_WIKI_NAMESPACE, WikiChunkRecord, WikiDocumentRecord, WikiRelationRecord,
    WikiSearchHit, WikiSyncReport, WikiTriggerRecord, chunk_markdown, content_sha256,
    frontmatter_i32, frontmatter_scalar, frontmatter_string_list, is_indexable_wiki_path_under,
    normalize_wiki_root, parse_frontmatter, parse_relation_card_under, relation_id,
};
use crate::tools::builtin::memory::WorkspaceResolver;
use crate::tools::tool::{Tool, ToolError, ToolOutput, require_str};

const DEFAULT_CHUNK_CHARS: usize = 6_000;
const WIKI_ROOT_SETTING_KEY: &str = "knowledge_wiki.root";
const WORKSPACE_ROOT_PREFIX: &str = "workspace:";
const FILE_ROOT_PREFIX: &str = "file:";

pub struct KnowledgeWikiTool {
    tool_name: &'static str,
    resolver: Arc<dyn WorkspaceResolver>,
    db: Arc<dyn Database>,
}

impl KnowledgeWikiTool {
    pub fn new(resolver: Arc<dyn WorkspaceResolver>, db: Arc<dyn Database>) -> Self {
        Self {
            tool_name: "knowledge_wiki",
            resolver,
            db,
        }
    }

    pub fn new_with_name(
        tool_name: &'static str,
        resolver: Arc<dyn WorkspaceResolver>,
        db: Arc<dyn Database>,
    ) -> Self {
        Self {
            tool_name,
            resolver,
            db,
        }
    }
}

#[async_trait]
impl Tool for KnowledgeWikiTool {
    fn name(&self) -> &str {
        self.tool_name
    }

    fn description(&self) -> &str {
        "Synchronize and query the Markdown-backed local knowledge wiki. \
         Markdown under the configured wiki root is the human-editable source \
         of truth; this tool maintains DB indexes for chunks and SPO relation \
         fact cards. Modes: sync, search, read, status, configure."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "mode": {
                    "type": "string",
                    "enum": ["sync", "search", "read", "status", "configure"],
                    "description": "Operation to run."
                },
                "path": {
                    "type": "string",
                    "description": "Path or prefix under the configured wiki root. For file roots, relative paths resolve under the file root."
                },
                "wiki_root": {
                    "type": "string",
                    "description": "Optional wiki root. Absolute paths and file: paths read local Markdown; workspace:<path> reads workspace Markdown. Bare relative paths are treated as workspace paths. Overrides the configured default for this call; configure mode persists it."
                },
                "namespace": {
                    "type": "string",
                    "description": "Knowledge wiki namespace. Defaults to global."
                },
                "query": {
                    "type": "string",
                    "description": "search mode query over chunk previews and SPO relation index."
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum search hits, default 10, max 100."
                },
                "force": {
                    "type": "boolean",
                    "description": "sync mode: reindex even when content hash is unchanged."
                }
            },
            "required": ["mode"]
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = std::time::Instant::now();
        let mode = require_str(&params, "mode")?;
        let result = match mode {
            "sync" => self.sync(params, ctx).await?,
            "search" => self.search(params, ctx).await?,
            "read" => self.read(params, ctx).await?,
            "status" => self.status(params, ctx).await?,
            "configure" => self.configure(params, ctx).await?,
            other => {
                return Err(ToolError::InvalidParameters(format!(
                    "unknown knowledge_wiki mode `{other}`"
                )));
            }
        };
        Ok(ToolOutput::success(result, start.elapsed()))
    }

    fn requires_sanitization(&self) -> bool {
        false
    }

    fn rate_limit_config(&self) -> Option<crate::tools::tool::ToolRateLimitConfig> {
        Some(crate::tools::tool::ToolRateLimitConfig::new(20, 200))
    }
}

impl KnowledgeWikiTool {
    async fn sync(
        &self,
        params: serde_json::Value,
        ctx: &JobContext,
    ) -> Result<serde_json::Value, ToolError> {
        let workspace = self.resolver.resolve(&ctx.user_id).await;
        let wiki_root = self.resolve_wiki_root(&params, ctx).await?;
        let namespace = resolve_namespace(&params)?;
        let prefix = wiki_root.resolve_prefix(params.get("path").and_then(|v| v.as_str()))?;
        let force = params
            .get("force")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let paths = wiki_root.list_paths(&workspace).await?;

        let mut report = WikiSyncReport {
            namespace: namespace.clone(),
            wiki_root: wiki_root.display(),
            wiki_root_kind: wiki_root.kind().to_string(),
            ..WikiSyncReport::default()
        };
        for path in paths {
            if !wiki_root.is_indexable_path(&path)
                || !path.starts_with(prefix.trim_end_matches('/'))
            {
                continue;
            }
            report.scanned += 1;
            let content = match wiki_root.read_content(&workspace, &path).await {
                Ok(content) => content,
                Err(e) => {
                    report.errors.push(format!("{path}: read failed: {e}"));
                    continue;
                }
            };
            let hash = content_sha256(&content);
            if !force {
                match self.db.get_wiki_document_hash(&namespace, &path).await {
                    Ok(Some(existing)) if existing == hash => {
                        report.skipped += 1;
                        continue;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        report
                            .errors
                            .push(format!("{path}: hash lookup failed: {e}"));
                    }
                }
            }

            let indexed_at = Utc::now();
            let (frontmatter, _) = parse_frontmatter(&content);
            let status = frontmatter_scalar(&frontmatter, "status")
                .unwrap_or_else(|| "approved".to_string());
            let priority = frontmatter_i32(&frontmatter, "priority").unwrap_or(0);
            let max_inject_chars = frontmatter_i32(&frontmatter, "max_inject_chars");
            let doc_type = frontmatter_scalar(&frontmatter, "type");
            let keywords = frontmatter_string_list(&frontmatter, "keywords");
            let document_record = WikiDocumentRecord {
                namespace: namespace.clone(),
                path: path.clone(),
                content_sha256: hash.clone(),
                frontmatter_json: serde_json::Value::Object(frontmatter.fields),
                doc_type,
                priority,
                max_inject_chars,
                indexed_at,
                status: status.clone(),
            };
            self.db
                .upsert_wiki_document(&document_record)
                .await
                .map_err(|e| ToolError::ExecutionFailed(format!("index document failed: {e}")))?;

            let chunk_records: Vec<WikiChunkRecord> =
                chunk_markdown(&path, &content, DEFAULT_CHUNK_CHARS)
                    .into_iter()
                    .map(|(chunk_id, chunk_index, chunk)| {
                        let preview: String = chunk.chars().take(500).collect();
                        WikiChunkRecord {
                            chunk_id,
                            namespace: namespace.clone(),
                            path: path.clone(),
                            chunk_index,
                            content_sha256: hash.clone(),
                            chunk_sha256: content_sha256(&chunk),
                            preview,
                            token_count: approx_tokens(&chunk),
                            indexed_at,
                        }
                    })
                    .collect();
            self.db
                .replace_wiki_chunks(&namespace, &path, &chunk_records)
                .await
                .map_err(|e| ToolError::ExecutionFailed(format!("index chunks failed: {e}")))?;
            report.chunks += chunk_records.len();

            let trigger_records: Vec<WikiTriggerRecord> = keywords
                .into_iter()
                .map(|keyword| WikiTriggerRecord {
                    namespace: namespace.clone(),
                    path: path.clone(),
                    keyword,
                    priority,
                    status: status.clone(),
                    indexed_at,
                })
                .collect();
            self.db
                .replace_wiki_triggers(&namespace, &path, &trigger_records)
                .await
                .map_err(|e| ToolError::ExecutionFailed(format!("index triggers failed: {e}")))?;

            match wiki_root.parse_relation_card(&path, &content) {
                Ok(Some(card)) => {
                    let rel_id = relation_id(&card.subject, &card.predicate, &card.object);
                    let relation_record = WikiRelationRecord {
                        relation_id: rel_id,
                        namespace: namespace.clone(),
                        path: path.clone(),
                        subject: card.subject,
                        predicate: card.predicate,
                        object: card.object,
                        confidence: card.confidence,
                        status: card.status,
                        sources_json: serde_json::to_value(&card.sources).map_err(|e| {
                            ToolError::ExecutionFailed(format!("encode relation sources: {e}"))
                        })?,
                        content_sha256: hash.clone(),
                        schema_version: card.schema_version,
                        indexed_at,
                    };
                    self.db
                        .upsert_wiki_relation(&relation_record)
                        .await
                        .map_err(|e| {
                            ToolError::ExecutionFailed(format!("index relation failed: {e}"))
                        })?;
                    report.relations += 1;
                }
                Ok(None) => {}
                Err(e) => report.errors.push(e),
            }

            report.indexed += 1;
        }

        Ok(serde_json::to_value(report)
            .map_err(|e| ToolError::ExecutionFailed(format!("encode sync report: {e}")))?)
    }

    async fn search(
        &self,
        params: serde_json::Value,
        ctx: &JobContext,
    ) -> Result<serde_json::Value, ToolError> {
        let query = require_str(&params, "query")?;
        let wiki_root = self.resolve_wiki_root(&params, ctx).await?;
        let namespace = resolve_namespace(&params)?;
        let limit = params
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(10)
            .min(100) as usize;
        let raw_limit = limit.saturating_mul(5).clamp(limit, 100);
        let hits: Vec<WikiSearchHit> = self
            .db
            .knowledge_wiki_search(&namespace, query, raw_limit)
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("knowledge_wiki search failed: {e}")))?
            .into_iter()
            .filter(|hit| wiki_root.contains_path(&hit.path))
            .take(limit)
            .collect();
        Ok(serde_json::json!({
            "query": query,
            "namespace": namespace,
            "wiki_root": wiki_root.display(),
            "wiki_root_kind": wiki_root.kind(),
            "hits": hits,
        }))
    }

    async fn read(
        &self,
        params: serde_json::Value,
        ctx: &JobContext,
    ) -> Result<serde_json::Value, ToolError> {
        let raw_path = require_str(&params, "path")?;
        let wiki_root = self.resolve_wiki_root(&params, ctx).await?;
        let namespace = resolve_namespace(&params)?;
        let path = wiki_root.resolve_read_path(raw_path)?;
        if !wiki_root.is_indexable_path(&path) {
            return Err(ToolError::InvalidParameters(format!(
                "knowledge_wiki read only accepts indexable Markdown paths under {}",
                wiki_root.display()
            )));
        }
        let workspace = self.resolver.resolve(&ctx.user_id).await;
        let content = wiki_root
            .read_content(&workspace, &path)
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("read wiki document failed: {e}")))?;
        Ok(serde_json::json!({
            "wiki_root": wiki_root.display(),
            "wiki_root_kind": wiki_root.kind(),
            "namespace": namespace,
            "path": path,
            "content_sha256": content_sha256(&content),
            "content": content,
        }))
    }

    async fn status(
        &self,
        params: serde_json::Value,
        ctx: &JobContext,
    ) -> Result<serde_json::Value, ToolError> {
        let wiki_root = self.resolve_wiki_root(&params, ctx).await?;
        let namespace = resolve_namespace(&params)?;
        let mut status = self
            .db
            .knowledge_wiki_status(&namespace)
            .await
            .map_err(|e| {
                ToolError::ExecutionFailed(format!("knowledge_wiki status failed: {e}"))
            })?;
        if let Some(obj) = status.as_object_mut() {
            obj.insert(
                "namespace".to_string(),
                serde_json::Value::String(namespace.clone()),
            );
            obj.insert(
                "wiki_root".to_string(),
                serde_json::Value::String(wiki_root.display()),
            );
            obj.insert(
                "wiki_root_kind".to_string(),
                serde_json::Value::String(wiki_root.kind().to_string()),
            );
        }
        Ok(status)
    }

    async fn configure(
        &self,
        params: serde_json::Value,
        _ctx: &JobContext,
    ) -> Result<serde_json::Value, ToolError> {
        let raw_root = require_str(&params, "wiki_root")?;
        let wiki_root = WikiRoot::parse_configured(raw_root)?;
        let namespace = resolve_namespace(&params)?;
        self.db
            .set_setting(
                DEFAULT_KNOWLEDGE_WIKI_NAMESPACE,
                &root_setting_key(&namespace),
                &serde_json::Value::String(wiki_root.setting_value()),
            )
            .await
            .map_err(|e| {
                ToolError::ExecutionFailed(format!("persist knowledge_wiki root failed: {e}"))
            })?;
        Ok(serde_json::json!({
            "wiki_root": wiki_root.display(),
            "wiki_root_kind": wiki_root.kind(),
            "namespace": namespace,
            "setting_key": root_setting_key(&namespace),
        }))
    }

    async fn resolve_wiki_root(
        &self,
        params: &serde_json::Value,
        _ctx: &JobContext,
    ) -> Result<WikiRoot, ToolError> {
        if let Some(root) = params.get("wiki_root").and_then(|v| v.as_str()) {
            return WikiRoot::parse_configured(root);
        }
        let namespace = resolve_namespace(params)?;
        match self
            .db
            .get_setting(
                DEFAULT_KNOWLEDGE_WIKI_NAMESPACE,
                &root_setting_key(&namespace),
            )
            .await
        {
            Ok(Some(serde_json::Value::String(root))) => WikiRoot::parse_configured(&root),
            Ok(Some(value)) => Err(ToolError::InvalidParameters(format!(
                "{WIKI_ROOT_SETTING_KEY} must be a string, got {value}"
            ))),
            Ok(None) => Ok(WikiRoot::default_file_root()),
            Err(e) => Err(ToolError::ExecutionFailed(format!(
                "load knowledge_wiki root setting failed: {e}"
            ))),
        }
    }
}

fn approx_tokens(text: &str) -> i32 {
    ((text.chars().count() + 3) / 4).min(i32::MAX as usize) as i32
}

fn resolve_namespace(params: &serde_json::Value) -> Result<String, ToolError> {
    let namespace = params
        .get("namespace")
        .and_then(|v| v.as_str())
        .unwrap_or(DEFAULT_KNOWLEDGE_WIKI_NAMESPACE)
        .trim();
    if namespace.is_empty() {
        return Err(ToolError::InvalidParameters(
            "knowledge_wiki namespace cannot be empty".to_string(),
        ));
    }
    if namespace.contains('/') || namespace.contains('\\') || namespace.contains("..") {
        return Err(ToolError::InvalidParameters(
            "knowledge_wiki namespace must not contain path separators or `..`".to_string(),
        ));
    }
    Ok(namespace.to_string())
}

fn root_setting_key(namespace: &str) -> String {
    format!("{WIKI_ROOT_SETTING_KEY}.{namespace}")
}

fn normalize_workspace_path(path: &str) -> String {
    path.trim()
        .trim_start_matches('/')
        .replace('\\', "/")
        .to_string()
}

#[derive(Clone, Debug)]
enum WikiRoot {
    Workspace { root: String },
    File { root: PathBuf },
}

impl WikiRoot {
    fn default_file_root() -> Self {
        Self::File {
            root: crate::bootstrap::ironclaw_base_dir().join("wiki"),
        }
    }

    fn parse_configured(raw: &str) -> Result<Self, ToolError> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err(ToolError::InvalidParameters(
                "wiki_root cannot be empty".to_string(),
            ));
        }
        if let Some(rest) = raw.strip_prefix(WORKSPACE_ROOT_PREFIX) {
            return Ok(Self::Workspace {
                root: normalize_wiki_root(rest).map_err(ToolError::InvalidParameters)?,
            });
        }
        if let Some(rest) = raw.strip_prefix(FILE_ROOT_PREFIX) {
            return Ok(Self::File {
                root: normalize_file_path(rest)?,
            });
        }
        if raw.starts_with('/') || raw.starts_with("~/") {
            return Ok(Self::File {
                root: normalize_file_path(raw)?,
            });
        }
        Ok(Self::Workspace {
            root: normalize_wiki_root(raw).map_err(ToolError::InvalidParameters)?,
        })
    }

    fn kind(&self) -> &'static str {
        match self {
            Self::Workspace { .. } => "workspace",
            Self::File { .. } => "file",
        }
    }

    fn display(&self) -> String {
        match self {
            Self::Workspace { root } => root.clone(),
            Self::File { root } => path_to_string(root),
        }
    }

    fn setting_value(&self) -> String {
        match self {
            Self::Workspace { root } => format!("{WORKSPACE_ROOT_PREFIX}{root}"),
            Self::File { root } => format!("{FILE_ROOT_PREFIX}{}", path_to_string(root)),
        }
    }

    fn resolve_prefix(&self, path: Option<&str>) -> Result<String, ToolError> {
        match self {
            Self::Workspace { root } => {
                let raw = path.unwrap_or(root);
                let normalized = normalize_workspace_path(raw);
                if self.contains_path(&normalized) {
                    Ok(normalized)
                } else {
                    Err(ToolError::InvalidParameters(format!(
                        "path must be under workspace root {root}"
                    )))
                }
            }
            Self::File { root } => {
                let resolved = resolve_file_path(root, path.unwrap_or(""))?;
                Ok(path_to_string(&resolved))
            }
        }
    }

    fn resolve_read_path(&self, raw: &str) -> Result<String, ToolError> {
        match self {
            Self::Workspace { .. } => Ok(normalize_workspace_path(raw)),
            Self::File { root } => resolve_file_path(root, raw).map(|p| path_to_string(&p)),
        }
    }

    fn contains_path(&self, path: &str) -> bool {
        match self {
            Self::Workspace { root } => {
                let path = normalize_workspace_path(path);
                let root = root.trim_end_matches('/');
                path == root || path.starts_with(&format!("{root}/"))
            }
            Self::File { root } => {
                let Ok(path) = normalize_file_path(path) else {
                    return false;
                };
                path == *root || path.starts_with(root)
            }
        }
    }

    fn is_indexable_path(&self, path: &str) -> bool {
        match self {
            Self::Workspace { root } => is_indexable_wiki_path_under(path, root),
            Self::File { root } => {
                let Ok(path) = normalize_file_path(path) else {
                    return false;
                };
                let Ok(relative) = path.strip_prefix(root) else {
                    return false;
                };
                let relative = relative.to_string_lossy().replace('\\', "/");
                relative.ends_with(".md")
                    && !relative.contains("..")
                    && matches!(
                        indexable_top_level(&relative),
                        Some("notes")
                            | Some("sources")
                            | Some("summaries")
                            | Some("entities")
                            | Some("relations")
                            | Some("schema/entities")
                            | Some("schema/relations")
                    )
            }
        }
    }

    async fn list_paths(
        &self,
        workspace: &Arc<crate::workspace::Workspace>,
    ) -> Result<Vec<String>, ToolError> {
        match self {
            Self::Workspace { .. } => workspace
                .list_all()
                .await
                .map(|paths| {
                    paths
                        .into_iter()
                        .map(|p| normalize_workspace_path(&p))
                        .collect()
                })
                .map_err(|e| ToolError::ExecutionFailed(format!("list wiki paths failed: {e}"))),
            Self::File { root } => list_markdown_files(root).await,
        }
    }

    async fn read_content(
        &self,
        workspace: &Arc<crate::workspace::Workspace>,
        path: &str,
    ) -> Result<String, String> {
        match self {
            Self::Workspace { .. } => workspace
                .read(path)
                .await
                .map(|doc| doc.content)
                .map_err(|e| e.to_string()),
            Self::File { .. } => tokio::fs::read_to_string(path)
                .await
                .map_err(|e| e.to_string()),
        }
    }

    fn parse_relation_card(
        &self,
        path: &str,
        content: &str,
    ) -> Result<Option<crate::knowledge_wiki::RelationCard>, String> {
        match self {
            Self::Workspace { root } => parse_relation_card_under(path, content, root),
            Self::File { root } => {
                let path = normalize_file_path(path).map_err(|e| e.to_string())?;
                let relative = path
                    .strip_prefix(root)
                    .map_err(|_| format!("relation card path is outside {}", root.display()))?
                    .to_string_lossy()
                    .replace('\\', "/");
                if !relative.starts_with("relations/") {
                    return Ok(None);
                }
                parse_relation_card_under(&format!("wiki/{relative}"), content, "wiki/")
            }
        }
    }
}

fn normalize_file_path(raw: &str) -> Result<PathBuf, ToolError> {
    let expanded = expand_home(raw)?;
    if expanded.as_os_str().is_empty() {
        return Err(ToolError::InvalidParameters(
            "wiki_root cannot be empty".to_string(),
        ));
    }
    if expanded
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(ToolError::InvalidParameters(
            "file wiki_root must not contain `..`".to_string(),
        ));
    }
    Ok(expanded)
}

fn expand_home(raw: &str) -> Result<PathBuf, ToolError> {
    if raw == "~" {
        return dirs::home_dir()
            .ok_or_else(|| ToolError::InvalidParameters("home directory not found".to_string()));
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return dirs::home_dir()
            .map(|home| home.join(rest))
            .ok_or_else(|| ToolError::InvalidParameters("home directory not found".to_string()));
    }
    Ok(PathBuf::from(raw))
}

fn resolve_file_path(root: &Path, raw: &str) -> Result<PathBuf, ToolError> {
    let raw = raw.trim();
    let path = if raw.is_empty() {
        root.to_path_buf()
    } else {
        let expanded = expand_home(raw)?;
        if expanded.is_absolute() {
            expanded
        } else {
            root.join(expanded)
        }
    };
    let path = normalize_file_path(&path.to_string_lossy())?;
    if path == root || path.starts_with(root) {
        Ok(path)
    } else {
        Err(ToolError::InvalidParameters(format!(
            "path must be under file root {}",
            root.display()
        )))
    }
}

async fn list_markdown_files(root: &Path) -> Result<Vec<String>, ToolError> {
    let root = root.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        if !root.exists() {
            return Ok(out);
        }
        collect_markdown_files(&root, &mut out)?;
        out.sort();
        Ok(out)
    })
    .await
    .map_err(|e| ToolError::ExecutionFailed(format!("list file wiki paths failed: {e}")))?
}

fn collect_markdown_files(dir: &Path, out: &mut Vec<String>) -> Result<(), ToolError> {
    let entries = std::fs::read_dir(dir).map_err(|e| {
        ToolError::ExecutionFailed(format!("read wiki directory {} failed: {e}", dir.display()))
    })?;
    for entry in entries {
        let entry = entry.map_err(|e| {
            ToolError::ExecutionFailed(format!("read wiki directory entry failed: {e}"))
        })?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|e| ToolError::ExecutionFailed(format!("read wiki file type failed: {e}")))?;
        if file_type.is_dir() {
            collect_markdown_files(&path, out)?;
        } else if file_type.is_file() && path.extension().and_then(|e| e.to_str()) == Some("md") {
            out.push(path_to_string(&path));
        }
    }
    Ok(())
}

fn indexable_top_level(relative: &str) -> Option<&'static str> {
    let relative = relative.trim_start_matches('/');
    if relative.starts_with("schema/entities/") {
        return Some("schema/entities");
    }
    if relative.starts_with("schema/relations/") {
        return Some("schema/relations");
    }
    match relative.split('/').next()? {
        "notes" => Some("notes"),
        "sources" => Some("sources"),
        "summaries" => Some("summaries"),
        "entities" => Some("entities"),
        "relations" => Some("relations"),
        _ => None,
    }
}

fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(all(test, feature = "libsql"))]
mod tests {
    use std::sync::Arc;

    use crate::context::JobContext;
    use crate::tools::builtin::memory::FixedWorkspaceResolver;
    use crate::workspace::Workspace;

    use super::*;

    async fn make_tool() -> (KnowledgeWikiTool, tempfile::TempDir) {
        let (db, db_dir) = crate::testing::test_db().await;
        let workspace = Arc::new(Workspace::new_with_db("wiki-test", Arc::clone(&db)));
        let resolver = Arc::new(FixedWorkspaceResolver::new(workspace));
        (KnowledgeWikiTool::new(resolver, db), db_dir)
    }

    #[tokio::test]
    async fn file_root_must_point_at_wiki_parent_not_sources_dir() {
        let temp = tempfile::tempdir().expect("create temp wiki root");
        let wiki_root = temp.path().join("wiki");
        let sources_dir = wiki_root.join("sources");
        std::fs::create_dir_all(&sources_dir).expect("create sources dir");
        std::fs::write(
            sources_dir.join("report.md"),
            "---\ntype: source\nkeywords: [report]\n---\n\n# Report\n\nBody.",
        )
        .expect("write source doc");

        let (tool, _db_dir) = make_tool().await;
        let ctx = JobContext::with_user("wiki-test", "test", "knowledge wiki sync test");

        let wrong_root_report = tool
            .sync(
                serde_json::json!({
                    "mode": "sync",
                    "namespace": "wrong-root",
                    "wiki_root": format!("file:{}", sources_dir.display()),
                    "force": true
                }),
                &ctx,
            )
            .await
            .expect("sync with sources as root should succeed");
        assert_eq!(wrong_root_report["scanned"], 0);
        assert_eq!(wrong_root_report["indexed"], 0);

        let parent_root_report = tool
            .sync(
                serde_json::json!({
                    "mode": "sync",
                    "namespace": "parent-root",
                    "wiki_root": format!("file:{}", wiki_root.display()),
                    "path": "sources",
                    "force": true
                }),
                &ctx,
            )
            .await
            .expect("sync with wiki parent root should index sources");
        assert_eq!(parent_root_report["scanned"], 1);
        assert_eq!(parent_root_report["indexed"], 1);
    }
}
