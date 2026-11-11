//! `/api/v1/tenants` — CRUD for tenants.
//! `/api/v1/tenants/:tenant_id` — read and patch a single tenant.

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use common_errors::{ApiJson, AppError};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    app_state::AppState,
    auth::extractors::{AuthUser, TenantCtx},
    domain::tenant::{Slug, TenantName},
    repositories::tenant_repo::TenantPatch,
    services::tenant_service,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", post(create_tenant).get(list_tenants))
        .route("/{tenant_id}", get(get_tenant).patch(update_tenant))
}

// ── Request / response types ──────────────────────────────────────────────────

#[derive(Deserialize)]
struct CreateTenantRequest {
    name: String,
    slug: String,
}

#[derive(Deserialize)]
struct ListTenantsQuery {
    after: Option<Uuid>,
    #[serde(default = "default_limit")]
    limit: i64,
}

fn default_limit() -> i64 {
    50
}

#[derive(Deserialize)]
struct PatchTenantRequest {
    name: Option<String>,
    slug: Option<String>,
    plan: Option<String>,
    status: Option<String>,
}

#[derive(Serialize)]
struct TenantResponse {
    id: Uuid,
    name: String,
    slug: String,
    plan: String,
    status: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

#[derive(Serialize)]
struct UserTenantResponse {
    tenant_id: Uuid,
    name: String,
    slug: String,
    plan: String,
    status: String,
    role: String,
}

#[derive(Serialize)]
struct TenantDetailResponse {
    id: Uuid,
    name: String,
    slug: String,
    plan: String,
    status: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    /// The calling user's role in this tenant.
    role: String,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

async fn create_tenant(
    State(state): State<AppState>,
    AuthUser(principal): AuthUser,
    ApiJson(body): ApiJson<CreateTenantRequest>,
) -> Result<impl IntoResponse, AppError> {
    let name = TenantName::parse(&body.name)?;
    let slug = Slug::parse(&body.slug)?;

    let tenant =
        tenant_service::create_tenant(&state, principal.user_id, name.as_str(), slug.as_str())
            .await?;

    Ok((
        StatusCode::CREATED,
        Json(TenantResponse {
            id: tenant.id,
            name: tenant.name,
            slug: tenant.slug,
            plan: tenant.plan,
            status: tenant.status,
            created_at: tenant.created_at,
            updated_at: tenant.updated_at,
        }),
    ))
}

async fn list_tenants(
    State(state): State<AppState>,
    AuthUser(principal): AuthUser,
    Query(query): Query<ListTenantsQuery>,
) -> Result<Json<Vec<UserTenantResponse>>, AppError> {
    let limit = query.limit.clamp(1, 100);
    let rows =
        tenant_service::list_user_tenants(&state, principal.user_id, query.after, limit).await?;

    Ok(Json(
        rows.into_iter()
            .map(|r| UserTenantResponse {
                tenant_id: r.tenant_id,
                name: r.name,
                slug: r.slug,
                plan: r.plan,
                status: r.tenant_status,
                role: r.role,
            })
            .collect(),
    ))
}

async fn get_tenant(
    State(state): State<AppState>,
    TenantCtx(ctx): TenantCtx,
) -> Result<Json<TenantDetailResponse>, AppError> {
    let detail = tenant_service::get_tenant(&state, &ctx).await?;

    Ok(Json(TenantDetailResponse {
        id: detail.tenant.id,
        name: detail.tenant.name,
        slug: detail.tenant.slug,
        plan: detail.tenant.plan,
        status: detail.tenant.status,
        created_at: detail.tenant.created_at,
        updated_at: detail.tenant.updated_at,
        role: detail.role,
    }))
}

async fn update_tenant(
    State(state): State<AppState>,
    TenantCtx(ctx): TenantCtx,
    ApiJson(body): ApiJson<PatchTenantRequest>,
) -> Result<Json<TenantResponse>, AppError> {
    // Validate any name/slug that was provided.
    let name = body.name.as_deref().map(TenantName::parse).transpose()?;
    let slug = body.slug.as_deref().map(Slug::parse).transpose()?;

    let patch = TenantPatch {
        name: name.map(|n| n.as_str().to_owned()),
        slug: slug.map(|s| s.as_str().to_owned()),
        plan: body.plan,
        status: body.status,
    };

    let tenant = tenant_service::update_tenant(&state, &ctx, patch).await?;

    Ok(Json(TenantResponse {
        id: tenant.id,
        name: tenant.name,
        slug: tenant.slug,
        plan: tenant.plan,
        status: tenant.status,
        created_at: tenant.created_at,
        updated_at: tenant.updated_at,
    }))
}
