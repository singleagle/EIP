use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FrontMatter {
    pub fields: serde_json::Map<String, serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WikiDocumentRecord {
    pub namespace: String,
    pub path: String,
    pub content_sha256: String,
    pub frontmatter_json: serde_json::Value,
    pub doc_type: Option<String>,
    pub priority: i32,
    pub max_inject_chars: Option<i32>,
    pub indexed_at: DateTime<Utc>,
    pub status: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WikiChunkRecord {
    pub chunk_id: String,
    pub namespace: String,
    pub path: String,
    pub chunk_index: i32,
    pub content_sha256: String,
    pub chunk_sha256: String,
    pub preview: String,
    pub token_count: i32,
    pub indexed_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RelationSource {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quote: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RelationCard {
    pub subject: String,
    pub predicate: String,
    pub object: String,
    pub confidence: f32,
    pub status: String,
    pub sources: Vec<RelationSource>,
    #[serde(default)]
    pub schema_version: Option<i32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WikiRelationRecord {
    pub relation_id: String,
    pub namespace: String,
    pub path: String,
    pub subject: String,
    pub predicate: String,
    pub object: String,
    pub confidence: f32,
    pub status: String,
    pub sources_json: serde_json::Value,
    pub content_sha256: String,
    pub schema_version: Option<i32>,
    pub indexed_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WikiTriggerRecord {
    pub namespace: String,
    pub path: String,
    pub keyword: String,
    pub priority: i32,
    pub status: String,
    pub indexed_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WikiSearchHit {
    pub kind: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub predicate: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doc_type: Option<String>,
    pub priority: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_inject_chars: Option<i32>,
    pub preview: String,
    pub score: f32,
    pub indexed_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WikiSyncReport {
    pub namespace: String,
    pub wiki_root: String,
    pub wiki_root_kind: String,
    pub scanned: usize,
    pub indexed: usize,
    pub skipped: usize,
    pub chunks: usize,
    pub relations: usize,
    pub errors: Vec<String>,
}
