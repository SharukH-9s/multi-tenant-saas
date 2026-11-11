//! Membership repository.
//!
//! Split by transaction scope:
//!
//! **User-scoped** (`begin_user_tx`): `load_access`, `insert_owner`,
//! `list_invited_for_user`, `accept_invitation`.
//!
//! **Tenant-scoped** (`begin_tenant_tx`): `list_members`, `find_member`,
//! `upsert_invited`, `update_member`, `delete_member`.
//!
//! All strings for `role` and `status` match the DB check constraints exactly
//! (`'owner'|'admin'|'member'|'viewer'` and `'invited'|'active'|'suspended'`).
//! The service layer converts to/from `TenantRole`.

use chrono::{DateTime, Utc};
use common_database::Tx;
use common_errors::AppError;
use sqlx::FromRow;
use uuid::Uuid;

use super::db_error::map_db_error;

// ── Row types ────────────────────────────────────────────────────────────────

/// Everything needed to decide whether a user may act inside a tenant.
///
/// The membership and tenant columns are `None` when there is no such
/// membership, or when RLS hides the tenant (see `load_access`).
#[derive(Debug, FromRow)]
pub struct AccessRow {
    pub user_status: String,
    pub role: Option<String>,
    pub membership_status: Option<String>,
    pub tenant_status: Option<String>,
}

/// One row from `GET /invitations` — pending invitations for the caller.
#[derive(Debug, FromRow)]
pub struct InvitationRow {
    pub tenant_id: Uuid,
    pub tenant_name: String,
    pub tenant_slug: String,
    pub role: String,
    pub invited_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

/// One row from `GET /tenants/:id/members` via the `tenant_member_directory()`
/// definer function. Includes email (safe: only active members of the tenant
/// can query the function; it reads `app.current_tenant_id` internally).
#[derive(Debug, FromRow)]
pub struct MemberRow {
    pub user_id: Uuid,
    pub email: String,
    pub role: String,
    pub status: String,
    pub joined_at: DateTime<Utc>,
}

/// Single membership row used for update / delete guard checks.
#[derive(Debug, FromRow)]
pub struct MembershipRow {
    pub id: Uuid,
    pub user_id: Uuid,
    pub role: String,
    pub status: String,
    pub updated_at: DateTime<Utc>,
}

// ── User-scoped queries ───────────────────────────────────────────────────────

/// Loads the caller's user status plus their membership in `tenant_id`.
///
/// Runs on a user-scoped transaction (`begin_user_tx`). It starts from `users`
/// and uses LEFT JOINs on purpose: the `tenants` policy only shows a tenant to
/// users with an *active* membership, so an inner join would make invited and
/// suspended members look like strangers (404 instead of 403).
///
/// Returns `None` only when the user row itself does not exist.
pub async fn load_access(
    tx: &mut Tx,
    tenant_id: Uuid,
    user_id: Uuid,
) -> Result<Option<AccessRow>, AppError> {
    Ok(sqlx::query_as::<_, AccessRow>(
        "SELECT u.status AS user_status,
                m.role,
                m.status AS membership_status,
                t.status AS tenant_status
           FROM users u
           LEFT JOIN memberships m ON m.user_id = u.id AND m.tenant_id = $1
           LEFT JOIN tenants     t ON t.id = m.tenant_id
          WHERE u.id = $2",
    )
    .bind(tenant_id)
    .bind(user_id)
    .fetch_optional(&mut **tx)
    .await?)
}

/// Inserts the first `owner/active` membership for a newly created tenant.
///
/// Must run on a transaction that has *both* GUCs set (tenant + user), since
/// the `memberships` INSERT policy checks `tenant_id = app_tenant_id()`.
/// `tenant_service::create_tenant` sets them atomically before this call.
pub async fn insert_owner(tx: &mut Tx, tenant_id: Uuid, user_id: Uuid) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO memberships (tenant_id, user_id, role, status)
         VALUES ($1, $2, 'owner', 'active')",
    )
    .bind(tenant_id)
    .bind(user_id)
    .execute(&mut **tx)
    .await
    .map_err(map_db_error)?;
    Ok(())
}

/// Lists the caller's pending (`invited`) memberships across all tenants.
/// Used for `GET /invitations`.
pub async fn list_invited_for_user(
    tx: &mut Tx,
    user_id: Uuid,
) -> Result<Vec<InvitationRow>, AppError> {
    Ok(sqlx::query_as::<_, InvitationRow>(
        "SELECT m.tenant_id,
                t.name  AS tenant_name,
                t.slug  AS tenant_slug,
                m.role,
                m.invited_by,
                m.created_at
           FROM memberships m
           JOIN tenants t ON t.id = m.tenant_id
          WHERE m.user_id = $1
            AND m.status  = 'invited'
            AND t.status  = 'active'
          ORDER BY m.created_at DESC",
    )
    .bind(user_id)
    .fetch_all(&mut **tx)
    .await?)
}

