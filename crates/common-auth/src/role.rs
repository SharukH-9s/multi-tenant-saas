use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

/// A user's role inside one tenant. The lowercase names match the
/// `memberships.role` check constraint in the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TenantRole {
    Owner,
    Admin,
    Member,
    Viewer,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseRoleError(pub String);

impl fmt::Display for ParseRoleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown role '{}'", self.0)
    }
}

impl std::error::Error for ParseRoleError {}

impl TenantRole {
    pub const ALL: [TenantRole; 4] = [
        TenantRole::Owner,
        TenantRole::Admin,
        TenantRole::Member,
        TenantRole::Viewer,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            TenantRole::Owner => "owner",
            TenantRole::Admin => "admin",
            TenantRole::Member => "member",
            TenantRole::Viewer => "viewer",
        }
    }

    /// Higher number = more powerful. Handy for sorting and display.
    pub fn rank(self) -> u8 {
        match self {
            TenantRole::Owner => 3,
            TenantRole::Admin => 2,
            TenantRole::Member => 1,
            TenantRole::Viewer => 0,
        }
    }

    /// Can someone with this role manage (invite, change, remove) a member
    /// whose role is `target`?
    ///
    /// - Owners can manage everyone.
    /// - Admins can manage everyone except owners.
    /// - Members and viewers manage nobody.
    ///
    /// To check a role *change*, call this for both the member's current role
    /// and the new role. That stops an admin from promoting anyone to owner.
    pub fn can_manage(self, target: TenantRole) -> bool {
        match self {
            TenantRole::Owner => true,
            TenantRole::Admin => target != TenantRole::Owner,
            TenantRole::Member | TenantRole::Viewer => false,
        }
    }
}

impl fmt::Display for TenantRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for TenantRole {
    type Err = ParseRoleError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        TenantRole::ALL
            .into_iter()
            .find(|role| role.as_str() == s)
            .ok_or_else(|| ParseRoleError(s.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_role_round_trips_through_string_and_json() {
        for role in TenantRole::ALL {
            assert_eq!(role.to_string().parse::<TenantRole>().unwrap(), role);

            let json = serde_json::to_string(&role).unwrap();
            assert_eq!(json, format!("\"{}\"", role.as_str()));
            assert_eq!(serde_json::from_str::<TenantRole>(&json).unwrap(), role);
        }
    }

    #[test]
    fn unknown_role_is_rejected() {
        assert!("superadmin".parse::<TenantRole>().is_err());
        assert!("Owner".parse::<TenantRole>().is_err());
    }

    #[test]
    fn rank_orders_roles_from_owner_down() {
        let ranks: Vec<u8> = TenantRole::ALL.iter().map(|r| r.rank()).collect();
        assert_eq!(ranks, vec![3, 2, 1, 0]);
    }

    #[test]
    fn can_manage_covers_all_sixteen_combinations() {
        use TenantRole::*;
        // (manager, target, allowed)
        let table = [
            (Owner, Owner, true),
            (Owner, Admin, true),
            (Owner, Member, true),
            (Owner, Viewer, true),
            (Admin, Owner, false),
            (Admin, Admin, true),
            (Admin, Member, true),
            (Admin, Viewer, true),
            (Member, Owner, false),
            (Member, Admin, false),
            (Member, Member, false),
            (Member, Viewer, false),
            (Viewer, Owner, false),
            (Viewer, Admin, false),
            (Viewer, Member, false),
            (Viewer, Viewer, false),
        ];
        for (manager, target, allowed) in table {
            assert_eq!(
                manager.can_manage(target),
                allowed,
                "{manager} managing {target}"
            );
        }
    }
}
