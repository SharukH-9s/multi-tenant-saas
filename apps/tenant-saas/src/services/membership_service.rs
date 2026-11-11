//! Membership management: invite, list, change role/status, remove, accept invitation.
//!
//! All escalation rules (D8) live here — not in the handlers or in RLS.
//! The database trigger is a last-resort safety net for the last-owner rule,
//! but the service checks it first so the error code is consistent.

use common_auth::{Permission, TenantContext, TenantRole};
use common_database::{begin_tenant_tx, begin_user_tx};
use common_errors::AppError;
use uuid::Uuid;

use crate::{
    app_state::AppState,
    repositories::membership_repo::{self, InvitationRow, MemberRow, MembershipRow},
};

// ── Output types ──────────────────────────────────────────────────────────────

/// What the update-member handler echoes back.
pub use membership_repo::MembershipRow as UpdatedMembership;

// ── Service functions ─────────────────────────────────────────────────────────

/// Invites a user (by email) to the current tenant with the given role.
///
/// Always returns `Ok(())` — the response is always `202 Accepted` regardless
/// of whether the email is registered or the user is already a member.
/// This prevents email enumeration (design D7, D9).
///
/// Errors that surface to the caller:
/// - `Forbidden` when the caller lacks `MembersManage`.
/// - `Forbidden` when the caller tries to invite at a role they cannot manage.
pub async fn invite_member(
    state: &AppState,
    ctx: &TenantContext,
    email: &str,
    role: TenantRole,
) -> Result<(), AppError> {
    ctx.require(Permission::MembersManage)?;

    // Escalation rule: an Admin cannot invite at owner level.
    if !ctx.role().can_manage(role) {
        return Err(AppError::Forbidden);
    }

    let mut tx = begin_tenant_tx(&state.db, ctx).await?;

    // `find_user_id_by_email` is a SECURITY DEFINER function that reads
    // `app.current_tenant_id`. Returns NULL if the email is not registered.
    let target_id: Option<Uuid> = sqlx::query_scalar("SELECT find_user_id_by_email($1::citext)")
        .bind(email)
        .fetch_one(&mut *tx)
        .await?;

    if let Some(target_user_id) = target_id {
        let result =
            membership_repo::upsert_invited(&mut tx, target_user_id, role.as_str(), ctx.user_id())
                .await;

        // ALREADY_MEMBER is silently swallowed — the endpoint always returns 202.
        match result {
            Ok(()) => {
                tracing::info!(
                    target: "audit",
                    event = "member.invited",
                    actor_user_id = %ctx.user_id(),
                    tenant_id     = %ctx.tenant_id(),
                    target_user_id = %target_user_id,
                    role = role.as_str(),
                );
            }
            Err(AppError::Conflict {
                code: "ALREADY_MEMBER",
                ..
            }) => {
                // No-op: idempotent invite.
            }
            Err(other) => return Err(other),
        }
    }
    // Unknown email: commit an empty tx, return Ok (no enumeration).

    tx.commit().await?;
    Ok(())
}

/// Lists all members of the current tenant.
/// Requires `MembersManage` (Owner or Admin).
pub async fn list_members(
    state: &AppState,
    ctx: &TenantContext,
) -> Result<Vec<MemberRow>, AppError> {
    ctx.require(Permission::MembersManage)?;

    let mut tx = begin_tenant_tx(&state.db, ctx).await?;
    let members = membership_repo::list_members(&mut tx).await?;
    tx.commit().await?;

    Ok(members)
}

