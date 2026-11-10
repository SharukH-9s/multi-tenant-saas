-- Runs once, on first initialisation of the data volume.
-- POSTGRES_USER (db_owner) owns the schema; app_runtime is the runtime account.
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'app_runtime') THEN
        CREATE ROLE app_runtime LOGIN PASSWORD 'runtime_password';
    END IF;
END
$$;

-- app_runtime is NOT a superuser, schema owner, or BYPASSRLS holder.
-- Table-level GRANT statements are added in each migration that creates a table.
