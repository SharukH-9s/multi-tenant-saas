use common_config::DatabaseConfig;
use common_errors::AppError;
use sqlx::{postgres::PgPoolOptions, PgPool};
use std::time::Duration;

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
