use std::time::Duration;

use axum::{
    http::{Request, StatusCode},
    routing::get,
    Router,
};
use tower::ServiceBuilder;
use tower_http::{
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    timeout::TimeoutLayer,
    trace::TraceLayer,
};

use crate::{app_state::AppState, handlers};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

// `/api/v1` routes are added in Phase 2.
pub fn build_router(state: AppState) -> Router {
    let middleware = ServiceBuilder::new()
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
        .layer(
            TraceLayer::new_for_http().make_span_with(|req: &Request<_>| {
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
            }),
        )
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            REQUEST_TIMEOUT,
        ));

    Router::new()
        .route("/health/live", get(handlers::health::liveness))
        .route("/health/ready", get(handlers::health::readiness))
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
        AppState {
            db,
            config: Arc::new(config),
        }
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
