use std::time::Duration;

use axum::{
    http::{Request, StatusCode},
    middleware::from_fn,
    routing::get,
    Router,
};
use common_errors::AppError;
use tower::ServiceBuilder;
use tower_http::{
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    timeout::TimeoutLayer,
    trace::{DefaultOnResponse, TraceLayer},
};
use tracing::Level;

use crate::{app_state::AppState, handlers, middleware::inject_request_id};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Versioned API routes. Feature routers are merged here.
fn api_v1() -> Router<AppState> {
    // Tenant sub-resources are nested under /tenants/{tenant_id} so that the
    // TenantCtx extractor can resolve the membership from the path parameter.
    let tenant_sub =
        handlers::members::router().merge(handlers::invitations::tenant_invitations_router());

    Router::new()
        .nest("/auth", handlers::auth::router())
        .nest(
            "/tenants",
            handlers::tenants::router().nest("/{tenant_id}", tenant_sub),
        )
        .nest("/invitations", handlers::invitations::invitations_router())
}

/// Unknown routes use the standard error envelope instead of axum's empty 404.
async fn not_found() -> AppError {
    AppError::NotFound
}

pub fn build_router(state: AppState) -> Router {
    let middleware = ServiceBuilder::new()
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid)) // Assigns or preserves a unique UUID (x-request-id)
        .layer(
            // Opens a tracing span: { method, uri, request_id }
            TraceLayer::new_for_http()
                .make_span_with(|req: &Request<_>| {
                    let request_id = req
                        .headers()
                        .get("x-request-id")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("unknown");
                    tracing::info_span!(
                        "http_request",
                        method = %req.method(),
                        uri = %req.uri(),
                        request_id,
                    )
                })
                // Logs status and latency; the default level (DEBUG) is hidden by RUST_LOG.
                .on_response(DefaultOnResponse::new().level(Level::INFO)),
        )
        .layer(PropagateRequestIdLayer::x_request_id()) // Copies the x-request-id header onto the outgoing response
        .layer(TimeoutLayer::with_status_code(
            // Guards the request with a strict 30-second deadline
            StatusCode::REQUEST_TIMEOUT,
            REQUEST_TIMEOUT,
        ))
        // Innermost: sees the request id set above and rewrites AppError bodies.
        .layer(from_fn(inject_request_id));

    Router::new()
        .route("/health/live", get(handlers::health::liveness))
        .route("/health/ready", get(handlers::health::readiness))
        .nest("/api/v1", api_v1()) //
        .fallback(not_found)
        .layer(middleware)
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{body::Body, http::StatusCode};
    use common_config::AppConfig;
    use sqlx::postgres::PgPoolOptions;
    use tower::ServiceExt;

    use super::*;

    /// State whose database is unreachable, so tests need no running PostgreSQL.
    fn state_without_database() -> AppState {
        let env = |key: &str| match key {
            "DATABASE_URL" => Some("postgres://u:p@127.0.0.1:1/db".to_string()),
            "OBJECT_STORAGE_ACCESS_KEY" | "OBJECT_STORAGE_SECRET_KEY" => Some("x".to_string()),
            "JWT_SECRET" => Some("x".repeat(32)),
            _ => None,
        };
        let config = AppConfig::load_from_env_getter(env).unwrap();
        let db = PgPoolOptions::new()
            .acquire_timeout(Duration::from_millis(200))
            .connect_lazy(&config.database.url)
            .unwrap();
        AppState::new(db, Arc::new(config)).unwrap()
    }

    async fn get_status(path: &str) -> StatusCode {
        let request = Request::builder().uri(path).body(Body::empty()).unwrap();
        build_router(state_without_database())
            .oneshot(request)
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn unknown_api_route_is_404() {
        assert_eq!(get_status("/api/v1/nope").await, StatusCode::NOT_FOUND);
    }

    // ── New Task 8 routes: unauthenticated requests return 401, not 404 ───────

    #[tokio::test]
    async fn tenant_routes_exist_and_require_auth() {
        // POST /tenants — missing token → 401
        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/tenants")
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap();
        let status = build_router(state_without_database())
            .oneshot(req)
            .await
            .unwrap()
            .status();
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn member_routes_exist_and_require_auth() {
        // GET /tenants/:id/members — missing token → 401
        let id = uuid::Uuid::new_v4();
        assert_eq!(
            get_status(&format!("/api/v1/tenants/{id}/members")).await,
            StatusCode::UNAUTHORIZED,
        );
    }

    #[tokio::test]
    async fn invitation_routes_exist_and_require_auth() {
        // GET /invitations — missing token → 401
        assert_eq!(
            get_status("/api/v1/invitations").await,
            StatusCode::UNAUTHORIZED,
        );
    }

    #[tokio::test]
    async fn liveness_is_ok_without_database() {
        assert_eq!(get_status("/health/live").await, StatusCode::OK);
    }

    #[tokio::test]
    async fn readiness_is_503_without_database() {
        assert_eq!(
            get_status("/health/ready").await,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
}
