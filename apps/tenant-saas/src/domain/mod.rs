//! Domain types: value objects that are valid by construction.
//!
//! Nothing here knows about Axum or SQLx. Handlers turn raw request strings
//! into these types first, so the rest of the code never sees invalid input.
//! The rules mirror the CHECK constraints in `002_identity.sql`.

pub mod tenant;
pub mod user;

pub use tenant::{Slug, TenantName};
pub use user::{Email, Password};
