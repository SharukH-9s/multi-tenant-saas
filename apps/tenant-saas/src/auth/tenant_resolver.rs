//! Turns "this user, this tenant id from the URL" into a trusted `TenantContext`.
//!
//! This is the ONLY file that builds a `TenantContext`. Everything else gets one
//! from here, so a tenant id from a request can never be trusted by accident.
//!
//! The tenant id in the URL is only a hint. The database says whether the user
//! really is an active member, and it is asked on every request, so removing a
//! member or changing a role takes effect immediately.

use std::str::FromStr;

use common_auth::{AuthenticatedPrincipal, TenantContext, TenantRole};
use common_database::begin_user_tx;
use common_errors::AppError;
use uuid::Uuid;

use crate::{
    app_state::AppState,
    repositories::membership_repo::{self, AccessRow},
};

const ACTIVE: &str = "active";

pub async fn resolve(
    state: &AppState,
    principal: AuthenticatedPrincipal,
    tenant_id: Uuid,
) -> Result<TenantContext, AppError> {
    let mut tx = begin_user_tx(&state.db, principal.user_id).await?;
    let row = membership_repo::load_access(&mut tx, tenant_id, principal.user_id).await?;
    // Give the connection back before deciding; nothing below needs the database.
    tx.commit().await?;

    decide(tenant_id, principal.user_id, row)
}

/// Parses the tenant id from a URL. Anything that is not a UUID gets the same
/// error as an unknown tenant, so a client cannot tell the two apart.
pub fn parse_tenant_id(raw: &str) -> Result<Uuid, AppError> {
    Uuid::parse_str(raw).map_err(|_| AppError::NotFound)
}

/// The access rules (design D7), as a pure function so they are easy to test.
///
/// | Database says                                   | Result          |
/// |-------------------------------------------------|-----------------|
/// | user missing or not active                      | `Unauthorized`  |
/// | no membership                                   | `NotFound`      |
/// | membership invited or suspended                 | `Forbidden`     |
/// | active membership, tenant not active            | `Forbidden`     |
/// | active membership, active tenant                | `TenantContext` |
///
/// `NotFound` is also what an unknown tenant gets, which hides which tenants exist.
fn decide(
    tenant_id: Uuid,
    user_id: Uuid,
    row: Option<AccessRow>,
) -> Result<TenantContext, AppError> {
    let row = row.ok_or(AppError::Unauthorized)?;
    if row.user_status != ACTIVE {
        return Err(AppError::Unauthorized);
    }

    let (Some(role), Some(membership_status)) = (row.role, row.membership_status) else {
        return Err(AppError::NotFound);
    };
    if membership_status != ACTIVE {
        return Err(AppError::Forbidden);
    }

    // An active member can always see their tenant, so `None` here is unexpected.
    match row.tenant_status.as_deref() {
        Some(ACTIVE) => {}
        Some(_) => return Err(AppError::Forbidden),
        None => return Err(AppError::NotFound),
    }

    let role = TenantRole::from_str(&role)
        .map_err(|e| AppError::Internal(format!("membership has an invalid role: {e}")))?;

    Ok(TenantContext::from_verified_membership(
        tenant_id, user_id, role,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A row that resolves successfully; each test breaks one thing.
    fn good_row() -> AccessRow {
        AccessRow {
            user_status: "active".into(),
            role: Some("member".into()),
            membership_status: Some("active".into()),
            tenant_status: Some("active".into()),
        }
    }

    fn run(row: Option<AccessRow>) -> Result<TenantContext, AppError> {
        decide(Uuid::new_v4(), Uuid::new_v4(), row)
    }

    #[test]
    fn active_member_of_active_tenant_gets_a_context() {
        let (tenant_id, user_id) = (Uuid::new_v4(), Uuid::new_v4());
        let ctx = decide(tenant_id, user_id, Some(good_row())).unwrap();

        assert_eq!(ctx.tenant_id(), tenant_id);
        assert_eq!(ctx.user_id(), user_id);
        assert_eq!(ctx.role(), TenantRole::Member);
    }

    #[test]
    fn every_role_is_carried_through() {
        for role in TenantRole::ALL {
            let row = AccessRow {
                role: Some(role.as_str().into()),
                ..good_row()
            };
            assert_eq!(run(Some(row)).unwrap().role(), role);
        }
    }

    #[test]
    fn missing_or_inactive_user_is_unauthorized() {
        assert!(matches!(run(None), Err(AppError::Unauthorized)));

        for status in ["disabled", "pending"] {
            let row = AccessRow {
                user_status: status.into(),
                ..good_row()
            };
            assert!(matches!(run(Some(row)), Err(AppError::Unauthorized)));
        }
    }

    #[test]
    fn inactive_user_is_unauthorized_even_without_a_membership() {
        // Checked first, so a disabled user never gets a 404 that hints at tenants.
        let row = AccessRow {
            user_status: "disabled".into(),
            role: None,
            membership_status: None,
            tenant_status: None,
        };
        assert!(matches!(run(Some(row)), Err(AppError::Unauthorized)));
    }

    #[test]
    fn no_membership_is_not_found() {
        let row = AccessRow {
            role: None,
            membership_status: None,
            tenant_status: None,
            ..good_row()
        };
        assert!(matches!(run(Some(row)), Err(AppError::NotFound)));
    }

    #[test]
    fn invited_or_suspended_member_is_forbidden_even_when_the_tenant_is_hidden() {
        // RLS hides the tenant from non-active members, so tenant_status is None.
        for status in ["invited", "suspended"] {
            let row = AccessRow {
                membership_status: Some(status.into()),
                tenant_status: None,
                ..good_row()
            };
            assert!(
                matches!(run(Some(row)), Err(AppError::Forbidden)),
                "{status}"
            );
        }
    }

    #[test]
    fn inactive_tenant_is_forbidden() {
        for status in ["suspended", "deleted"] {
            let row = AccessRow {
                tenant_status: Some(status.into()),
                ..good_row()
            };
            assert!(
                matches!(run(Some(row)), Err(AppError::Forbidden)),
                "{status}"
            );
        }
    }

    #[test]
    fn active_member_with_invisible_tenant_is_not_found() {
        let row = AccessRow {
            tenant_status: None,
            ..good_row()
        };
        assert!(matches!(run(Some(row)), Err(AppError::NotFound)));
    }

    #[test]
    fn unknown_role_is_an_internal_error_not_a_guess() {
        let row = AccessRow {
            role: Some("superadmin".into()),
            ..good_row()
        };
        assert!(matches!(run(Some(row)), Err(AppError::Internal(_))));
    }

    #[test]
    fn tenant_id_must_be_a_uuid_and_failures_look_like_unknown_tenants() {
        let id = Uuid::new_v4();
        assert_eq!(parse_tenant_id(&id.to_string()).unwrap(), id);

        for bad in ["", "nope", "123", "' OR 1=1 --", "../etc/passwd"] {
            assert!(
                matches!(parse_tenant_id(bad), Err(AppError::NotFound)),
                "{bad:?}"
            );
        }
    }
}
