use common_errors::AppError;
use uuid::Uuid;

use crate::{
    permission::{role_permissions, Permission},
    role::TenantRole,
};

/// "This user is an active member of this tenant, with this role."
///
/// It is immutable once built. `begin_tenant_tx` only accepts a `TenantContext`,
/// so code cannot open a tenant-scoped transaction from a raw tenant id that
/// came from a request.
///
/// Rust cannot stop other crates from calling the constructor, so the rule is
/// by convention: only the app's `auth::tenant_resolver` module calls
/// `from_verified_membership`, right after loading and checking the membership.
/// A CI grep guard enforces that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TenantContext {
    tenant_id: Uuid,
    user_id: Uuid,
    role: TenantRole,
}

impl TenantContext {
    pub fn from_verified_membership(tenant_id: Uuid, user_id: Uuid, role: TenantRole) -> Self {
        Self {
            tenant_id,
            user_id,
            role,
        }
    }

    pub fn tenant_id(&self) -> Uuid {
        self.tenant_id
    }

    pub fn user_id(&self) -> Uuid {
        self.user_id
    }

    pub fn role(&self) -> TenantRole {
        self.role
    }

    pub fn permissions(&self) -> &'static [Permission] {
        role_permissions(self.role)
    }

    pub fn has(&self, permission: Permission) -> bool {
        self.permissions().contains(&permission)
    }

    /// `Ok` if the role allows `permission`, otherwise `Forbidden`.
    pub fn require(&self, permission: Permission) -> Result<(), AppError> {
        if self.has(permission) {
            Ok(())
        } else {
            Err(AppError::Forbidden)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(role: TenantRole) -> TenantContext {
        TenantContext::from_verified_membership(Uuid::new_v4(), Uuid::new_v4(), role)
    }

    #[test]
    fn getters_return_what_was_given() {
        let (tenant_id, user_id) = (Uuid::new_v4(), Uuid::new_v4());
        let ctx = TenantContext::from_verified_membership(tenant_id, user_id, TenantRole::Admin);

        assert_eq!(ctx.tenant_id(), tenant_id);
        assert_eq!(ctx.user_id(), user_id);
        assert_eq!(ctx.role(), TenantRole::Admin);
    }

    #[test]
    fn require_allows_what_the_role_has() {
        assert!(context(TenantRole::Owner)
            .require(Permission::TenantManageCritical)
            .is_ok());
        assert!(context(TenantRole::Viewer)
            .require(Permission::DocumentsSearch)
            .is_ok());
    }

    #[test]
    fn require_returns_forbidden_otherwise() {
        assert!(matches!(
            context(TenantRole::Admin).require(Permission::TenantManageCritical),
            Err(AppError::Forbidden)
        ));
        assert!(matches!(
            context(TenantRole::Viewer).require(Permission::DocumentsUpload),
            Err(AppError::Forbidden)
        ));
    }
}
