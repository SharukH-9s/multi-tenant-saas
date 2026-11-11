use uuid::Uuid;

/// Who is calling: the user from a verified access token.
/// This says nothing about tenants; see `TenantContext` for that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthenticatedPrincipal {
    pub user_id: Uuid,
    pub token_id: Uuid,
}
