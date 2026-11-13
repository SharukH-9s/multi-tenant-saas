-- Migration: 003_documents.sql
-- Purpose: documents table, storage_orphans table, indexes, grants, and RLS.
-- Runs as db_owner. Forward-only (no down migration).
--
-- Phase 3 only writes status values: pending, deleting, deleted.
-- The CHECK already lists all lifecycle values from §10.4 so that widening
-- it later does not require dropping and re-adding the constraint on a
-- growing table.

-- ---------------------------------------------------------------------------
-- Tables
-- ---------------------------------------------------------------------------

CREATE TABLE documents (
    id                uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id         uuid        NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    uploaded_by       uuid        REFERENCES users(id) ON DELETE SET NULL,
    title             text        NOT NULL CHECK (char_length(title) BETWEEN 1 AND 200),
    original_filename text        NOT NULL CHECK (char_length(original_filename) BETWEEN 1 AND 255),
    content_hash      char(64)    NOT NULL CHECK (content_hash ~ '^[0-9a-f]{64}$'),
    object_key        text        NOT NULL CHECK (char_length(object_key) BETWEEN 1 AND 1024),
    mime_type         text        NOT NULL CHECK (mime_type IN (
                          'application/pdf', 'text/plain', 'text/markdown')),
    size_bytes        bigint      NOT NULL CHECK (size_bytes > 0),
    status            text        NOT NULL DEFAULT 'pending' CHECK (status IN (
                          'pending', 'processing', 'ready',
                          'retry_scheduled', 'failed', 'deleting', 'deleted')),
    failure_code      text        CHECK (failure_code IS NULL
                          OR failure_code ~ '^[A-Z][A-Z0-9_]{2,63}$'),
    failure_detail    text        CHECK (char_length(failure_detail) <= 2000),
    chunk_count       integer     NOT NULL DEFAULT 0 CHECK (chunk_count >= 0),
    created_at        timestamptz NOT NULL DEFAULT now(),
    updated_at        timestamptz NOT NULL DEFAULT now(),
    deleted_at        timestamptz,
    -- Composite unique for Phase 4 child tables (document_chunks, ingestion_jobs)
    -- that need a composite FK: (tenant_id, document_id).
    CONSTRAINT documents_tenant_id_id_unique UNIQUE (tenant_id, id),
    -- Soft-delete invariant: a row in deleting/deleted must carry a timestamp,
    -- and a live row must not.
    CONSTRAINT documents_deleted_consistency CHECK (
        (status IN ('deleting', 'deleted')) = (deleted_at IS NOT NULL))
);

-- Deduplication index: one live copy of each content hash per tenant.
-- The predicate must be repeated verbatim in ON CONFLICT or inference fails
-- with runtime error 42P10.
CREATE UNIQUE INDEX documents_tenant_content_hash_uniq
    ON documents (tenant_id, content_hash) WHERE deleted_at IS NULL;

-- List endpoint: newest-first keyset pagination with a total order.
-- id DESC breaks ties when created_at is identical.
CREATE INDEX documents_tenant_created_idx
    ON documents (tenant_id, created_at DESC, id DESC) WHERE deleted_at IS NULL;

-- Filter by status within a tenant (used by list and the Phase 4 worker).
CREATE INDEX documents_tenant_status_idx
    ON documents (tenant_id, status) WHERE deleted_at IS NULL;

-- Find documents uploaded by a specific user within a tenant.
CREATE INDEX documents_uploader_idx
    ON documents (tenant_id, uploaded_by) WHERE deleted_at IS NULL;

-- Reconciler: find documents stuck in deleting/deleted for cleanup.
CREATE INDEX documents_cleanup_idx
    ON documents (status, deleted_at) WHERE deleted_at IS NOT NULL;

-- ---------------------------------------------------------------------------
-- storage_orphans: objects that need cleanup by the reconciler.
--
-- No RLS on this table: the reconciler runs with no tenant context and must
-- see every row. The table holds only opaque keys and timestamps, never
-- document content, and no endpoint exposes it. This follows the same
-- reasoning as refresh_tokens in 002 (identity data that is not tenant data).
-- See ADR-006 for the full rationale.
-- ---------------------------------------------------------------------------

CREATE TABLE storage_orphans (
    object_key      text        PRIMARY KEY,
    tenant_id       uuid,
    reason          text        NOT NULL CHECK (reason IN (
                        'insert_failed', 'duplicate_cleanup_failed', 'delete_failed')),
    attempts        integer     NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    last_error      text        CHECK (char_length(last_error) <= 500),
    next_attempt_at timestamptz NOT NULL DEFAULT now(),
    created_at      timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX storage_orphans_due_idx ON storage_orphans (next_attempt_at);

-- ---------------------------------------------------------------------------
-- Triggers
-- ---------------------------------------------------------------------------

CREATE TRIGGER documents_set_updated_at BEFORE UPDATE ON documents
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

-- ---------------------------------------------------------------------------
-- Grants (app_runtime gets only what it needs)
-- ---------------------------------------------------------------------------

REVOKE ALL ON documents, storage_orphans FROM PUBLIC;

-- No DELETE on documents: soft-delete only. Withholding the grant means a
-- coding mistake cannot hard-delete a row. Phase 4 adds the grant if chunk
-- cleanup needs it, or keeps the hard delete in a definer function.
GRANT SELECT, INSERT, UPDATE ON documents TO app_runtime;

-- storage_orphans: DELETE is needed so the reconciler can remove cleared rows.
GRANT SELECT, INSERT, UPDATE, DELETE ON storage_orphans TO app_runtime;

-- ---------------------------------------------------------------------------
-- Row-level security for documents
-- ---------------------------------------------------------------------------
-- Copied from the Phase 2 template (memberships) without change.
-- No "OR user_id = app_user_id()" escape: documents belong to a tenant,
-- not to a person, so there is no "see your own across tenants" case.
-- The DELETE policy exists even though no DELETE grant does, so that a
-- future grant does not silently arrive without a policy.
--
-- No FORCE: the table owner (db_owner) is exempt, matching the `users` table
-- pattern from 002_identity.sql. This lets the reconciler and maintenance
-- sweeps inspect documents across tenants while app_runtime remains strictly
-- bound by RLS.

ALTER TABLE documents ENABLE ROW LEVEL SECURITY;

CREATE POLICY documents_select ON documents FOR SELECT
    USING (tenant_id = (SELECT app_tenant_id()));
CREATE POLICY documents_insert ON documents FOR INSERT
    WITH CHECK (tenant_id = (SELECT app_tenant_id()));
CREATE POLICY documents_update ON documents FOR UPDATE
    USING (tenant_id = (SELECT app_tenant_id()))
    WITH CHECK (tenant_id = (SELECT app_tenant_id()));
CREATE POLICY documents_delete ON documents FOR DELETE
    USING (tenant_id = (SELECT app_tenant_id()));

