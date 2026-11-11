use common_errors::AppError;
use sqlx::{FromRow, PgExecutor};
use uuid::Uuid;

/// Owner and family of a refresh token.
#[derive(Debug, FromRow)]
pub struct TokenOwner {
    pub user_id: Uuid,
    pub family_id: Uuid,
}

// Only the SHA-256 hash of a token is ever stored or passed around here.
// Every function takes a generic executor so it works on a pool or inside a transaction.

pub async fn insert<'e>(
    executor: impl PgExecutor<'e>,
    user_id: Uuid,
    family_id: Uuid,
    token_hash: &[u8; 32],
    ttl_secs: u64,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO refresh_tokens (user_id, family_id, token_hash, expires_at)
         VALUES ($1, $2, $3, now() + make_interval(secs => $4))",
    )
    .bind(user_id)
    .bind(family_id)
    .bind(&token_hash[..])
    .bind(ttl_secs as f64)
    .execute(executor)
    .await?;
    Ok(())
}

/// Marks a token as used, but only if it is still usable. One atomic statement,
/// so of two concurrent requests with the same token only one can win.
pub async fn rotate<'e>(
    executor: impl PgExecutor<'e>,
    token_hash: &[u8; 32],
) -> Result<Option<TokenOwner>, AppError> {
    Ok(sqlx::query_as::<_, TokenOwner>(
        "UPDATE refresh_tokens
            SET rotated_at = now()
          WHERE token_hash = $1
            AND rotated_at IS NULL
            AND revoked_at IS NULL
            AND expires_at > now()
        RETURNING user_id, family_id",
    )
    .bind(&token_hash[..])
    .fetch_optional(executor)
    .await?)
}

/// Finds a token that was already rotated. Presenting one again means it was
/// copied, so the caller revokes the whole family.
pub async fn find_rotated<'e>(
    executor: impl PgExecutor<'e>,
    token_hash: &[u8; 32],
) -> Result<Option<TokenOwner>, AppError> {
    Ok(sqlx::query_as::<_, TokenOwner>(
        "SELECT user_id, family_id FROM refresh_tokens
          WHERE token_hash = $1 AND rotated_at IS NOT NULL",
    )
    .bind(&token_hash[..])
    .fetch_optional(executor)
    .await?)
}

pub async fn revoke_family<'e>(
    executor: impl PgExecutor<'e>,
    family_id: Uuid,
) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE refresh_tokens SET revoked_at = now()
          WHERE family_id = $1 AND revoked_at IS NULL",
    )
    .bind(family_id)
    .execute(executor)
    .await?;
    Ok(())
}

/// Logout: revokes the family of this token, but only if it belongs to `user_id`.
/// Unknown tokens and other users' tokens do nothing.
pub async fn revoke_family_of_token<'e>(
    executor: impl PgExecutor<'e>,
    token_hash: &[u8; 32],
    user_id: Uuid,
) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE refresh_tokens SET revoked_at = now()
          WHERE revoked_at IS NULL
            AND family_id IN (SELECT family_id FROM refresh_tokens
                               WHERE token_hash = $1 AND user_id = $2)",
    )
    .bind(&token_hash[..])
    .bind(user_id)
    .execute(executor)
    .await?;
    Ok(())
}
