//! Tenant CRUD repository.
//!
//! All mutating functions run inside a transaction whose GUCs (`app.current_user_id`
//! and/or `app.current_tenant_id`) are already set by `begin_user_tx` /
//! `begin_tenant_tx`. RLS enforces the tenant boundary — no extra filtering needed.

use chrono::{DateTime, Utc};
use common_database::Tx;
use common_errors::AppError;
use sqlx::FromRow;
use uuid::Uuid;

use super::db_error::map_db_error;

// ── Row types ────────────────────────────────────────────────────────────────

/// Full tenant record returned from insert / get / patch.
#[derive(Debug, FromRow)]
pub struct TenantRow {
    pub id: Uuid,
    pub name: String,
    pub slug: String,
    pub plan: String,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Lightweight row used in `GET /tenants` — includes the caller's role.
#[derive(Debug, FromRow)]
pub struct UserTenantRow {
    pub tenant_id: Uuid,
    pub name: String,
    pub slug: String,
    pub plan: String,
    pub tenant_status: String,
    pub role: String,
    pub membership_status: String,
}

/// Partial update. Only `Some` fields are written.
#[derive(Debug, Default)]
pub struct TenantPatch {
    pub name: Option<String>,
    pub slug: Option<String>,
    pub plan: Option<String>,
    pub status: Option<String>,
}

// ── Write operations (user-scoped tx) ────────────────────────────────────────

/// Inserts a new tenant row and returns it.
///
/// The transaction must be user-scoped (`begin_user_tx(user_id)`) with
/// `app.current_tenant_id` also set to `id` before calling this, so the
/// `tenants` RLS INSERT policy (`id = app_tenant_id()`) passes and `RETURNING`
/// gives back the row. The caller (`tenant_service::create_tenant`) sets both
/// GUCs in one transaction for the atomic create + first-owner insert.
///
/// Unique violation on `tenants_slug_key` → `AppError::Conflict { code: "SLUG_TAKEN" }`.
pub async fn insert(tx: &mut Tx, id: Uuid, name: &str, slug: &str) -> Result<TenantRow, AppError> {
    sqlx::query_as::<_, TenantRow>(
        "INSERT INTO tenants (id, name, slug)
         VALUES ($1, $2, $3)
         RETURNING id, name, slug, plan, status, created_at, updated_at",
    )
    .bind(id)
    .bind(name)
    .bind(slug)
    .fetch_one(&mut **tx)
    .await
    .map_err(map_db_error)
}

/// Counts tenants where the user is an *active* owner, to enforce the per-user
/// tenant-creation cap. Runs on a user-scoped transaction.
pub async fn count_owned(tx: &mut Tx, user_id: Uuid) -> Result<i64, AppError> {
    Ok(sqlx::query_scalar::<_, i64>(
        "SELECT count(*)
           FROM memberships m
           JOIN tenants t ON t.id = m.tenant_id
          WHERE m.user_id = $1
            AND m.role    = 'owner'
            AND m.status  = 'active'
            AND t.status  = 'active'",
    )
    .bind(user_id)
    .fetch_one(&mut **tx)
    .await?)
}

// ── Read operations (user-scoped tx) ─────────────────────────────────────────

/// Lists every active tenant the caller belongs to, ordered by name then id.
///
/// Uses cursor-based pagination: pass the `id` of the last row from the
/// previous page as `after` to get the next page. `limit` is capped by the
/// caller (default 50, max 100).
///
/// The query joins through `memberships` so RLS only needs `app.current_user_id`
/// (the `memberships` U-branch policy).
pub async fn list_for_user(
    tx: &mut Tx,
    user_id: Uuid,
    after: Option<Uuid>,
    limit: i64,
) -> Result<Vec<UserTenantRow>, AppError> {
    // Cursor pagination: (name, id) is a stable, index-friendly sort key.
    // When `after` is None we use a sentinel that sorts before every real row.
    Ok(sqlx::query_as::<_, UserTenantRow>(
        "SELECT t.id    AS tenant_id,
                t.name,
                t.slug,
                t.plan,
                t.status AS tenant_status,
                m.role,
                m.status AS membership_status
           FROM memberships m
           JOIN tenants t ON t.id = m.tenant_id
          WHERE m.user_id = $1
            AND m.status  = 'active'
            AND t.status  = 'active'
            AND (
                  $2::uuid IS NULL
                  OR (t.name, t.id) > (
                       SELECT name, id FROM tenants WHERE id = $2
                     )
                )
          ORDER BY t.name, t.id
          LIMIT $3",
    )
    .bind(user_id)
    .bind(after)
    .bind(limit)
    .fetch_all(&mut **tx)
    .await?)
}

// ── Read operations (tenant-scoped tx) ───────────────────────────────────────

/// Fetches a single tenant by id.
///
/// RLS enforces access — returns `NotFound` for non-members exactly like a
/// non-existent tenant (concealment per design D7).
pub async fn get_by_id(tx: &mut Tx, tenant_id: Uuid) -> Result<TenantRow, AppError> {
    sqlx::query_as::<_, TenantRow>(
        "SELECT id, name, slug, plan, status, created_at, updated_at
           FROM tenants
          WHERE id = $1",
    )
    .bind(tenant_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(AppError::NotFound)
}

// ── Write operations (tenant-scoped tx) ──────────────────────────────────────

/// Applies a partial update to a tenant and returns the updated row.
///
/// Only the `Some` fields in `patch` are changed; all others keep their
/// current DB values via `COALESCE`. An empty patch is a no-op that still
/// returns the current row.
///
/// Unique violation on `tenants_slug_key` → `AppError::Conflict { code: "SLUG_TAKEN" }`.
pub async fn update(
    tx: &mut Tx,
    tenant_id: Uuid,
    patch: TenantPatch,
) -> Result<TenantRow, AppError> {
    sqlx::query_as::<_, TenantRow>(
        "UPDATE tenants
            SET name   = COALESCE($2, name),
                slug   = COALESCE($3, slug),
                plan   = COALESCE($4, plan),
                status = COALESCE($5, status)
          WHERE id = $1
          RETURNING id, name, slug, plan, status, created_at, updated_at",
    )
    .bind(tenant_id)
    .bind(patch.name)
    .bind(patch.slug)
    .bind(patch.plan)
    .bind(patch.status)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(AppError::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tenant_patch_default_is_all_none() {
        let patch = TenantPatch::default();
        assert!(patch.name.is_none());
        assert!(patch.slug.is_none());
        assert!(patch.plan.is_none());
        assert!(patch.status.is_none());
    }

    #[test]
    fn tenant_patch_partial_construction() {
        let patch = TenantPatch {
            name: Some("Acme".into()),
            ..Default::default()
        };
        assert_eq!(patch.name.as_deref(), Some("Acme"));
        assert!(patch.slug.is_none());
    }
}
