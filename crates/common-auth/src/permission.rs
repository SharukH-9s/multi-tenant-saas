use serde::{Deserialize, Serialize};

use crate::role::TenantRole;

/// Things a member may be allowed to do inside a tenant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    /// Change the tenant name.
    TenantUpdateSettings,
    /// Change slug, plan or status.
    TenantManageCritical,
    MembersManage,
    DocumentsUpload,
    DocumentsSearch,
    DocumentsDeleteAny,
    DocumentsDeleteOwn,
    UsageViewAll,
    UsageViewOwn,
}

impl Permission {
    pub const ALL: [Permission; 9] = [
        Permission::TenantUpdateSettings,
        Permission::TenantManageCritical,
        Permission::MembersManage,
        Permission::DocumentsUpload,
        Permission::DocumentsSearch,
        Permission::DocumentsDeleteAny,
        Permission::DocumentsDeleteOwn,
        Permission::UsageViewAll,
        Permission::UsageViewOwn,
    ];
}

/// The role policy (decision D8 in the Phase 2 plan), in one place.
/// The `match` is exhaustive, so adding a role forces a decision here.
pub fn role_permissions(role: TenantRole) -> &'static [Permission] {
    use Permission::*;

    match role {
        TenantRole::Owner => &[
            TenantUpdateSettings,
            TenantManageCritical,
            MembersManage,
            DocumentsUpload,
            DocumentsSearch,
            DocumentsDeleteAny,
            DocumentsDeleteOwn,
            UsageViewAll,
            UsageViewOwn,
        ],
        TenantRole::Admin => &[
            TenantUpdateSettings,
            MembersManage,
            DocumentsUpload,
            DocumentsSearch,
            DocumentsDeleteAny,
            DocumentsDeleteOwn,
            UsageViewAll,
            UsageViewOwn,
        ],
        TenantRole::Member => &[
            DocumentsUpload,
            DocumentsSearch,
            DocumentsDeleteOwn,
            UsageViewOwn,
        ],
        TenantRole::Viewer => &[DocumentsSearch],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Permission::*;
    use TenantRole::*;

    /// The full matrix, written out so any accidental change fails loudly.
    /// Columns: Owner, Admin, Member, Viewer.
    #[test]
    fn full_role_permission_matrix() {
        let matrix = [
            (TenantUpdateSettings, [true, true, false, false]),
            (TenantManageCritical, [true, false, false, false]),
            (MembersManage, [true, true, false, false]),
            (DocumentsUpload, [true, true, true, false]),
            (DocumentsSearch, [true, true, true, true]),
            (DocumentsDeleteAny, [true, true, false, false]),
            (DocumentsDeleteOwn, [true, true, true, false]),
            (UsageViewAll, [true, true, false, false]),
            (UsageViewOwn, [true, true, true, false]),
        ];
        assert_eq!(matrix.len(), Permission::ALL.len());

        for (permission, expected) in matrix {
            for (role, allowed) in [Owner, Admin, Member, Viewer].into_iter().zip(expected) {
                assert_eq!(
                    role_permissions(role).contains(&permission),
                    allowed,
                    "{role} / {permission:?}"
                );
            }
        }
    }

    #[test]
    fn permissions_serialize_as_snake_case() {
        let json = serde_json::to_string(&DocumentsDeleteOwn).unwrap();
        assert_eq!(json, "\"documents_delete_own\"");
        for permission in Permission::ALL {
            let json = serde_json::to_string(&permission).unwrap();
            assert_eq!(
                serde_json::from_str::<Permission>(&json).unwrap(),
                permission
            );
        }
    }
}
