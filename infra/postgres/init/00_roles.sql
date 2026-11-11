-- Runs once, on first initialisation of the data volume.
-- Executed by the bootstrap superuser ('postgres').

-- 1. Create migration role (owns schema, but is NOT a superuser and does NOT bypass RLS)
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'db_owner') THEN
        CREATE ROLE db_owner LOGIN PASSWORD 'owner_password' CREATEDB;
    END IF;
END
$$;

ALTER ROLE db_owner NOSUPERUSER NOBYPASSRLS NOCREATEROLE;

-- 2. Create application runtime role (non-superuser, no DDL, strictly bound by RLS)
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'app_runtime') THEN
        CREATE ROLE app_runtime LOGIN PASSWORD 'runtime_password';
    END IF;
END
$$;

ALTER ROLE app_runtime NOSUPERUSER NOBYPASSRLS NOCREATEROLE;

-- 3. Dynamic database grants for the current target database
DO $$
BEGIN
    EXECUTE format('GRANT ALL ON DATABASE %I TO db_owner', current_database());
    EXECUTE format('GRANT CONNECT ON DATABASE %I TO app_runtime', current_database());
END
$$;

-- 4. Schema grants
ALTER SCHEMA public OWNER TO db_owner;
GRANT ALL ON SCHEMA public TO db_owner;
GRANT USAGE ON SCHEMA public TO app_runtime;

