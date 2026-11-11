//! Tenant lifecycle: create, list (for the calling user), get, and update.
//!
//! Permission checks happen here, before any transaction is opened.
//! DB-level enforcement (RLS) is a second layer of defence, not the first.

use common_auth::{Permission, TenantContext};
use common_database::{begin_tenant_tx, begin_user_tx};
use common_errors::AppError;
use uuid::Uuid;

use crate::{
    app_state::AppState,
    repositories::{
        membership_repo,
        tenant_repo::{self, TenantPatch, TenantRow, UserTenantRow},
    },
};

/// Cap on active owned tenants per user (abuse prevention).
const MAX_OWNED_TENANTS: i64 = 10;

// ── Output types ──────────────────────────────────────────────────────────────

/// Returned by `create_tenant` and `get_tenant`.
pub struct TenantDetail {
    pub tenant: TenantRow,
    /// The calling user's role in this tenant.
    pub role: String,
}

// ── Service functions ─────────────────────────────────────────────────────────

/// Creates a new tenant and makes the calling user its first owner.
///
/// The insert and the owner membership are written in **one transaction** so
/// there is never a tenant without an owner. Both GUCs are set before any
/// write, satisfying the RLS INSERT policies on both tables.
///
/// Returns `Conflict { code: "SLUG_TAKEN" }` on duplicate slug.
/// Returns `Validation` when the user already owns 10 tenants.
pub async fn create_tenant(
    state: &AppState,
    user_id: Uuid,
    name: &str,
    slug: &str,
) -> Result<TenantRow, AppError> {
    // Pick the id now so we can set the tenant GUC before the INSERT,
    // which is required by the `tenants` RLS INSERT policy (`id = app_tenant_id()`).
    let tenant_id = Uuid::new_v4();

    // Open a user-scoped tx, then also set the tenant GUC so the memberships
    // INSERT policy (`tenant_id = app_tenant_id()`) passes too.
    let mut tx = begin_user_tx(&state.db, user_id).await?;

    // Set both GUCs: user is already set by begin_user_tx; now add tenant.
    sqlx::query("SELECT set_config('app.current_tenant_id', $1, true)")
        .bind(tenant_id.to_string())
        .execute(&mut *tx)
        .await?;

    let owned = tenant_repo::count_owned(&mut tx, user_id).await?;
    if owned >= MAX_OWNED_TENANTS {
        return Err(AppError::Validation(format!(
            "You may not own more than {MAX_OWNED_TENANTS} tenants."
        )));
    }

    let tenant = tenant_repo::insert(&mut tx, tenant_id, name, slug).await?;
    membership_repo::insert_owner(&mut tx, tenant_id, user_id).await?;
    tx.commit().await?;

    tracing::info!(
        target: "audit",
        event = "tenant.created",
        actor_user_id = %user_id,
        tenant_id = %tenant.id,
    );

    Ok(tenant)
}

/// Lists the calling user's active tenant memberships.
/// `after` is a cursor (the `tenant_id` of the last item on the previous page).
pub async fn list_user_tenants(
    state: &AppState,
    user_id: Uuid,
    after: Option<Uuid>,
    limit: i64,
) -> Result<Vec<UserTenantRow>, AppError> {
    let mut tx = begin_user_tx(&state.db, user_id).await?;
    let rows = tenant_repo::list_for_user(&mut tx, user_id, after, limit).await?;
    tx.commit().await?;
    Ok(rows)
}

/// Returns a single tenant that the caller is an active member of.
pub async fn get_tenant(state: &AppState, ctx: &TenantContext) -> Result<TenantDetail, AppError> {
    let mut tx = begin_tenant_tx(&state.db, ctx).await?;
    let tenant = tenant_repo::get_by_id(&mut tx, ctx.tenant_id()).await?;
    tx.commit().await?;

    Ok(TenantDetail {
        tenant,
        role: ctx.role().as_str().to_owned(),
    })
}

/// Applies a partial update to a tenant.
///
/// Permission rules (from decision D8):
/// - Any field present → requires `TenantUpdateSettings` (Owner + Admin).
/// - `slug`, `plan`, or `status` present → additionally requires
///   `TenantManageCritical` (Owner only). An Admin hitting this returns `403`.
pub async fn update_tenant(
    state: &AppState,
    ctx: &TenantContext,
    patch: TenantPatch,
) -> Result<TenantRow, AppError> {
    // Every field requires at least update-settings permission.
    ctx.require(Permission::TenantUpdateSettings)?;

    // Critical fields require a stricter permission.
    if patch.slug.is_some() || patch.plan.is_some() || patch.status.is_some() {
        ctx.require(Permission::TenantManageCritical)?;
    }

    let mut tx = begin_tenant_tx(&state.db, ctx).await?;
    let tenant = tenant_repo::update(&mut tx, ctx.tenant_id(), patch).await?;
    tx.commit().await?;

    tracing::info!(
        target: "audit",
        event = "tenant.updated",
        actor_user_id = %ctx.user_id(),
        tenant_id = %tenant.id,
    );

    Ok(tenant)
}
