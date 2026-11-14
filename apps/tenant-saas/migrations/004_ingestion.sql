-- Migration: 004_ingestion.sql
-- Purpose: ingestion_jobs, outbox_events and document_chunks tables, indexes,
--          triggers, grants, RLS, and a backfill for Phase 3 documents.
-- Runs as db_owner. Forward-only (no down migration).
--
-- Design: ADR-007 and phase4_implementation_plan.md (D2, D4, D8, D9, D10).
-- The cross-tenant sweeper function (sweep_ingestion_jobs) lives in 005.

-- ---------------------------------------------------------------------------
-- Extension
-- ---------------------------------------------------------------------------
-- No-op when infra/postgres/init/00_roles.sql already installed it (fresh
-- volumes and CI). Existing local volumes do not re-run init scripts: run this
-- line once as the `postgres` superuser, because pgvector may not be marked
-- `trusted` and db_owner is not a superuser.
CREATE EXTENSION IF NOT EXISTS vector;

-- ---------------------------------------------------------------------------
-- ingestion_jobs
--
-- One row per processing attempt chain for a document. History is kept: a
-- manual retry inserts a new row and leaves the old `failed` row in place.
-- ---------------------------------------------------------------------------

CREATE TABLE ingestion_jobs (
    id               uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id        uuid        NOT NULL,
    document_id      uuid        NOT NULL,
    status           text        NOT NULL DEFAULT 'pending' CHECK (status IN (
                         'pending', 'processing', 'retry_scheduled',
                         'succeeded', 'failed', 'cancelled')),
    attempts         integer     NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    max_attempts     integer     NOT NULL CHECK (max_attempts BETWEEN 1 AND 100),
    available_at     timestamptz NOT NULL DEFAULT now(),
    last_enqueued_at timestamptz NOT NULL DEFAULT now(),
    lease_token      uuid,
    locked_by        text        CHECK (char_length(locked_by) <= 200),
    locked_until     timestamptz,
    last_error_code  text        CHECK (last_error_code IS NULL
                         OR last_error_code ~ '^[A-Z][A-Z0-9_]{2,63}$'),
    last_error_at    timestamptz,
    created_at       timestamptz NOT NULL DEFAULT now(),
    updated_at       timestamptz NOT NULL DEFAULT now(),
    completed_at     timestamptz,
    CONSTRAINT ingestion_jobs_tenant_id_id_unique UNIQUE (tenant_id, id),
    CONSTRAINT ingestion_jobs_document_fk FOREIGN KEY (tenant_id, document_id)
        REFERENCES documents (tenant_id, id) ON DELETE CASCADE,
    -- A lease exists exactly while the job is processing.
    CONSTRAINT ingestion_jobs_lease_consistency CHECK (
        (status = 'processing') = (lease_token IS NOT NULL AND locked_until IS NOT NULL))
);

-- At most one active job per document. Manual retry and duplicate enqueue rely
-- on this. The predicate is repeated verbatim in any ON CONFLICT clause.
CREATE UNIQUE INDEX ingestion_jobs_one_active
    ON ingestion_jobs (tenant_id, document_id)
    WHERE status IN ('pending', 'processing', 'retry_scheduled');

-- Per-document history, newest first (status endpoint, retry endpoint).
CREATE INDEX ingestion_jobs_document_idx
    ON ingestion_jobs (tenant_id, document_id, created_at DESC);

-- Sweeper probes, one partial index per rule.
CREATE INDEX ingestion_jobs_lease_idx   ON ingestion_jobs (locked_until)     WHERE status = 'processing';
CREATE INDEX ingestion_jobs_retry_idx   ON ingestion_jobs (available_at)     WHERE status = 'retry_scheduled';
CREATE INDEX ingestion_jobs_pending_idx ON ingestion_jobs (last_enqueued_at) WHERE status = 'pending';

-- ---------------------------------------------------------------------------
-- outbox_events
--
-- No RLS (ADR-007 §3): the publisher runs with no tenant context and must see
-- every row. The table holds opaque identifiers only, never document content,
-- and no endpoint exposes it. Same reasoning as storage_orphans (ADR-006).
-- ---------------------------------------------------------------------------

CREATE TABLE outbox_events (
    id               uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id        uuid,
    aggregate_type   text        NOT NULL CHECK (char_length(aggregate_type) BETWEEN 1 AND 64),
    aggregate_id     uuid        NOT NULL,
    event_type       text        NOT NULL CHECK (char_length(event_type) BETWEEN 1 AND 128),
    payload          jsonb       NOT NULL CHECK (jsonb_typeof(payload) = 'object'),
    created_at       timestamptz NOT NULL DEFAULT now(),
    claimed_until    timestamptz,
    publish_attempts integer     NOT NULL DEFAULT 0 CHECK (publish_attempts >= 0),
    published_at     timestamptz,
    last_error       text        CHECK (char_length(last_error) <= 500),
    dead_lettered_at timestamptz
);

CREATE INDEX outbox_unpublished_idx ON outbox_events (created_at)
    WHERE published_at IS NULL AND dead_lettered_at IS NULL;
-- Sweeper "already announced?" check.
CREATE INDEX outbox_unpublished_aggregate_idx ON outbox_events (aggregate_id)
    WHERE published_at IS NULL AND dead_lettered_at IS NULL;
-- Retention purge of published rows.
CREATE INDEX outbox_published_idx ON outbox_events (published_at)
    WHERE published_at IS NOT NULL;

