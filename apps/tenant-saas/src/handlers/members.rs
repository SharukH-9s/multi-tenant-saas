//! `/api/v1/tenants/:tenant_id/members` — list, update, remove members.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, patch},
    Json, Router,
};
use chrono::{DateTime, Utc};
use common_auth::TenantRole;
use common_errors::{ApiJson, AppError};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{app_state::AppState, auth::extractors::TenantCtx, services::membership_service};

pub fn router() -> Router<AppState> {
    // Note: mounted under /tenants/:tenant_id by the parent router.
    Router::new().route("/members", get(list_members)).route(
        "/members/{user_id}",
        patch(update_member).delete(remove_member),
    )
}

// ── Request / response types ──────────────────────────────────────────────────

#[derive(Deserialize)]
struct UpdateMemberRequest {
    role: Option<String>,
    status: Option<String>,
}

#[derive(Serialize)]
struct MemberResponse {
    user_id: Uuid,
    email: String,
    role: String,
    status: String,
    joined_at: DateTime<Utc>,
}

#[derive(Serialize)]
struct MembershipResponse {
    id: Uuid,
    user_id: Uuid,
    role: String,
    status: String,
    updated_at: DateTime<Utc>,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

async fn list_members(
    State(state): State<AppState>,
    TenantCtx(ctx): TenantCtx,
) -> Result<Json<Vec<MemberResponse>>, AppError> {
    let members = membership_service::list_members(&state, &ctx).await?;

    Ok(Json(
        members
            .into_iter()
            .map(|m| MemberResponse {
                user_id: m.user_id,
                email: m.email,
                role: m.role,
                status: m.status,
                joined_at: m.joined_at,
            })
            .collect(),
    ))
}

async fn update_member(
    State(state): State<AppState>,
    TenantCtx(ctx): TenantCtx,
    Path((_tenant_id, target_user_id)): Path<(Uuid, Uuid)>,
    ApiJson(body): ApiJson<UpdateMemberRequest>,
) -> Result<Json<MembershipResponse>, AppError> {
    // Parse and validate the new role if provided.
    let new_role = body
        .role
        .as_deref()
        .map(|r| {
            r.parse::<TenantRole>()
                .map_err(|_| AppError::Validation(format!("unknown role '{r}'")))
        })
        .transpose()?;

    let updated = membership_service::update_member(
        &state,
        &ctx,
        target_user_id,
        new_role,
        body.status.as_deref(),
    )
    .await?;

    Ok(Json(MembershipResponse {
        id: updated.id,
        user_id: updated.user_id,
        role: updated.role,
        status: updated.status,
        updated_at: updated.updated_at,
    }))
}

async fn remove_member(
    State(state): State<AppState>,
    TenantCtx(ctx): TenantCtx,
    Path((_tenant_id, target_user_id)): Path<(Uuid, Uuid)>,
) -> Result<impl IntoResponse, AppError> {
    membership_service::remove_member(&state, &ctx, target_user_id).await?;
    Ok(StatusCode::NO_CONTENT)
}
