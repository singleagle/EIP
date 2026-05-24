use async_trait::async_trait;

use crate::db::KnowledgeWikiStore;
use crate::db::libsql::{LibSqlBackend, fmt_ts, get_opt_text, get_text, get_ts};
use crate::error::DatabaseError;
use crate::knowledge_wiki::{
    WikiChunkRecord, WikiDocumentRecord, WikiRelationRecord, WikiSearchHit, WikiTriggerRecord,
};

#[async_trait]
impl KnowledgeWikiStore for LibSqlBackend {
    async fn upsert_wiki_document(&self, record: &WikiDocumentRecord) -> Result<(), DatabaseError> {
        let conn = self.connect().await?;
        conn.execute(
            "INSERT INTO knowledge_wiki_docs \
             (namespace, path, content_sha256, frontmatter_json, doc_type, priority, \
              max_inject_chars, indexed_at, status) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
             ON CONFLICT (namespace, path) DO UPDATE SET \
                content_sha256 = excluded.content_sha256, \
                frontmatter_json = excluded.frontmatter_json, \
                doc_type = excluded.doc_type, \
                priority = excluded.priority, \
                max_inject_chars = excluded.max_inject_chars, \
                indexed_at = excluded.indexed_at, \
                status = excluded.status",
            libsql::params![
                record.namespace.clone(),
                record.path.clone(),
                record.content_sha256.clone(),
                serde_json::to_string(&record.frontmatter_json)
                    .map_err(|e| DatabaseError::Serialization(e.to_string()))?,
                record.doc_type.clone(),
                record.priority as i64,
                record.max_inject_chars.map(i64::from),
                fmt_ts(&record.indexed_at),
                record.status.clone(),
            ],
        )
        .await?;
        Ok(())
    }

