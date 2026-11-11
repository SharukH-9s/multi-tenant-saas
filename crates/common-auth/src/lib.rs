//! # common-auth
//!
//! Pure auth building blocks: roles, permissions, password hashing, access
//! tokens, refresh tokens and the verified tenant context.
//!
//! This crate never touches the database. The app looks things up and hands
//! the results in.

mod context;
mod password;
mod permission;
mod principal;
mod refresh;
mod role;
mod token;

pub use context::TenantContext;
pub use password::{PasswordService, PasswordSettings, MAX_PASSWORD_CHARS, MIN_PASSWORD_CHARS};
pub use permission::{role_permissions, Permission};
pub use principal::AuthenticatedPrincipal;
pub use refresh::{generate_refresh_token, hash_refresh_token, RefreshToken};
pub use role::{ParseRoleError, TenantRole};
pub use token::{AccessClaims, IssuedToken, TokenService, TokenSettings};