-- ---------------------------------------------------------------------------
-- document_chunks
--
-- Immutable: a re-ingest deletes all chunks of the document and inserts the
-- new set in one transaction (ADR-007 §5, D8, D10). The tsvector column and
-- the HNSW index arrive with Phase 5 (ADR-007 §6).
-- ---------------------------------------------------------------------------

CREATE TABLE document_chunks (
    id                uuid         PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id         uuid         NOT NULL,
    document_id       uuid         NOT NULL,
    chunk_index       integer      NOT NULL CHECK (chunk_index >= 0),
    content           text         NOT NULL CHECK (char_length(content) > 0),
    token_count       integer      NOT NULL CHECK (token_count > 0),
    embedding         vector(1536) NOT NULL,
    embedding_model   text         NOT NULL CHECK (char_length(embedding_model) BETWEEN 1 AND 200),
    embedding_version text         NOT NULL CHECK (char_length(embedding_version) BETWEEN 1 AND 64),
    metadata          jsonb        NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(metadata) = 'object'),
    created_at        timestamptz  NOT NULL DEFAULT now(),
    -- The unique index has (tenant_id, document_id) as its leading columns, so
    -- it also serves "all chunks of a document"; no separate index is needed.
    CONSTRAINT document_chunks_position_unique UNIQUE (tenant_id, document_id, chunk_index),
    CONSTRAINT document_chunks_document_fk FOREIGN KEY (tenant_id, document_id)
        REFERENCES documents (tenant_id, id) ON DELETE CASCADE
);

-- ---------------------------------------------------------------------------
-- Triggers
-- ---------------------------------------------------------------------------

CREATE TRIGGER ingestion_jobs_set_updated_at BEFORE UPDATE ON ingestion_jobs
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

-- ---------------------------------------------------------------------------
-- Grants (app_runtime gets only what each access pattern needs)
-- ---------------------------------------------------------------------------

REVOKE ALL ON ingestion_jobs, outbox_events, document_chunks FROM PUBLIC;

-- History is kept, so no DELETE: cancellation is a status change.
GRANT SELECT, INSERT, UPDATE         ON ingestion_jobs  TO app_runtime;
-- Immutable rows: replace = delete + insert. No UPDATE.
GRANT SELECT, INSERT, DELETE         ON document_chunks TO app_runtime;
-- DELETE is for the retention purge of published rows.
GRANT SELECT, INSERT, UPDATE, DELETE ON outbox_events   TO app_runtime;

-- ---------------------------------------------------------------------------
-- Row-level security for ingestion_jobs and document_chunks
-- ---------------------------------------------------------------------------
-- Copied from documents (003): tenant_id = app_tenant_id(), four policies.
--
-- ENABLE only, NO FORCE (plan decision D2). The sweeper added in 005 is a
-- SECURITY DEFINER function owned by db_owner, and it sees every tenant's
-- rows only because the table owner is exempt from RLS. app_runtime is never
-- the owner and stays fully bound. Do NOT add FORCE ROW LEVEL SECURITY here
-- or in a later "tidy-up": CI asserts relforcerowsecurity is false.
--
-- outbox_events has no RLS (ADR-007 §3), see above.

ALTER TABLE ingestion_jobs ENABLE ROW LEVEL SECURITY;

CREATE POLICY ingestion_jobs_select ON ingestion_jobs FOR SELECT
    USING (tenant_id = (SELECT app_tenant_id()));
CREATE POLICY ingestion_jobs_insert ON ingestion_jobs FOR INSERT
    WITH CHECK (tenant_id = (SELECT app_tenant_id()));
CREATE POLICY ingestion_jobs_update ON ingestion_jobs FOR UPDATE
    USING (tenant_id = (SELECT app_tenant_id()))
    WITH CHECK (tenant_id = (SELECT app_tenant_id()));
CREATE POLICY ingestion_jobs_delete ON ingestion_jobs FOR DELETE
    USING (tenant_id = (SELECT app_tenant_id()));

ALTER TABLE document_chunks ENABLE ROW LEVEL SECURITY;

CREATE POLICY document_chunks_select ON document_chunks FOR SELECT
    USING (tenant_id = (SELECT app_tenant_id()));
CREATE POLICY document_chunks_insert ON document_chunks FOR INSERT
    WITH CHECK (tenant_id = (SELECT app_tenant_id()));
CREATE POLICY document_chunks_update ON document_chunks FOR UPDATE
    USING (tenant_id = (SELECT app_tenant_id()))
    WITH CHECK (tenant_id = (SELECT app_tenant_id()));
CREATE POLICY document_chunks_delete ON document_chunks FOR DELETE
    USING (tenant_id = (SELECT app_tenant_id()));

-- ---------------------------------------------------------------------------
-- Backfill
-- ---------------------------------------------------------------------------
-- Documents uploaded during Phase 3 sit in `pending` with no job. This runs as
-- the table owner, which is exempt from RLS, so it sees every tenant.
-- `5` is a literal because SQL cannot read JOB_MAX_ATTEMPTS; keep it in
-- step with the default of that setting.

WITH new_jobs AS (
    INSERT INTO ingestion_jobs (tenant_id, document_id, max_attempts)
    SELECT d.tenant_id, d.id, 5
      FROM documents d
     WHERE d.status = 'pending' AND d.deleted_at IS NULL
    RETURNING id, tenant_id, document_id
)
INSERT INTO outbox_events (tenant_id, aggregate_type, aggregate_id, event_type, payload)
SELECT tenant_id, 'ingestion_job', id, 'ingestion.requested',
       jsonb_build_object('v', 1, 'job_id', id, 'tenant_id', tenant_id, 'document_id', document_id)
  FROM new_jobs;
