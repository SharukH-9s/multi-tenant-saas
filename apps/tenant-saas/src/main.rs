mod app_state;
mod handlers;
mod routes;

use std::sync::Arc;

use anyhow::Context;
use app_state::AppState;
use common_config::AppConfig;
use sqlx::PgPool;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = AppConfig::load()?;
    common_telemetry::init_tracing(&config.app.rust_log);

    // Migrations run in-process only in development. In production a separate
    // job runs them first, so replicas never race on the schema.
    if config.app.env == "development" {
        run_migrations(&config).await?;
    } else {
        tracing::info!("Skipping in-process migrations; run the migration job separately");
    }

    let db = common_database::create_pool(&config.database).await?;

    let addr = format!("{}:{}", config.app.host, config.app.port);
    let state = AppState {
        db,
        config: Arc::new(config),
    };

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(address = %addr, "API listening");
    axum::serve(listener, routes::build_router(state)).await?;

    Ok(())
}

/// Migrations need DDL rights, so they use the `db_owner` account, never `app_runtime`.
async fn run_migrations(config: &AppConfig) -> anyhow::Result<()> {
    let url = config
        .database
        .migration_url
        .as_deref()
        .context("DATABASE_MIGRATION_URL is required to run migrations in development")?;

    tracing::info!("Running migrations (development mode)");
    let pool = PgPool::connect(url).await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    pool.close().await;

    Ok(())
}
