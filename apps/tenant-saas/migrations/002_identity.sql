-- Migration: 002_identity.sql
-- Purpose: users, tenants, memberships, refresh tokens, plus row-level security.
-- Runs as db_owner. Forward-only (no down migration).
--
-- How tenant isolation works:
--   The app starts every request transaction by setting two settings
--   (see common-database begin_user_tx / begin_tenant_tx):
--     app.current_user_id    who is calling
--     app.current_tenant_id  which tenant they are working in
--   RLS policies compare rows against those settings. If a setting is missing
--   it reads as NULL, the comparison is never true, and no rows are visible.

-- ---------------------------------------------------------------------------
-- Helper functions
-- ---------------------------------------------------------------------------

-- Read the current tenant / user from the transaction settings (NULL if unset).
CREATE FUNCTION app_tenant_id() RETURNS uuid
LANGUAGE sql STABLE
SET search_path = pg_catalog
AS $$ SELECT NULLIF(current_setting('app.current_tenant_id', true), '')::uuid $$;

CREATE FUNCTION app_user_id() RETURNS uuid
LANGUAGE sql STABLE
SET search_path = pg_catalog
AS $$ SELECT NULLIF(current_setting('app.current_user_id', true), '')::uuid $$;

-- Keeps updated_at current on every UPDATE.
CREATE FUNCTION set_updated_at() RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    NEW.updated_at = now();
    RETURN NEW;
END
$$;

-- ---------------------------------------------------------------------------
-- Tables
-- ---------------------------------------------------------------------------

-- Global identity: one row per person, shared across tenants.
CREATE TABLE users (
    id                uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    email             citext      NOT NULL UNIQUE,
    password_hash     text,
    email_verified_at timestamptz,
    status            text        NOT NULL DEFAULT 'active'
                      CHECK (status IN ('active', 'disabled', 'pending')),
    created_at        timestamptz NOT NULL DEFAULT now(),
    updated_at        timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT users_email_len CHECK (char_length(email) BETWEEN 3 AND 254)
);

