//! Markdown-backed business knowledge wiki.
//!
//! `knowledge_wiki` treats operator-maintained Markdown as the human-editable
//! source of truth while database rows hold recoverable machine state:
//! document hashes, chunk previews, relation indexes, and sync status.

mod context;
mod parser;
mod types;

pub const DEFAULT_KNOWLEDGE_WIKI_NAMESPACE: &str = "global";

pub use context::WikiContextProvider;
pub use parser::{
    WIKI_ROOT, chunk_markdown, content_sha256, frontmatter_i32, frontmatter_scalar,
    frontmatter_string_list, is_indexable_wiki_path, is_indexable_wiki_path_under,
    normalize_wiki_root, parse_frontmatter, parse_relation_card, parse_relation_card_under,
    relation_id,
};
pub use types::{
    FrontMatter, RelationCard, RelationSource, WikiChunkRecord, WikiDocumentRecord,
    WikiRelationRecord, WikiSearchHit, WikiSyncReport, WikiTriggerRecord,
};
