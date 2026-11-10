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
