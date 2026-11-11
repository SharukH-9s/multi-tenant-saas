//! Library half of the `tenant-saas` service.
//!
//! The binary (`main.rs`) is a thin wrapper so integration tests can build the
//! router with `build_router` and drive it with `tower::ServiceExt::oneshot`.
//!
//! Layers: handlers -> services -> repositories. The `domain` module has no
//! Axum or SQLx types.

pub mod app_state;
pub mod auth;
pub mod domain;
pub mod handlers;
pub mod middleware;
pub mod repositories;
pub mod routes;
pub mod services;
