//! Request authentication and tenant resolution.
//!
//! `extractors`: `AuthUser` (Bearer token to the caller) and `TenantCtx`
//! (caller + tenant id from the URL to a verified `RequestContext`).
//! `tenant_resolver`: the only place that builds a `TenantContext`.
//! `request_context`: `RequestContext` bundles `TenantContext` + `ChunkTarget`.
//! `client_ip`: resolves the effective client IP for rate limiting.

pub mod client_ip;
pub mod extractors;
pub mod request_context;
pub mod tenant_resolver;

pub use request_context::RequestContext;