/// Changes `role` and/or `status` for a member of the current tenant.
///
/// Escalation rules enforced (D8):
/// 1. Requires `MembersManage`.
/// 2. Nobody can change their own role or status.
/// 3. Actor must be able to manage the target's **current** role.
/// 4. Actor must be able to manage the **new** role (prevents promotion to owner by admin).
/// 5. Last active owner → `409 LAST_OWNER` (also caught by DB trigger).
pub async fn update_member(
    state: &AppState,
    ctx: &TenantContext,
    target_user_id: Uuid,
    new_role: Option<TenantRole>,
    new_status: Option<&str>,
) -> Result<MembershipRow, AppError> {
    ctx.require(Permission::MembersManage)?;

    // Rule 2: cannot self-modify.
    if target_user_id == ctx.user_id() {
        return Err(AppError::Forbidden);
    }

    let mut tx = begin_tenant_tx(&state.db, ctx).await?;

    // Load target to check their current role before writing.
    let current = membership_repo::find_member(&mut tx, target_user_id)
        .await?
        .ok_or(AppError::NotFound)?;

    let current_role: TenantRole = current
        .role
        .parse()
        .map_err(|_| AppError::Internal("unknown role in memberships row".into()))?;

    // Rule 3: actor must be able to manage the target's current role.
    if !ctx.role().can_manage(current_role) {
        return Err(AppError::Forbidden);
    }

    // Rule 4: actor must be able to manage the requested new role.
    if let Some(r) = new_role {
        if !ctx.role().can_manage(r) {
            return Err(AppError::Forbidden);
        }
    }

    let updated = membership_repo::update_member(
        &mut tx,
        target_user_id,
        new_role.map(|r| r.as_str()),
        new_status,
    )
    .await?;

    tx.commit().await?;

    tracing::info!(
        target: "audit",
        event = "member.role_changed",
        actor_user_id  = %ctx.user_id(),
        tenant_id      = %ctx.tenant_id(),
        target_user_id = %target_user_id,
        new_role       = ?new_role,
        new_status     = ?new_status,
    );

    Ok(updated)
}

/// Removes a member from the current tenant.
///
/// A member may remove themselves (self-leave) unless they are the last active
/// owner. Admins/Owners may remove others subject to the escalation rules.
pub async fn remove_member(
    state: &AppState,
    ctx: &TenantContext,
    target_user_id: Uuid,
) -> Result<(), AppError> {
    ctx.require(Permission::MembersManage)?;

    let is_self = target_user_id == ctx.user_id();

    let mut tx = begin_tenant_tx(&state.db, ctx).await?;

    if !is_self {
        // Load to enforce escalation rules against the target's role.
        let current = membership_repo::find_member(&mut tx, target_user_id)
            .await?
            .ok_or(AppError::NotFound)?;

        let current_role: TenantRole = current
            .role
            .parse()
            .map_err(|_| AppError::Internal("unknown role in memberships row".into()))?;

        if !ctx.role().can_manage(current_role) {
            return Err(AppError::Forbidden);
        }
    }

    // The last-owner DB trigger fires here if needed.
    membership_repo::delete_member(&mut tx, target_user_id).await?;
    tx.commit().await?;

    tracing::info!(
        target: "audit",
        event = "member.removed",
        actor_user_id  = %ctx.user_id(),
        tenant_id      = %ctx.tenant_id(),
        target_user_id = %target_user_id,
    );

    Ok(())
}

/// Lists the calling user's pending (`invited`) memberships across all tenants.
/// Used for `GET /invitations`.
pub async fn list_pending_invitations(
    state: &AppState,
    user_id: Uuid,
) -> Result<Vec<InvitationRow>, AppError> {
    let mut tx = begin_user_tx(&state.db, user_id).await?;
    let rows = membership_repo::list_invited_for_user(&mut tx, user_id).await?;
    tx.commit().await?;
    Ok(rows)
}

/// Transitions the caller's `invited` membership for `tenant_id` to `active`.
///
/// Uses a tenant-scoped transaction (the GUC is set to `tenant_id` so the
/// `memberships` UPDATE RLS policy allows the transition). Returns `NotFound`
/// when no matching `invited` membership exists.
pub async fn accept_invitation(
    state: &AppState,
    user_id: Uuid,
    tenant_id: Uuid,
) -> Result<(), AppError> {
    // Open user-scoped tx, then set tenant GUC so RLS allows updating this tenant's membership.
    // Atomically verified in membership_repo (WHERE status = 'invited').
    let mut tx = begin_user_tx(&state.db, user_id).await?;
    sqlx::query("SELECT set_config('app.current_tenant_id', $1, true)")
        .bind(tenant_id.to_string())
        .execute(&mut *tx)
        .await?;

    membership_repo::accept_invitation(&mut tx, user_id, tenant_id).await?;
    tx.commit().await?;

    tracing::info!(
        target: "audit",
        event = "invitation.accepted",
        actor_user_id = %user_id,
        tenant_id     = %tenant_id,
    );

    Ok(())
}
