//! Invitation endpoints.
//!
//! `POST /api/v1/tenants/:tenant_id/invitations` — send an invite (tenant-scoped).
//! `GET  /api/v1/invitations`                    — list the caller's pending invites.
//! `POST /api/v1/invitations/:tenant_id/accept`  — accept a pending invite.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use common_auth::TenantRole;
use common_errors::{ApiJson, AppError};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    app_state::AppState,
    auth::extractors::{AuthUser, TenantCtx},
    services::membership_service,
};

/// Router mounted at `/tenants/:tenant_id` for the invite sub-resource.
pub fn tenant_invitations_router() -> Router<AppState> {
    Router::new().route("/invitations", post(invite_member))
}

/// Router mounted at `/invitations` for user-scoped invitation actions.
pub fn invitations_router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_invitations))
        .route("/{tenant_id}/accept", post(accept_invitation))
}

// ── Request / response types ──────────────────────────────────────────────────

#[derive(Deserialize)]
struct InviteRequest {
    email: String,
    role: String,
}

#[derive(Serialize)]
struct InvitationResponse {
    tenant_id: Uuid,
    tenant_name: String,
    tenant_slug: String,
    role: String,
    invited_by: Option<Uuid>,
    created_at: DateTime<Utc>,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

async fn invite_member(
    State(state): State<AppState>,
    TenantCtx(ctx): TenantCtx,
    ApiJson(body): ApiJson<InviteRequest>,
) -> Result<impl IntoResponse, AppError> {
    let role = body
        .role
        .parse::<TenantRole>()
        .map_err(|_| AppError::Validation(format!("unknown role '{}'", body.role)))?;

    membership_service::invite_member(&state, &ctx, &body.email, role).await?;

    Ok(StatusCode::ACCEPTED)
}

async fn list_invitations(
    State(state): State<AppState>,
    AuthUser(principal): AuthUser,
) -> Result<Json<Vec<InvitationResponse>>, AppError> {
    let rows = membership_service::list_pending_invitations(&state, principal.user_id).await?;

    Ok(Json(
        rows.into_iter()
            .map(|r| InvitationResponse {
                tenant_id: r.tenant_id,
                tenant_name: r.tenant_name,
                tenant_slug: r.tenant_slug,
                role: r.role,
                invited_by: r.invited_by,
                created_at: r.created_at,
            })
            .collect(),
    ))
}

async fn accept_invitation(
    State(state): State<AppState>,
    AuthUser(principal): AuthUser,
    Path(tenant_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    membership_service::accept_invitation(&state, principal.user_id, tenant_id).await?;
    Ok(StatusCode::NO_CONTENT)
}
