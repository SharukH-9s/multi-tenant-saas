-- Migration: 008_tighten_dedicated_schema_length.sql
-- Purpose: Tightens dedicated_schema length to 63 bytes total (Phase 5, Option B.1).
-- Runs as db_owner. Forward-only (no down migration).
--
-- Background:
-- PostgreSQL's identifier limit (NAMEDATALEN - 1) is 63 bytes total.
-- The prefix "tenant_" is 7 bytes, leaving at most 56 bytes for the suffix.
-- Migration 007 allowed up to 63 chars for the suffix (total 70 bytes),
-- which would be silently truncated by PostgreSQL to 63 bytes.
-- This migration updates the CHECK constraint to enforce ^tenant_[a-z0-9_]{1,56}$
-- so that the full schema name never exceeds 63 bytes.

-- Verify that no existing row violates the tighter invariant before applying
DO $$
BEGIN
  IF EXISTS (
    SELECT 1 FROM tenants
     WHERE dedicated_schema IS NOT NULL
       AND dedicated_schema !~ '^tenant_[a-z0-9_]{1,56}$'
  ) THEN
    RAISE EXCEPTION 'Cannot tighten dedicated_schema format: existing rows violate ^tenant_[a-z0-9_]{1,56}$';
  END IF;
END $$;

ALTER TABLE tenants
    DROP CONSTRAINT IF EXISTS tenants_dedicated_schema_format,
    ADD CONSTRAINT tenants_dedicated_schema_format
        CHECK (
            dedicated_schema IS NULL
            OR dedicated_schema ~ '^tenant_[a-z0-9_]{1,56}$'
        );

COMMENT ON COLUMN tenants.dedicated_schema IS
    'Schema name for this tenant''s dedicated document_chunks table, e.g. '
    '"tenant_<uuid_no_hyphens>". NULL = tenant uses public.document_chunks. '
    'Value is validated against ^tenant_[a-z0-9_]{1,56}$ (total identifier <= 63 bytes) '
    'before interpolation into SQL identifiers by the Rust query layer.';
