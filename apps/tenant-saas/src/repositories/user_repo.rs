use chrono::{DateTime, Utc};
use common_database::Tx;
use common_errors::AppError;
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use super::db_error::map_db_error;

// `users.email` is `citext`, which sqlx cannot decode directly. Queries cast it
// to text on the way out (`email::text`) and to citext on the way in.

#[derive(Debug, FromRow)]
pub struct UserRecord {
    pub id: Uuid,
    pub email: String,
    pub created_at: DateTime<Utc>,
}

/// What login needs. Only `auth_get_credentials()` can read `password_hash`.
#[derive(FromRow)]
pub struct Credentials {
    pub id: Uuid,
    pub password_hash: Option<String>,
    pub status: String,
}

#[derive(Debug, FromRow)]
pub struct UserProfile {
    pub id: Uuid,
    pub email: String,
    pub status: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, FromRow)]
pub struct MembershipSummary {
    pub tenant_id: Uuid,
    pub slug: String,
    pub name: String,
    pub role: String,
}

/// Registration. The transaction must be scoped to `id` (`begin_user_tx(id)`),
/// otherwise RLS hides the row from `RETURNING`.
pub async fn insert(
    tx: &mut Tx,
    id: Uuid,
    email: &str,
    password_hash: &str,
) -> Result<UserRecord, AppError> {
    sqlx::query_as::<_, UserRecord>(
        "INSERT INTO users (id, email, password_hash, status)
         VALUES ($1, $2::citext, $3, 'active')
         RETURNING id, email::text AS email, created_at",
    )
    .bind(id)
    .bind(email)
    .bind(password_hash)
    .fetch_one(&mut **tx)
    .await
    .map_err(map_db_error)
}

/// Login lookup through the SECURITY DEFINER function. No user scope needed.
pub async fn find_credentials(pool: &PgPool, email: &str) -> Result<Option<Credentials>, AppError> {
    Ok(sqlx::query_as::<_, Credentials>(
        "SELECT id, password_hash, status FROM auth_get_credentials($1::citext)",
    )
    .bind(email)
    .fetch_optional(pool)
    .await?)
}

/// The caller's own row (RLS only lets a user see themselves).
pub async fn find_by_id(tx: &mut Tx, id: Uuid) -> Result<Option<UserProfile>, AppError> {
    Ok(sqlx::query_as::<_, UserProfile>(
        "SELECT id, email::text AS email, status, created_at FROM users WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?)
}

/// Active memberships in active tenants, for a user-scoped transaction.
pub async fn list_active_memberships(
    tx: &mut Tx,
    user_id: Uuid,
) -> Result<Vec<MembershipSummary>, AppError> {
    Ok(sqlx::query_as::<_, MembershipSummary>(
        "SELECT t.id AS tenant_id, t.slug, t.name, m.role
           FROM memberships m
           JOIN tenants t ON t.id = m.tenant_id
          WHERE m.user_id = $1 AND m.status = 'active' AND t.status = 'active'
          ORDER BY t.name, t.id",
    )
    .bind(user_id)
    .fetch_all(&mut **tx)
    .await?)
}
