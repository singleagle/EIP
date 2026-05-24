CREATE TABLE IF NOT EXISTS knowledge_wiki_docs (
    namespace          TEXT NOT NULL,
    path             TEXT NOT NULL,
    content_sha256   TEXT NOT NULL,
    frontmatter_json JSONB NOT NULL DEFAULT '{}',
    doc_type         TEXT,
    priority         INTEGER NOT NULL DEFAULT 0,
    max_inject_chars INTEGER,
    indexed_at       TIMESTAMPTZ NOT NULL,
    status           TEXT NOT NULL DEFAULT 'approved',
    PRIMARY KEY (namespace, path)
);

CREATE INDEX IF NOT EXISTS idx_knowledge_wiki_docs_indexed_at
    ON knowledge_wiki_docs (indexed_at DESC);

CREATE TABLE IF NOT EXISTS knowledge_wiki_chunks (
    chunk_id        TEXT PRIMARY KEY,
    namespace         TEXT NOT NULL,
    path            TEXT NOT NULL,
    chunk_index     INTEGER NOT NULL,
    content_sha256  TEXT NOT NULL,
    chunk_sha256    TEXT NOT NULL,
    preview         TEXT NOT NULL,
    token_count     INTEGER NOT NULL,
    indexed_at      TIMESTAMPTZ NOT NULL,
    UNIQUE (namespace, path, chunk_index)
);

CREATE INDEX IF NOT EXISTS idx_knowledge_wiki_chunks_user_path
    ON knowledge_wiki_chunks (namespace, path);
CREATE INDEX IF NOT EXISTS idx_knowledge_wiki_chunks_preview
    ON knowledge_wiki_chunks USING gin (to_tsvector('simple', preview));

CREATE TABLE IF NOT EXISTS knowledge_wiki_relations (
    relation_id     TEXT NOT NULL,
    namespace         TEXT NOT NULL,
    path            TEXT NOT NULL,
    subject         TEXT NOT NULL,
    predicate       TEXT NOT NULL,
    object          TEXT NOT NULL,
    confidence      REAL NOT NULL DEFAULT 0.5,
    status          TEXT NOT NULL DEFAULT 'proposed',
    sources_json    JSONB NOT NULL DEFAULT '[]',
    content_sha256  TEXT NOT NULL,
    schema_version  INTEGER,
    indexed_at      TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (namespace, relation_id)
);

CREATE INDEX IF NOT EXISTS idx_knowledge_wiki_relations_spo
    ON knowledge_wiki_relations (namespace, subject, predicate, object);
CREATE INDEX IF NOT EXISTS idx_knowledge_wiki_relations_predicate
    ON knowledge_wiki_relations (namespace, predicate);
CREATE INDEX IF NOT EXISTS idx_knowledge_wiki_relations_status
    ON knowledge_wiki_relations (namespace, status);

CREATE TABLE IF NOT EXISTS knowledge_wiki_triggers (
    namespace     TEXT NOT NULL,
    path          TEXT NOT NULL,
    keyword       TEXT NOT NULL,
    priority      INTEGER NOT NULL DEFAULT 0,
    status        TEXT NOT NULL DEFAULT 'approved',
    indexed_at    TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (namespace, path, keyword)
);

CREATE INDEX IF NOT EXISTS idx_knowledge_wiki_triggers_keyword
    ON knowledge_wiki_triggers (namespace, keyword);
CREATE INDEX IF NOT EXISTS idx_knowledge_wiki_triggers_status
    ON knowledge_wiki_triggers (namespace, status);
