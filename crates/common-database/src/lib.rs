//! Database pool and scoped transaction helpers.
//!
//! Row-level security policies read two transaction settings, `app.current_user_id`
//! and `app.current_tenant_id`. The `begin_*_tx` helpers set them, so every
//! query run on the returned transaction is limited to the right rows.
//!
//! Rule: do no network I/O (Redis, password hashing, S3, LLM calls) while a
//! transaction is open. Commit or drop it first so the connection goes back to the pool.

use common_auth::TenantContext;
use common_config::DatabaseConfig;
use common_errors::AppError;
use sqlx::{postgres::PgPoolOptions, PgPool, Postgres, Transaction};
use std::time::Duration;
use uuid::Uuid;

/// A started transaction that owns its connection.
pub type Tx = Transaction<'static, Postgres>;

/// Creates a PostgreSQL connection pool configured for the application runtime.
pub async fn create_pool(config: &DatabaseConfig) -> Result<PgPool, AppError> {
    PgPoolOptions::new()
        .max_connections(config.max_connections)
        .acquire_timeout(Duration::from_secs(config.acquire_timeout_secs))
        .connect(&config.url)
        .await
        .map_err(|e| AppError::Internal(format!("DB pool creation failed: {e}")))
}

/// Checks database connectivity for health checks without holding a long-lived connection.
pub async fn ping(pool: &PgPool) -> Result<(), AppError> {
    sqlx::query("SELECT 1")
        .execute(pool)
        .await
        .map(|_| ())
        .map_err(|_| AppError::ExternalService("database".into()))
}

/// Starts a transaction scoped to a user (`app.current_user_id` only).
///
/// Use it for things that belong to the caller and not to one tenant: login
/// follow-ups, listing their tenants and invitations, and loading a membership
/// before a `TenantContext` exists.
pub async fn begin_user_tx(pool: &PgPool, user_id: Uuid) -> Result<Tx, AppError> {
    let mut tx = pool.begin().await?;
    set_user_scope(&mut tx, user_id).await?;
    Ok(tx)
}

/// Sets `app.current_user_id` on a transaction that is already open.
///
/// For the rare case where the user is only known after the first query,
/// such as refresh-token rotation. Prefer `begin_user_tx` otherwise.
pub async fn set_user_scope(tx: &mut Tx, user_id: Uuid) -> Result<(), AppError> {
    sqlx::query("SELECT set_config('app.current_user_id', $1, true)")
        .bind(user_id.to_string())
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Starts a transaction scoped to a tenant and a user.
///
/// It takes a `&TenantContext` instead of a raw `Uuid`, so a tenant id from a
/// request cannot be used without first verifying the membership.
pub async fn begin_tenant_tx(pool: &PgPool, ctx: &TenantContext) -> Result<Tx, AppError> {
    let mut tx = pool.begin().await?;

    // `true` makes the settings local to this transaction: they are cleared on
    // commit or rollback, so they never leak to the next user of the connection.
    sqlx::query(
        "SELECT set_config('app.current_tenant_id', $1, true),
                set_config('app.current_user_id', $2, true)",
    )
    .bind(ctx.tenant_id().to_string())
    .bind(ctx.user_id().to_string())
    .execute(&mut *tx)
    .await?;

    Ok(tx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use common_auth::TenantRole;

    #[tokio::test]
    async fn create_pool_fails_gracefully_with_invalid_host() {
        let config = DatabaseConfig {
            url: "postgres://invalid_user:invalid_pwd@127.0.0.1:54321/nonexistent_db".to_string(),
            migration_url: None,
            max_connections: 5,
            acquire_timeout_secs: 1,
        };

        let result = create_pool(&config).await;
        assert!(matches!(result, Err(AppError::Internal(_))));
    }

    // The tests below need a real database. DATABASE_URL comes from the environment
    // or from .env, and the tests are skipped (with a message) when it is unset.
    // The pool has one connection, so every query reuses it and would see a leaked setting.
    async fn test_pool() -> Option<PgPool> {
        dotenvy::dotenv().ok();
        if std::env::var("SKIP_DB_TESTS").as_deref() == Ok("1") {
            eprintln!("skipping: SKIP_DB_TESTS=1 is set");
            return None;
        }
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("skipping: DATABASE_URL is not set");
            return None;
        };
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .expect("DATABASE_URL is set but the database is not reachable");
        Some(pool)
    }

    /// Reads a transaction setting. Unset and cleared both come back as "".
    async fn read_setting<'e>(
        executor: impl sqlx::Executor<'e, Database = Postgres>,
        name: &str,
    ) -> String {
        sqlx::query_scalar::<_, Option<String>>("SELECT current_setting($1, true)")
            .bind(name)
            .fetch_one(executor)
            .await
            .unwrap()
            .unwrap_or_default()
    }

    fn tenant_context() -> TenantContext {
        TenantContext::from_verified_membership(Uuid::new_v4(), Uuid::new_v4(), TenantRole::Member)
    }

    #[tokio::test]
    async fn user_tx_sets_user_id_only() {
        let Some(pool) = test_pool().await else {
            return;
        };
        let user_id = Uuid::new_v4();

        let mut tx = begin_user_tx(&pool, user_id).await.unwrap();

        assert_eq!(
            read_setting(&mut *tx, "app.current_user_id").await,
            user_id.to_string()
        );
        assert_eq!(read_setting(&mut *tx, "app.current_tenant_id").await, "");
    }

    #[tokio::test]
    async fn tenant_tx_sets_tenant_and_user_ids() {
        let Some(pool) = test_pool().await else {
            return;
        };
        let ctx = tenant_context();

        let mut tx = begin_tenant_tx(&pool, &ctx).await.unwrap();

        assert_eq!(
            read_setting(&mut *tx, "app.current_tenant_id").await,
            ctx.tenant_id().to_string()
        );
        assert_eq!(
            read_setting(&mut *tx, "app.current_user_id").await,
            ctx.user_id().to_string()
        );
    }

    #[tokio::test]
    async fn settings_are_cleared_after_commit() {
        let Some(pool) = test_pool().await else {
            return;
        };

        let tx = begin_tenant_tx(&pool, &tenant_context()).await.unwrap();
        tx.commit().await.unwrap();

        assert_eq!(read_setting(&pool, "app.current_tenant_id").await, "");
        assert_eq!(read_setting(&pool, "app.current_user_id").await, "");
    }

    #[tokio::test]
    async fn settings_are_cleared_after_rollback() {
        let Some(pool) = test_pool().await else {
            return;
        };

        let tx = begin_tenant_tx(&pool, &tenant_context()).await.unwrap();
        tx.rollback().await.unwrap();

        assert_eq!(read_setting(&pool, "app.current_tenant_id").await, "");
        assert_eq!(read_setting(&pool, "app.current_user_id").await, "");
    }

    #[tokio::test]
    async fn dropping_an_uncommitted_tx_clears_settings() {
        let Some(pool) = test_pool().await else {
            return;
        };

        let tx = begin_user_tx(&pool, Uuid::new_v4()).await.unwrap();
        drop(tx);

        assert_eq!(read_setting(&pool, "app.current_user_id").await, "");
    }
}
