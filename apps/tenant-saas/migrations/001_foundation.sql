-- Migration: 001_foundation.sql
-- Purpose: Extensions and GUC documentation.
-- Note: Roles (db_owner and app_runtime) are provisioned during cluster bootstrap
-- (via infra/postgres/init/00_roles.sql) by the bootstrap superuser ('postgres').
-- Table grants for app_runtime are added in the migration that creates each table.

-- Extensions
CREATE EXTENSION IF NOT EXISTS "uuid-ossp";
CREATE EXTENSION IF NOT EXISTS "citext";
-- pgvector is added in the Phase 5 migration.

-- Custom GUC app.current_tenant_id needs no DDL. Used from Phase 2 with RLS:
--   SET LOCAL app.current_tenant_id = '<uuid>';
