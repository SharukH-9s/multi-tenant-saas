-- Migration: 006_search.sql
-- Purpose: full-text column and the two search indexes (Phase 5, ADR-008).
-- Runs as db_owner. Forward-only.
--
-- Index builds run inside the migration transaction. That is fine at current
-- size (F4). On a large production table build them CONCURRENTLY in a
-- separate, non-transactional step instead (see docs/runbooks/search-indexes.md).

ALTER TABLE document_chunks
    ADD COLUMN tsv_content tsvector
    GENERATED ALWAYS AS (to_tsvector('english'::regconfig, content)) STORED;

CREATE INDEX document_chunks_tsv_idx
    ON document_chunks USING gin (tsv_content);

CREATE INDEX document_chunks_embedding_hnsw_idx
    ON document_chunks USING hnsw (embedding vector_cosine_ops)
    WITH (m = 16, ef_construction = 64);
