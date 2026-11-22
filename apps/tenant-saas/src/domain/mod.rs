//! Domain types: value objects that are valid by construction.
//!
//! Nothing here knows about Axum or SQLx. Handlers turn raw request strings
//! into these types first, so the rest of the code never sees invalid input.
//! The rules mirror the CHECK constraints in `002_identity.sql` and
//! `007_tenant_dedicated_schema.sql`.

pub mod chunk;
pub mod document;
pub mod ingestion;
pub mod search;
pub mod tenant;
pub mod user;

pub use chunk::{ChunkTarget, ValidatedSchema};
pub use document::{ContentHash, DocumentStatus, DocumentTitle, OriginalFilename, SupportedFormat};
pub use tenant::{Slug, TenantName};
pub use user::{Email, Password};
