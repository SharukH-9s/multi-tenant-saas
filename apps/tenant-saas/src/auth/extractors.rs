use axum::{
    extract::{FromRequestParts, RawPathParams},
    http::{header::AUTHORIZATION, request::Parts, HeaderMap},
};
use common_auth::{AuthenticatedPrincipal, TenantContext};
use common_errors::AppError;

use super::tenant_resolver;
use crate::app_state::AppState;

/// Name of the URL parameter that carries the tenant id: `/tenants/{tenant_id}/...`.
const TENANT_PARAM: &str = "tenant_id";

/// The caller, taken from a valid `Authorization: Bearer <access token>` header.
///
/// This is a local wrapper because `AuthenticatedPrincipal` lives in another
/// crate, and Rust does not allow implementing axum's trait on it here.
///
/// Every failure (missing header, wrong scheme, bad signature, expired, wrong
/// issuer/audience/type) is the same generic `401`.
#[derive(Debug, Clone, Copy)]
pub struct AuthUser(pub AuthenticatedPrincipal);

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = bearer_token(&parts.headers).ok_or(AppError::Unauthorized)?;
        let claims = state.token_service.verify_access(token)?;
        Ok(AuthUser(AuthenticatedPrincipal::from(&claims)))
    }
}

/// A verified tenant context for routes shaped like `/.../{tenant_id}/...`.
///
/// It first authenticates the caller (401), then reads `tenant_id` from the URL
/// path only (never the body, a header or the query), then asks the database
/// whether the caller is an active member (404 / 403 / 401, see `tenant_resolver`).
/// Handlers then call `ctx.require(Permission::X)?` and `begin_tenant_tx`.
#[derive(Debug, Clone)]
pub struct TenantCtx(pub TenantContext);

impl FromRequestParts<AppState> for TenantCtx {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // Authenticate first: a caller without a valid token learns nothing
        // about tenants, not even that an id is malformed.
        let AuthUser(principal) = AuthUser::from_request_parts(parts, state).await?;

        let params = RawPathParams::from_request_parts(parts, state)
            .await
            .map_err(|e| AppError::Internal(format!("no path parameters on route: {e}")))?;
        let raw_tenant_id = params
            .iter()
            .find_map(|(name, value)| (name == TENANT_PARAM).then_some(value))
            .ok_or_else(|| {
                AppError::Internal(format!("route has no `{{{TENANT_PARAM}}}` parameter"))
            })?;
        let tenant_id = tenant_resolver::parse_tenant_id(raw_tenant_id)?;

        let ctx = tenant_resolver::resolve(state, principal, tenant_id).await?;
        Ok(TenantCtx(ctx))
    }
}

/// Returns the token from `Authorization: Bearer <token>` (scheme is case-insensitive).
fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    let token = token.trim();

    if scheme.eq_ignore_ascii_case("bearer") && !token.is_empty() {
        Some(token)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    fn headers(value: &str) -> HeaderMap {
        let mut map = HeaderMap::new();
        map.insert(AUTHORIZATION, HeaderValue::from_str(value).unwrap());
        map
    }

    #[test]
    fn reads_a_bearer_token() {
        assert_eq!(bearer_token(&headers("Bearer abc.def")), Some("abc.def"));
        assert_eq!(bearer_token(&headers("bearer abc")), Some("abc"));
    }

    #[test]
    fn rejects_missing_or_malformed_headers() {
        assert_eq!(bearer_token(&HeaderMap::new()), None);
        assert_eq!(bearer_token(&headers("Bearer")), None);
        assert_eq!(bearer_token(&headers("Bearer ")), None);
        assert_eq!(bearer_token(&headers("Basic abc")), None);
        assert_eq!(bearer_token(&headers("abc")), None);
    }
}