    async fn replace_wiki_chunks(
        &self,
        namespace: &str,
        path: &str,
        chunks: &[WikiChunkRecord],
    ) -> Result<(), DatabaseError> {
        let conn = self.connect().await?;
        let tx = conn.transaction().await?;
        tx.execute(
            "DELETE FROM knowledge_wiki_chunks WHERE namespace = ?1 AND path = ?2",
            libsql::params![namespace, path],
        )
        .await?;
        for chunk in chunks {
            tx.execute(
                "INSERT INTO knowledge_wiki_chunks \
                 (chunk_id, namespace, path, chunk_index, content_sha256, chunk_sha256, \
                  preview, token_count, indexed_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                libsql::params![
                    chunk.chunk_id.clone(),
                    chunk.namespace.clone(),
                    chunk.path.clone(),
                    chunk.chunk_index as i64,
                    chunk.content_sha256.clone(),
                    chunk.chunk_sha256.clone(),
                    chunk.preview.clone(),
                    chunk.token_count as i64,
                    fmt_ts(&chunk.indexed_at),
                ],
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn upsert_wiki_relation(&self, record: &WikiRelationRecord) -> Result<(), DatabaseError> {
        let conn = self.connect().await?;
        conn.execute(
            "INSERT INTO knowledge_wiki_relations \
             (relation_id, namespace, path, subject, predicate, object, confidence, status, \
              sources_json, content_sha256, schema_version, indexed_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12) \
             ON CONFLICT (namespace, relation_id) DO UPDATE SET \
                path = excluded.path, subject = excluded.subject, predicate = excluded.predicate, \
                object = excluded.object, confidence = excluded.confidence, status = excluded.status, \
                sources_json = excluded.sources_json, content_sha256 = excluded.content_sha256, \
                schema_version = excluded.schema_version, indexed_at = excluded.indexed_at",
            libsql::params![
                record.relation_id.clone(),
                record.namespace.clone(),
                record.path.clone(),
                record.subject.clone(),
                record.predicate.clone(),
                record.object.clone(),
                record.confidence as f64,
                record.status.clone(),
                serde_json::to_string(&record.sources_json)
                    .map_err(|e| DatabaseError::Serialization(e.to_string()))?,
                record.content_sha256.clone(),
                record.schema_version.map(i64::from),
                fmt_ts(&record.indexed_at),
            ],
        )
        .await?;
        Ok(())
    }

    async fn replace_wiki_triggers(
        &self,
        namespace: &str,
        path: &str,
        triggers: &[WikiTriggerRecord],
    ) -> Result<(), DatabaseError> {
        let conn = self.connect().await?;
        let tx = conn.transaction().await?;
        tx.execute(
            "DELETE FROM knowledge_wiki_triggers WHERE namespace = ?1 AND path = ?2",
            libsql::params![namespace, path],
        )
        .await?;
        for trigger in triggers {
            tx.execute(
                "INSERT INTO knowledge_wiki_triggers \
                 (namespace, path, keyword, priority, status, indexed_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                libsql::params![
                    trigger.namespace.clone(),
                    trigger.path.clone(),
                    trigger.keyword.clone(),
                    trigger.priority as i64,
                    trigger.status.clone(),
                    fmt_ts(&trigger.indexed_at),
                ],
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn get_wiki_document_hash(
        &self,
        namespace: &str,
        path: &str,
    ) -> Result<Option<String>, DatabaseError> {
        let conn = self.connect().await?;
        let mut rows = conn
            .query(
                "SELECT content_sha256 FROM knowledge_wiki_docs WHERE namespace = ?1 AND path = ?2",
                libsql::params![namespace, path],
            )
            .await?;
        let Some(row) = rows.next().await? else {
            return Ok(None);
        };
        Ok(row.get::<String>(0).ok())
    }

    async fn knowledge_wiki_search(
        &self,
        namespace: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<WikiSearchHit>, DatabaseError> {
        let conn = self.connect().await?;
        let pattern = format!("%{}%", query);
        let mut rows = conn
            .query(
                "SELECT kind, path, chunk_id, relation_id, subject, predicate, object, status, \
                        doc_type, priority, max_inject_chars, preview, score, indexed_at \
                   FROM ( \
                    SELECT 'chunk' AS kind, c.path, c.chunk_id, NULL AS relation_id, \
                           NULL AS subject, NULL AS predicate, NULL AS object, \
                           d.status AS status, d.doc_type, d.priority, d.max_inject_chars, \
                           c.preview, 1.0 + (d.priority / 1000.0) AS score, c.indexed_at \
                      FROM knowledge_wiki_chunks c \
                      JOIN knowledge_wiki_docs d ON d.namespace = c.namespace AND d.path = c.path \
                     WHERE c.namespace = ?1 AND d.status != 'disabled' AND \
                           (c.preview LIKE ?2 OR c.path LIKE ?2 OR d.frontmatter_json LIKE ?2) \
                    UNION ALL \
                    SELECT 'trigger' AS kind, c.path, c.chunk_id, NULL AS relation_id, \
                           NULL AS subject, NULL AS predicate, NULL AS object, \
                           d.status AS status, d.doc_type, d.priority, d.max_inject_chars, \
                           c.preview, 2.0 + (t.priority / 1000.0) AS score, c.indexed_at \
                      FROM knowledge_wiki_triggers t \
                      JOIN knowledge_wiki_docs d ON d.namespace = t.namespace AND d.path = t.path \
                      JOIN knowledge_wiki_chunks c ON c.namespace = t.namespace AND c.path = t.path \
                     WHERE t.namespace = ?1 AND t.status != 'disabled' AND d.status != 'disabled' \
                       AND ?4 LIKE '%' || t.keyword || '%' \
                    UNION ALL \
                    SELECT 'relation' AS kind, r.path, NULL AS chunk_id, r.relation_id, \
                           r.subject, r.predicate, r.object, r.status, \
                           d.doc_type, d.priority, d.max_inject_chars, \
                           subject || ' ' || predicate || ' ' || object AS preview, \
                           r.confidence + (d.priority / 1000.0) AS score, r.indexed_at \
                      FROM knowledge_wiki_relations r \
                      LEFT JOIN knowledge_wiki_docs d ON d.namespace = r.namespace AND d.path = r.path \
                     WHERE r.namespace = ?1 AND \
                           (r.subject LIKE ?2 OR r.predicate LIKE ?2 OR r.object LIKE ?2 OR r.path LIKE ?2) \
                   ) hits \
                  ORDER BY score DESC, indexed_at DESC \
                  LIMIT ?3",
                libsql::params![namespace, pattern, limit.min(100) as i64, query],
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(WikiSearchHit {
                kind: get_text(&row, 0),
                path: get_text(&row, 1),
                chunk_id: get_opt_text(&row, 2),
                relation_id: get_opt_text(&row, 3),
                subject: get_opt_text(&row, 4),
                predicate: get_opt_text(&row, 5),
                object: get_opt_text(&row, 6),
                status: get_opt_text(&row, 7),
                doc_type: get_opt_text(&row, 8),
                priority: row.get::<i64>(9).unwrap_or(0) as i32,
                max_inject_chars: row.get::<i64>(10).ok().map(|v| v as i32),
                preview: get_text(&row, 11),
                score: row.get::<f64>(12).unwrap_or(0.0) as f32,
                indexed_at: get_ts(&row, 13),
            });
        }
        Ok(out)
    }

    async fn knowledge_wiki_status(
        &self,
        namespace: &str,
    ) -> Result<serde_json::Value, DatabaseError> {
        let conn = self.connect().await?;
        let count = |table: &'static str| {
            let conn = &conn;
            async move {
                let sql = format!("SELECT COUNT(*) FROM {table} WHERE namespace = ?1");
                let mut rows = conn.query(&sql, libsql::params![namespace]).await?;
                let Some(row) = rows.next().await? else {
                    return Ok::<i64, DatabaseError>(0);
                };
                Ok(row.get::<i64>(0).unwrap_or(0))
            }
        };
        Ok(serde_json::json!({
            "documents": count("knowledge_wiki_docs").await?,
            "chunks": count("knowledge_wiki_chunks").await?,
            "relations": count("knowledge_wiki_relations").await?,
            "triggers": count("knowledge_wiki_triggers").await?,
        }))
    }
}
