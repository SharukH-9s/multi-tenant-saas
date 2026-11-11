//! Register, login, refresh, logout and "who am I".
//!
//! Two rules shape this file:
//! - No hashing or other slow work while a transaction is open, so connections
//!   go back to the pool quickly.
//! - A transaction is committed *before* returning an error when the error path
//!   wrote something (revoked tokens). Returning early would roll that back.
//!
//! Audit events use `target = "audit"` and carry ids only: never emails,
//! passwords, tokens or hashes.

use common_auth::{
    generate_refresh_token, hash_refresh_token, AuthenticatedPrincipal, RefreshToken,
};
use common_database::{begin_user_tx, set_user_scope};
use common_errors::AppError;
use uuid::Uuid;

use crate::{
    app_state::AppState,
    domain::user::{Email, Password},
    repositories::{
        refresh_token_repo,
        user_repo::{self, MembershipSummary, UserProfile, UserRecord},
    },
};

/// Tokens returned by login and refresh.
pub struct TokenPair {
    pub access_token: String,
    pub expires_in_secs: u64,
    pub refresh_token: String,
}

/// The caller's profile and the tenants they belong to.
pub struct Me {
    pub user: UserProfile,
    pub memberships: Vec<MembershipSummary>,
}

const ACTIVE: &str = "active";

pub async fn register(
    state: &AppState,
    email: &str,
    password: String,
) -> Result<UserRecord, AppError> {
    let email = Email::parse(email)?;
    let password = Password::parse(password)?;

    // Hash first, with no transaction open.
    let password_hash = state.password_service.hash(password.as_str()).await?;

    // The app picks the id so the transaction can be scoped to it. That lets
    // RLS allow the insert's RETURNING to read the new row back.
    let user_id = Uuid::new_v4();
    let mut tx = begin_user_tx(&state.db, user_id).await?;
    let user = user_repo::insert(&mut tx, user_id, email.as_str(), &password_hash).await?;
    tx.commit().await?;

    tracing::info!(target: "audit", event = "user.registered", user_id = %user.id);
    Ok(user)
}

pub async fn login(state: &AppState, email: &str, password: &str) -> Result<TokenPair, AppError> {
    // An unparseable email is treated like an unknown one, so the response
    // (and the work done) does not reveal which check failed.
    let credentials = match Email::parse(email) {
        Ok(email) => user_repo::find_credentials(&state.db, email.as_str()).await?,
        Err(_) => None,
    };

    // Always runs an Argon2 verify; with `None` it checks a dummy hash.
    let stored_hash = credentials
        .as_ref()
        .and_then(|c| c.password_hash.as_deref());
    let password_ok = state.password_service.verify(password, stored_hash).await?;

    match credentials {
        Some(user) if password_ok && user.status == ACTIVE => {
            let tokens = start_session(state, user.id).await?;
            tracing::info!(target: "audit", event = "auth.login", user_id = %user.id);
            Ok(tokens)
        }
        other => {
            // Unknown email, wrong password and disabled account all look the same.
            let user_id = other.map(|c| c.id);
            tracing::warn!(target: "audit", event = "auth.login_failed", user_id = ?user_id);
            Err(AppError::InvalidCredentials)
        }
    }
}

/// Swaps a refresh token for a new access + refresh token (rotation).
///
/// A token can be used once. Presenting an already-used token means it was
/// copied, so every token in its family is revoked. Two simultaneous requests
/// with the same token therefore count as reuse too: strict by design.
pub async fn refresh(state: &AppState, presented_token: &str) -> Result<TokenPair, AppError> {
    let token_hash = hash_refresh_token(presented_token);
    let mut tx = state.db.begin().await?;

    let Some(owner) = refresh_token_repo::rotate(&mut *tx, &token_hash).await? else {
        if let Some(reused) = refresh_token_repo::find_rotated(&mut *tx, &token_hash).await? {
            refresh_token_repo::revoke_family(&mut *tx, reused.family_id).await?;
            tx.commit().await?;
            tracing::warn!(
                target: "audit",
                event = "auth.refresh_reuse_detected",
                user_id = %reused.user_id,
                family_id = %reused.family_id
            );
        }
        // Unknown, expired and revoked tokens end here too.
        return Err(AppError::Unauthorized);
    };

    // The user is only known now, so scope the open transaction to them.
    set_user_scope(&mut tx, owner.user_id).await?;
    let user = user_repo::find_by_id(&mut tx, owner.user_id).await?;
    if !user.is_some_and(|u| u.status == ACTIVE) {
        refresh_token_repo::revoke_family(&mut *tx, owner.family_id).await?;
        tx.commit().await?;
        return Err(AppError::Unauthorized);
    }

    let next = generate_refresh_token();
    refresh_token_repo::insert(
        &mut *tx,
        owner.user_id,
        owner.family_id,
        &next.hash,
        state.config.auth.refresh_token_ttl_secs,
    )
    .await?;
    tx.commit().await?;

    build_pair(state, owner.user_id, next)
}

/// Revokes the session behind `presented_token`. Always succeeds, so callers
/// cannot learn whether a token existed or who owned it.
pub async fn logout(
    state: &AppState,
    principal: AuthenticatedPrincipal,
    presented_token: &str,
) -> Result<(), AppError> {
    let token_hash = hash_refresh_token(presented_token);
    refresh_token_repo::revoke_family_of_token(&state.db, &token_hash, principal.user_id).await?;

    tracing::info!(target: "audit", event = "auth.logout", user_id = %principal.user_id);
    Ok(())
}

pub async fn me(state: &AppState, principal: AuthenticatedPrincipal) -> Result<Me, AppError> {
    let mut tx = begin_user_tx(&state.db, principal.user_id).await?;

    // A valid token for a deleted or disabled user stops working here.
    let user = user_repo::find_by_id(&mut tx, principal.user_id)
        .await?
        .filter(|u| u.status == ACTIVE)
        .ok_or(AppError::Unauthorized)?;
    let memberships = user_repo::list_active_memberships(&mut tx, principal.user_id).await?;
    tx.commit().await?;

    Ok(Me { user, memberships })
}

/// New login session: a fresh refresh-token family plus an access token.
async fn start_session(state: &AppState, user_id: Uuid) -> Result<TokenPair, AppError> {
    let refresh = generate_refresh_token();
    refresh_token_repo::insert(
        &state.db,
        user_id,
        Uuid::new_v4(),
        &refresh.hash,
        state.config.auth.refresh_token_ttl_secs,
    )
    .await?;

    build_pair(state, user_id, refresh)
}

fn build_pair(
    state: &AppState,
    user_id: Uuid,
    refresh: RefreshToken,
) -> Result<TokenPair, AppError> {
    let access = state.token_service.issue_access(user_id)?;
    Ok(TokenPair {
        access_token: access.token,
        expires_in_secs: access.expires_in_secs,
        refresh_token: refresh.plain,
    })
}
