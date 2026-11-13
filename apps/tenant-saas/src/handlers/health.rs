use axum::{extract::State, http::StatusCode, Json};
use serde_json::{json, Value};

use crate::app_state::AppState;

/// The process is up. Deliberately checks no dependencies.
pub async fn liveness() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

/// The API can serve traffic. In Phase 1 that only depends on PostgreSQL.
pub async fn readiness(State(state): State<AppState>) -> (StatusCode, Json<Value>) {
    match common_database::ping(&state.db).await {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({ "status": "ready", "db": "ok" })),
        ),
        Err(_) => {
            tracing::warn!("Readiness check failed: database unavailable");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "status": "not_ready", "db": "unavailable" })),
            )
        }
    }
}

/// Detailed dependency health check for database, storage, and Redis.
/// Returns 200 OK if all dependencies are healthy, or 503 Service Unavailable if any are down.
pub async fn dependencies(State(state): State<AppState>) -> (StatusCode, Json<Value>) {
    let db_ok = common_database::ping(&state.db).await.is_ok();
    let storage_ok = state.storage.health().await.is_ok();
    let redis_ok = state.rate_limiter.health().await;

    let all_ok = db_ok && storage_ok && redis_ok;
    let status_code = if all_ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    (
        status_code,
        Json(json!({
            "status": if all_ok { "ok" } else { "degraded" },
            "database": if db_ok { "ok" } else { "unavailable" },
            "storage": if storage_ok { "ok" } else { "unavailable" },
            "redis": if redis_ok { "ok" } else { "unavailable" },
        })),
    )
}
