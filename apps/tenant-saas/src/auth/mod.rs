//! Request authentication and tenant resolution.
//!
//! `extractors`: `AuthUser` (Bearer token to the caller) and `TenantCtx`
//! (caller + tenant id from the URL to a verified `TenantContext`).
//! `tenant_resolver`: the only place that builds a `TenantContext`.
//! `client_ip`: resolves the effective client IP for rate limiting.

pub mod client_ip;
pub mod extractors;
pub mod tenant_resolver;
