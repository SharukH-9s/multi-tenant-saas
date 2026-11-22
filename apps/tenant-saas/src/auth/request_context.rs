//! `RequestContext` — the per-request bundle of identity and chunk routing.
//!
//! An HTTP request goes through two resolution steps before a handler runs:
//!
//! 1. `AuthUser` verifies the Bearer token → `AuthenticatedPrincipal`.
//! 2. `TenantCtx` calls `tenant_resolver::resolve` →
//!    `TenantContext` (identity) + `ChunkTarget` (table routing), bundled
//!    here into a single `RequestContext`.
//!
//! # Deref to `TenantContext`
//!
//! `RequestContext: Deref<Target = TenantContext>` so that existing code
//! which accepts `&TenantContext` continues to work via deref coercion:
//!
//! ```ignore
//! // Both of these compile when `rc: RequestContext`:
//! some_service(&state, &rc, q).await?;         // &rc → &TenantContext
//! rc.require(Permission::DocumentsSearch)?;    // method via deref
//! rc.tenant_id()                               // method via deref
//! ```
//!
//! When a service or repository needs chunk routing (Task 11), it accepts
//! `&RequestContext` and reads `rc.chunk_target` directly.
//!
//! # Construction
//!
//! The primary constructor is `pub(crate) new(...)` — only
//! `auth::tenant_resolver` calls it in production. This keeps the CI grep
//! guard on the `TenantContext` constructor in force: a `RequestContext`
//! can only be built after a verified membership lookup.
//!
//! For tests that operate outside the HTTP middleware (integration tests,
//! evaluation harnesses), `for_shared(ctx)` is available publicly — it
//! builds a `RequestContext` over the shared `public.document_chunks` table,
//! which is the correct target for all unpromoted test tenants.

use std::ops::Deref;

use common_auth::TenantContext;

use crate::domain::chunk::ChunkTarget;

/// A verified, per-request context that bundles tenant identity with
/// chunk-table routing.
#[derive(Debug, Clone)]
pub struct RequestContext {
    inner: TenantContext,
    /// The `document_chunks` table this tenant's chunks live in.
    pub chunk_target: ChunkTarget,
}

impl RequestContext {
    /// Constructs from a verified `TenantContext` and a resolved `ChunkTarget`.
    ///
    /// `pub(crate)` — only `auth::tenant_resolver` calls this in production.
    pub(crate) fn new(inner: TenantContext, chunk_target: ChunkTarget) -> Self {
        Self {
            inner,
            chunk_target,
        }
    }

    /// Builds a `RequestContext` targeting the shared `public.document_chunks`
    /// table (i.e., `ChunkTarget::Shared`).
    ///
    /// Intended for integration tests and evaluation harnesses that construct a
    /// context outside the HTTP middleware, where all test tenants are
    /// unpromoted and therefore use the shared table.
    pub fn for_shared(inner: TenantContext) -> Self {
        Self::new(inner, ChunkTarget::Shared)
    }
}

impl Deref for RequestContext {
    type Target = TenantContext;

    /// Dereferences to the inner `TenantContext`.
    ///
    /// This enables deref coercion: `&RequestContext` → `&TenantContext`,
    /// so services that accept `&TenantContext` receive a `&rc` without any
    /// call-site changes.
    fn deref(&self) -> &TenantContext {
        &self.inner
    }
}