/// Transitions the caller's `invited` membership for `tenant_id` to `active`.
///
/// Runs on a **tenant-scoped** transaction (GUC = T + U). RLS's
/// `memberships` UPDATE policy (`tenant_id = app_tenant_id()`) ensures the
/// caller can only flip their own invited row in the right tenant. If the
/// membership doesn't exist or is not in `invited` state, `NotFound` is
/// returned (conceals whether the tenant exists).
pub async fn accept_invitation(
    tx: &mut Tx,
    user_id: Uuid,
    tenant_id: Uuid,
) -> Result<(), AppError> {
    let rows_affected = sqlx::query(
        "UPDATE memberships
            SET status = 'active'
          WHERE tenant_id = $1
            AND user_id   = $2
            AND status    = 'invited'",
    )
    .bind(tenant_id)
    .bind(user_id)
    .execute(&mut **tx)
    .await
    .map_err(map_db_error)?
    .rows_affected();

    if rows_affected == 0 {
        Err(AppError::NotFound)
    } else {
        Ok(())
    }
}

// ── Tenant-scoped queries ─────────────────────────────────────────────────────

/// Lists all members of the current tenant via the `tenant_member_directory()`
/// SECURITY DEFINER function.
///
/// The function reads `app.current_tenant_id` internally — it takes no
/// parameter — so it cannot be redirected to another tenant.
pub async fn list_members(tx: &mut Tx) -> Result<Vec<MemberRow>, AppError> {
    Ok(sqlx::query_as::<_, MemberRow>(
        "SELECT user_id, email, role, status, joined_at
           FROM tenant_member_directory()
          ORDER BY joined_at",
    )
    .fetch_all(&mut **tx)
    .await?)
}

/// Loads a single membership by target user id within the current tenant.
/// Returns `None` when the membership doesn't exist (RLS hides it too).
pub async fn find_member(
    tx: &mut Tx,
    target_user_id: Uuid,
) -> Result<Option<MembershipRow>, AppError> {
    Ok(sqlx::query_as::<_, MembershipRow>(
        "SELECT id, user_id, role, status, updated_at
           FROM memberships
          WHERE user_id = $1",
    )
    .bind(target_user_id)
    .fetch_optional(&mut **tx)
    .await?)
}

/// Creates or ignores an invited membership for `target_user_id`.
///
/// If the user is already a member (any status) the unique constraint fires and
/// `map_db_error` returns `Conflict { code: "ALREADY_MEMBER" }`. The service
/// layer catches that specific code for endpoints that want to silently ignore
/// duplicates (e.g. `POST /invitations` returns 202 regardless).
pub async fn upsert_invited(
    tx: &mut Tx,
    target_user_id: Uuid,
    role: &str,
    invited_by: Uuid,
) -> Result<(), AppError> {
    // `tenant_id` comes from `app.current_tenant_id` via RLS / policy;
    // we still need to pass it explicitly to the INSERT because the GUC is for
    // row filtering (WITH CHECK), not for column defaults.
    sqlx::query(
        "INSERT INTO memberships (tenant_id, user_id, role, status, invited_by)
         SELECT (SELECT app_tenant_id()), $1, $2, 'invited', $3",
    )
    .bind(target_user_id)
    .bind(role)
    .bind(invited_by)
    .execute(&mut **tx)
    .await
    .map_err(map_db_error)?;
    Ok(())
}

/// Updates `role` and/or `status` for a membership row.
///
/// Only `Some` fields are changed (COALESCE). Returns the updated row.
/// If the update would leave the tenant with no active owner, the
/// `memberships_protect_last_owner` trigger raises P0001/LAST_OWNER,
/// which `map_db_error` converts to `Conflict { code: "LAST_OWNER" }`.
pub async fn update_member(
    tx: &mut Tx,
    target_user_id: Uuid,
    role: Option<&str>,
    status: Option<&str>,
) -> Result<MembershipRow, AppError> {
    sqlx::query_as::<_, MembershipRow>(
        "UPDATE memberships
            SET role   = COALESCE($2, role),
                status = COALESCE($3, status)
          WHERE user_id = $1
          RETURNING id, user_id, role, status, updated_at",
    )
    .bind(target_user_id)
    .bind(role)
    .bind(status)
    .fetch_optional(&mut **tx)
    .await
    .map_err(map_db_error)?
    .ok_or(AppError::NotFound)
}

/// Removes a membership row from the current tenant.
///
/// If the target is the last active owner the trigger raises LAST_OWNER.
/// Returns `NotFound` when the membership doesn't exist (also RLS-hidden rows).
pub async fn delete_member(tx: &mut Tx, target_user_id: Uuid) -> Result<(), AppError> {
    let rows_affected = sqlx::query(
        "DELETE FROM memberships
          WHERE user_id = $1",
    )
    .bind(target_user_id)
    .execute(&mut **tx)
    .await
    .map_err(map_db_error)?
    .rows_affected();

    if rows_affected == 0 {
        Err(AppError::NotFound)
    } else {
        Ok(())
    }
}
