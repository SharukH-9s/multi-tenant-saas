-- Migration: 001_foundation.sql
-- Purpose: Extensions and database roles only. Domain tables are added per phase.

-- Extensions
CREATE EXTENSION IF NOT EXISTS "uuid-ossp";
CREATE EXTENSION IF NOT EXISTS "citext";
-- pgvector is added in the Phase 5 migration.

-- Database roles (design doc 7.5)
--   db_owner    : runs migrations and owns schema objects. Never used by the running app.
--   app_runtime : application role. Not a superuser, owns nothing, no BYPASSRLS, no CREATEROLE.
-- Table grants for app_runtime are added in the migration that creates each table.
-- Dev-only passwords; they must match .env.example and infra/postgres/init/00_roles.sql.
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'db_owner') THEN
        CREATE ROLE db_owner LOGIN PASSWORD 'owner_password' CREATEDB;
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'app_runtime') THEN
        CREATE ROLE app_runtime LOGIN PASSWORD 'runtime_password';
    END IF;
END
$$;

-- Custom GUC app.current_tenant_id needs no DDL. Used from Phase 2 with RLS:
--   SET LOCAL app.current_tenant_id = '<uuid>';
