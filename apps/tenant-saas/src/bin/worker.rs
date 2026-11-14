//! `tenant-saas-worker`: outbox publisher, ingestion consumer, sweeper and
//! storage reconciler in one process.
//!
//! It never runs migrations. The API (development) or the migration job
//! (production) owns the schema.

use std::sync::Arc;

use common_config::AppConfig;
use common_messaging::{StreamClient, StreamConfig};
use sqlx::PgPool;
use tenant_saas::{app_state::AppState, workers::supervisor};
use tokio_util::sync::CancellationToken;

/// Publisher, sweeper and reconciler each use a connection besides the
/// `WORKER_CONCURRENCY` jobs.
const BACKGROUND_CONNECTIONS: u32 = 3;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = AppConfig::load()?;
    common_telemetry::init_tracing(&config.app.rust_log);

    let needed = config.ingestion.worker_concurrency as u32 + BACKGROUND_CONNECTIONS;
    if config.database.max_connections < needed {
        tracing::warn!(
            max_connections = config.database.max_connections,
            needed,
            "DATABASE_MAX_CONNECTIONS is below WORKER_CONCURRENCY + 3; jobs may wait for a connection"
        );
    }

    let db = common_database::create_pool(&config.database).await?;
    let mut state = AppState::new(db, Arc::new(config.clone()))?;
    if let Some(url) = config.database.migration_url.as_deref() {
        // Only the storage reconciler uses the owner pool.
        match PgPool::connect(url).await {
            Ok(pool) => state = state.with_owner_pool(pool),
            Err(e) => {
                tracing::warn!(error = %e, "owner pool unavailable, reconciler passes 2 and 3 are skipped")
            }
        }
    }

    let streams = Arc::new(StreamClient::new_lazy(StreamConfig::new(
        config.redis.url.clone(),
        config.ingestion.stream.clone(),
        config.ingestion.group.clone(),
    ))?);

    let cancel = CancellationToken::new();
    tokio::spawn(shutdown_signal(cancel.clone()));

    supervisor::run(state, streams, cancel).await
}

/// Cancels `cancel` on Ctrl+C, and on SIGTERM where available.
async fn shutdown_signal(cancel: CancellationToken) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
    tracing::info!("shutdown signal received");
    cancel.cancel();
}
