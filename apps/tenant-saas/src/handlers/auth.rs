//! `/api/v1/auth/*`. Handlers only translate HTTP to and from `auth_service`.

use axum::{
    extract::{DefaultBodyLimit, State},
    http::{header::CACHE_CONTROL, StatusCode},
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
    auth::{client_ip::ClientIp, extractors::AuthUser},
    services::auth_service::{self, TokenPair},
};

/// Auth bodies are tiny (an email and a password).
const MAX_BODY_BYTES: usize = 16 * 1024;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/register", post(register))
        .route("/login", post(login))
        .route("/refresh", post(refresh))
        .route("/logout", post(logout))
        .route("/me", get(me))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
}

// Request types have no `Debug` on purpose: they hold passwords and tokens.

#[derive(Deserialize)]
struct CredentialsRequest {
    email: String,
    password: String,
}

#[derive(Deserialize)]
struct RefreshRequest {
    refresh_token: String,
}

#[derive(Serialize)]
struct RegisteredUser {
    id: Uuid,
    email: String,
    created_at: DateTime<Utc>,
}

#[derive(Serialize)]
struct TokenResponse {
    access_token: String,
    token_type: &'static str,
    expires_in: u64,
    refresh_token: String,
}

impl From<TokenPair> for TokenResponse {
    fn from(pair: TokenPair) -> Self {
        Self {
            access_token: pair.access_token,
            token_type: "Bearer",
            expires_in: pair.expires_in_secs,
            refresh_token: pair.refresh_token,
        }
    }
}

#[derive(Serialize)]
struct MeResponse {
    id: Uuid,
    email: String,
    created_at: DateTime<Utc>,
    memberships: Vec<MembershipResponse>,
}

#[derive(Serialize)]
struct MembershipResponse {
    tenant_id: Uuid,
    slug: String,
    name: String,
    role: String,
}

/// Tokens must never be stored by browsers or proxies.
fn no_store<T: IntoResponse>(response: T) -> impl IntoResponse {
    ([(CACHE_CONTROL, "no-store")], response)
}

/// Rate-limit check is the first thing done: a blocked request never reaches
/// Argon2, the semaphore, or a database transaction.
async fn register(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    ApiJson(body): ApiJson<CredentialsRequest>,
) -> Result<impl IntoResponse, AppError> {
    state
        .rate_limiter
        .check("register", &ip, Some(&body.email))
        .await?;

    let user = auth_service::register(&state, &body.email, body.password).await?;

    Ok((
        StatusCode::CREATED,
        Json(RegisteredUser {
            id: user.id,
            email: user.email,
            created_at: user.created_at,
        }),
    ))
}

async fn login(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    ApiJson(body): ApiJson<CredentialsRequest>,
) -> Result<impl IntoResponse, AppError> {
    state
        .rate_limiter
        .check("login", &ip, Some(&body.email))
        .await?;

    let tokens = auth_service::login(&state, &body.email, &body.password).await?;
    Ok(no_store(Json(TokenResponse::from(tokens))))
}

async fn refresh(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    ApiJson(body): ApiJson<RefreshRequest>,
) -> Result<impl IntoResponse, AppError> {
    // No email in the refresh request, so only the IP bucket is checked.
    state.rate_limiter.check("refresh", &ip, None).await?;

    let tokens = auth_service::refresh(&state, &body.refresh_token).await?;
    Ok(no_store(Json(TokenResponse::from(tokens))))
}

async fn logout(
    State(state): State<AppState>,
    AuthUser(principal): AuthUser,
    ApiJson(body): ApiJson<RefreshRequest>,
) -> Result<StatusCode, AppError> {
    auth_service::logout(&state, principal, &body.refresh_token).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn me(
    State(state): State<AppState>,
    AuthUser(principal): AuthUser,
) -> Result<Json<MeResponse>, AppError> {
    let me = auth_service::me(&state, principal).await?;

    Ok(Json(MeResponse {
        id: me.user.id,
        email: me.user.email,
        created_at: me.user.created_at,
        memberships: me
            .memberships
            .into_iter()
            .map(|m| MembershipResponse {
                tenant_id: m.tenant_id,
                slug: m.slug,
                name: m.name,
                role: m.role,
            })
            .collect(),
    }))
}
