-- Migration: 007_tenant_dedicated_schema.sql
-- Purpose: Adds dedicated-schema promotion columns to the tenants table (Phase 5, Option B.1).
-- Runs as db_owner. Forward-only (no down migration).
--
-- Design: dedicated-schema-redesign-analysis.md §Task 9, hnsw-recall-analysis-and-option-b.md §2.
--
-- Two columns are added:
--
--   dedicated_schema  — the PostgreSQL schema name allocated to this tenant when
--                       it has been promoted out of public.document_chunks into its
--                       own schema (e.g. "tenant_018f1a2b…").  NULL means the
--                       tenant still uses the shared public table.  A CHECK
--                       constraint enforces the naming contract so that the value
--                       is safe to interpolate directly into SQL identifiers by
--                       chunk_repo / search_repo (SQL injection defence, see §3
--                       of the analysis doc).
--
--   promotion_fenced  — write fence used by the 5-phase promotion protocol.
--                       Phases 2-3 set this to TRUE to drain in-flight ingestion
--                       workers before the atomic schema flip.  Phase 4 clears it.
--                       Defaults to FALSE so all existing tenants are unaffected.
--
-- Both columns are additive / nullable / constant-defaulted, so PostgreSQL 16
-- satisfies them with a catalog-only update — no table rewrite, no lock longer
-- than AccessExclusiveLock on the catalog row, zero downtime.
--
-- The partial index on (id) WHERE dedicated_schema IS NOT NULL lets the
-- ingestion worker's ChunkTarget resolution query use an index scan rather than
-- a full table scan when looking up which tenants are promoted.  Most rows will
-- have dedicated_schema IS NULL, so the index stays small.

-- ---------------------------------------------------------------------------
-- Schema columns
-- ---------------------------------------------------------------------------

ALTER TABLE tenants
    ADD COLUMN dedicated_schema text
        CONSTRAINT tenants_dedicated_schema_format
            CHECK (
                dedicated_schema IS NULL
                OR dedicated_schema ~ '^tenant_[a-z0-9_]{1,63}$'
            ),
    ADD COLUMN promotion_fenced boolean NOT NULL DEFAULT false;

-- ---------------------------------------------------------------------------
-- Supporting index
-- ---------------------------------------------------------------------------

-- Partial index: only promoted tenants appear; keeps the index tiny.
-- Used by: ingestion_service ChunkTarget resolution, promotion_service status.
CREATE INDEX tenants_dedicated_schema_idx
    ON tenants (id)
    WHERE dedicated_schema IS NOT NULL;

-- ---------------------------------------------------------------------------
-- Comments
-- ---------------------------------------------------------------------------

COMMENT ON COLUMN tenants.dedicated_schema IS
    'Schema name for this tenant''s dedicated document_chunks table, e.g. '
    '"tenant_<uuid_no_hyphens>". NULL = tenant uses public.document_chunks. '
    'Value is validated against ^tenant_[a-z0-9_]{1,63}$ before interpolation '
    'into SQL identifiers by the Rust query layer.';

COMMENT ON COLUMN tenants.promotion_fenced IS
    'Write fence toggled during the 5-phase promotion protocol. '
    'TRUE blocks new ingestion writes to public.document_chunks for this tenant '
    'while the delta-sync and atomic schema flip (Phases 2-3) are in progress. '
    'Cleared to FALSE once promotion completes or is rolled back.';
