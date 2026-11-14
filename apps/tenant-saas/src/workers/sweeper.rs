//! Sweeper: the single recovery path for ingestion jobs (plan D1).
//!
//! Redis only rings a doorbell. Every decision about what work is left comes
//! from `ingestion_jobs`, and this loop is what moves stuck jobs along:
//! expired leases, due retries, lost events, and jobs of deleted documents.
//! The rules themselves live in the `sweep_ingestion_jobs` SQL function.

use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::{
    app_state::AppState,
    repositories::{
        ingestion_repo::{self, SweptJob},
        outbox_repo,
    },
};

const SWEEP_BATCH: i32 = 500;
const PURGE_AFTER_DAYS: i32 = 7;
const PURGE_BATCH: i64 = 1000;

pub async fn run(state: AppState, cancel: CancellationToken) {
    let cfg = &state.config.ingestion;
    let stale_secs = i32::try_from(cfg.job_lease_secs).unwrap_or(i32::MAX);

    let mut ticker = tokio::time::interval(Duration::from_secs(cfg.sweep_interval_secs.max(1)));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = ticker.tick() => {}
        }

        match ingestion_repo::sweep(&state.db, SWEEP_BATCH, stale_secs).await {
            Ok(swept) => log_sweep(&swept),
            Err(e) => warn!(error = %e, "ingestion sweep failed"),
        }

        match outbox_repo::purge_published(&state.db, PURGE_AFTER_DAYS, PURGE_BATCH).await {
            Ok(0) => {}
            Ok(purged) => info!(purged, "purged published outbox rows"),
            Err(e) => warn!(error = %e, "outbox purge failed"),
        }
    }
}

fn log_sweep(swept: &[SweptJob]) {
    if swept.is_empty() {
        return;
    }
    let count = |action: &str| swept.iter().filter(|j| j.action == action).count();
    let (requeued, cancelled, failed) = (count("requeued"), count("cancelled"), count("failed"));

    if failed > 0 {
        warn!(
            requeued,
            cancelled, failed, "ingestion sweep gave up on jobs"
        );
    } else {
        info!(requeued, cancelled, failed, "ingestion sweep");
    }
}