CREATE TABLE tenants (
    id         uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    name       text        NOT NULL CHECK (char_length(name) BETWEEN 1 AND 100),
    slug       text        NOT NULL UNIQUE
               CHECK (slug ~ '^[a-z0-9](?:[a-z0-9-]{1,48}[a-z0-9])$'),
    plan       text        NOT NULL DEFAULT 'free',
    status     text        NOT NULL DEFAULT 'active'
               CHECK (status IN ('active', 'suspended', 'deleted')),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

-- Which user belongs to which tenant, and with what role.
CREATE TABLE memberships (
    id         uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id  uuid        NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    user_id    uuid        NOT NULL REFERENCES users(id)   ON DELETE CASCADE,
    role       text        NOT NULL CHECK (role   IN ('owner', 'admin', 'member', 'viewer')),
    status     text        NOT NULL CHECK (status IN ('invited', 'active', 'suspended')),
    invited_by uuid        REFERENCES users(id) ON DELETE SET NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT memberships_tenant_user_unique UNIQUE (tenant_id, user_id),
    -- Lets tenant-owned child tables use a composite FK (tenant_id, membership_id).
    CONSTRAINT memberships_tenant_id_id_unique UNIQUE (tenant_id, id)
);
CREATE INDEX memberships_user_idx ON memberships (user_id, status);

-- Refresh tokens belong to a user, not a tenant, so they have no RLS.
-- Only a SHA-256 hash of the random token is stored.
CREATE TABLE refresh_tokens (
    id         uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id    uuid        NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    family_id  uuid        NOT NULL,
    token_hash bytea       NOT NULL UNIQUE,
    expires_at timestamptz NOT NULL,
    rotated_at timestamptz,
    revoked_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX refresh_tokens_family_idx ON refresh_tokens (family_id);
CREATE INDEX refresh_tokens_user_idx   ON refresh_tokens (user_id);

-- ---------------------------------------------------------------------------
-- Triggers
-- ---------------------------------------------------------------------------

CREATE TRIGGER users_set_updated_at BEFORE UPDATE ON users
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();
CREATE TRIGGER tenants_set_updated_at BEFORE UPDATE ON tenants
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();
CREATE TRIGGER memberships_set_updated_at BEFORE UPDATE ON memberships
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

-- A tenant must always keep at least one active owner.
-- Runs as the caller, inside the tenant transaction, so RLS still applies.
CREATE FUNCTION memberships_protect_last_owner() RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    other_owners int;
BEGIN
    IF OLD.role = 'owner' AND OLD.status = 'active'
       AND (TG_OP = 'DELETE' OR NEW.role <> 'owner' OR NEW.status <> 'active') THEN

        -- Lock the owner rows so two concurrent demotions cannot both succeed.
        PERFORM 1 FROM memberships
         WHERE tenant_id = OLD.tenant_id AND role = 'owner' AND status = 'active'
           FOR UPDATE;

        SELECT count(*) INTO other_owners FROM memberships
         WHERE tenant_id = OLD.tenant_id AND role = 'owner' AND status = 'active'
           AND id <> OLD.id;

        IF other_owners = 0 THEN
            -- The app maps P0001 + hint LAST_OWNER to a 409 LAST_OWNER error.
            RAISE EXCEPTION 'last active owner' USING ERRCODE = 'P0001', HINT = 'LAST_OWNER';
        END IF;
    END IF;

    RETURN CASE WHEN TG_OP = 'DELETE' THEN OLD ELSE NEW END;
END
$$;

CREATE TRIGGER memberships_last_owner BEFORE UPDATE OR DELETE ON memberships
    FOR EACH ROW EXECUTE FUNCTION memberships_protect_last_owner();

-- ---------------------------------------------------------------------------
-- Grants (app_runtime gets only what it needs)
-- ---------------------------------------------------------------------------

REVOKE ALL ON users, tenants, memberships, refresh_tokens FROM PUBLIC;

-- users: column-level, so password_hash can never be selected directly.
-- Login reads it through auth_get_credentials() below.
GRANT SELECT (id, email, status, email_verified_at, created_at, updated_at)
    ON users TO app_runtime;
GRANT INSERT (id, email, password_hash, status) ON users TO app_runtime;
GRANT UPDATE (status, updated_at) ON users TO app_runtime;

GRANT SELECT, INSERT, UPDATE ON tenants TO app_runtime;
GRANT SELECT, INSERT, UPDATE, DELETE ON memberships TO app_runtime;
GRANT SELECT, INSERT, UPDATE ON refresh_tokens TO app_runtime;

GRANT EXECUTE ON FUNCTION app_tenant_id(), app_user_id() TO app_runtime;

-- ---------------------------------------------------------------------------
-- Row-level security
-- ---------------------------------------------------------------------------
-- (SELECT app_tenant_id()) lets Postgres evaluate the setting once per query.

-- memberships: visible inside the current tenant, or when they are your own.
-- Writes only inside the current tenant. FORCE applies the policies to the
-- table owner too. The policies have no "TO <role>" on purpose, so they also
-- apply to db_owner, which owns the definer functions further down.
ALTER TABLE memberships ENABLE ROW LEVEL SECURITY;
ALTER TABLE memberships FORCE ROW LEVEL SECURITY;

CREATE POLICY memberships_select ON memberships FOR SELECT
    USING (tenant_id = (SELECT app_tenant_id()) OR user_id = (SELECT app_user_id()));
CREATE POLICY memberships_insert ON memberships FOR INSERT
    WITH CHECK (tenant_id = (SELECT app_tenant_id()));
CREATE POLICY memberships_update ON memberships FOR UPDATE
    USING (tenant_id = (SELECT app_tenant_id()))
    WITH CHECK (tenant_id = (SELECT app_tenant_id()));
CREATE POLICY memberships_delete ON memberships FOR DELETE
    USING (tenant_id = (SELECT app_tenant_id()));

-- tenants: you see the tenant you are working in, plus every tenant where you
-- are an active member (so "list my tenants" works before choosing one).
-- The policies on memberships never mention tenants, so there is no recursion.
ALTER TABLE tenants ENABLE ROW LEVEL SECURITY;
ALTER TABLE tenants FORCE ROW LEVEL SECURITY;

CREATE POLICY tenants_select ON tenants FOR SELECT
    USING (
        id = (SELECT app_tenant_id())
        OR EXISTS (
            SELECT 1 FROM memberships m
             WHERE m.tenant_id = tenants.id
               AND m.user_id = (SELECT app_user_id())
               AND m.status = 'active'
        )
    );
CREATE POLICY tenants_insert ON tenants FOR INSERT
    WITH CHECK (id = (SELECT app_tenant_id()));
CREATE POLICY tenants_update ON tenants FOR UPDATE
    USING (id = (SELECT app_tenant_id()))
    WITH CHECK (id = (SELECT app_tenant_id()));

-- users: a caller can only see and update their own row. Registration may
-- insert freely. No FORCE: the table owner (db_owner) is exempt, which is what
-- lets the definer functions below look up other users safely.
ALTER TABLE users ENABLE ROW LEVEL SECURITY;

CREATE POLICY users_select ON users FOR SELECT TO app_runtime
    USING (id = (SELECT app_user_id()));
CREATE POLICY users_update ON users FOR UPDATE TO app_runtime
    USING (id = (SELECT app_user_id()))
    WITH CHECK (id = (SELECT app_user_id()));
CREATE POLICY users_insert ON users FOR INSERT TO app_runtime
    WITH CHECK (true);

-- ---------------------------------------------------------------------------
-- Narrow SECURITY DEFINER functions
-- These run as db_owner and are the ONLY way app_runtime can read across users.
-- ---------------------------------------------------------------------------

-- Login: the only path to password_hash.
CREATE FUNCTION auth_get_credentials(p_email citext)
RETURNS TABLE (id uuid, password_hash text, status text)
LANGUAGE sql STABLE SECURITY DEFINER
SET search_path = pg_catalog, public
AS $$
    SELECT u.id, u.password_hash, u.status FROM users u WHERE u.email = p_email
$$;

-- Member list with emails. Takes no tenant argument: it always uses the
-- current tenant, so it cannot be pointed at another one.
CREATE FUNCTION tenant_member_directory()
RETURNS TABLE (user_id uuid, email citext, role text, status text, created_at timestamptz)
LANGUAGE sql STABLE SECURITY DEFINER
SET search_path = pg_catalog, public
AS $$
    SELECT m.user_id, u.email, m.role, m.status, m.created_at
      FROM memberships m
      JOIN users u ON u.id = m.user_id
     WHERE m.tenant_id = app_tenant_id()
     ORDER BY m.created_at
$$;

-- Invitations: email -> user id (NULL if not registered). Needs a tenant context.
CREATE FUNCTION find_user_id_by_email(p_email citext)
RETURNS uuid
LANGUAGE plpgsql STABLE SECURITY DEFINER
SET search_path = pg_catalog, public
AS $$
BEGIN
    IF app_tenant_id() IS NULL THEN
        RAISE EXCEPTION 'tenant context required' USING ERRCODE = '42501';
    END IF;
    RETURN (SELECT u.id FROM users u WHERE u.email = p_email);
END
$$;

REVOKE EXECUTE ON FUNCTION auth_get_credentials(citext),
                           tenant_member_directory(),
                           find_user_id_by_email(citext) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION auth_get_credentials(citext),
                          tenant_member_directory(),
                          find_user_id_by_email(citext) TO app_runtime;
