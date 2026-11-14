//! Runs the worker's background loops and keeps them alive.
//!
//! Each loop is wrapped in [`supervise`]: if it returns or panics it is logged
//! and restarted after a short delay, unless shutdown was requested. This is
//! what design §5.3 calls "supervised".

use std::{future::Future, sync::Arc, time::Duration};

use common_messaging::StreamClient;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::{
    app_state::AppState,
    repositories::outbox_repo,
    services::storage_reconciler,
    workers::{ingestion_consumer, outbox_publisher, sweeper},
};

/// How long in-flight work gets to finish after shutdown is requested.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);
const RESTART_DELAY: Duration = Duration::from_secs(1);
const RECONCILE_EVERY: Duration = Duration::from_secs(10 * 60);
const HEARTBEAT_EVERY: Duration = Duration::from_secs(30);
const GROUP_RETRY: Duration = Duration::from_secs(2);

/// `"{hostname}:{pid}:{8 hex}"`. Used as the Redis consumer name and as
/// `ingestion_jobs.locked_by`, so a stuck job can be traced to a process.
pub fn worker_id() -> String {
    let host = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "worker".into());
    let host: String = host
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .take(100)
        .collect();
    let random = Uuid::new_v4().simple().to_string();
    format!("{host}:{}:{}", std::process::id(), &random[..8])
}

/// Runs `make()` in a loop. A task that returns or panics is logged and
/// restarted after `restart_delay`, unless `cancel` fired.
pub async fn supervise_with_delay<F, Fut>(
    name: &'static str,
    cancel: CancellationToken,
    restart_delay: Duration,
    make: F,
) where
    F: Fn() -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    while !cancel.is_cancelled() {
        match tokio::spawn(make()).await {
            Ok(()) => warn!(task = name, "worker task exited, restarting"),
            Err(e) if e.is_panic() => error!(task = name, "worker task panicked, restarting"),
            Err(e) => error!(task = name, error = %e, "worker task failed, restarting"),
        }
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(restart_delay) => {}
        }
    }
}

/// [`supervise_with_delay`] with the standard 1 s restart delay.
pub async fn supervise<F, Fut>(name: &'static str, cancel: CancellationToken, make: F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    supervise_with_delay(name, cancel, RESTART_DELAY, make).await;
}

/// Creates the consumer group, retrying until Redis is reachable. Returns
/// `false` if shutdown came first.
async fn ensure_group(streams: &StreamClient, cancel: &CancellationToken) -> bool {
    loop {
        match streams.ensure_group().await {
            Ok(()) => return true,
            Err(e) => warn!(error = %e, "cannot create consumer group yet, Redis unavailable?"),
        }
        tokio::select! {
            _ = cancel.cancelled() => return false,
            _ = tokio::time::sleep(GROUP_RETRY) => {}
        }
    }
}

/// Runs every loop until `cancel` fires, then waits up to [`SHUTDOWN_GRACE`].
pub async fn run(
    state: AppState,
    streams: Arc<StreamClient>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    let worker_id = worker_id();
    info!(%worker_id, "worker starting");

    if !ensure_group(&streams, &cancel).await {
        return Ok(());
    }

    let mut tasks = JoinSet::new();

    tasks.spawn({
        let (state, streams, cancel) = (state.clone(), streams.clone(), cancel.clone());
        async move {
            let token = cancel.clone();
            supervise("outbox_publisher", cancel, move || {
                outbox_publisher::run(
                    state.db.clone(),
                    streams.clone(),
                    state.config.ingestion.clone(),
                    token.clone(),
                )
            })
            .await;
        }
    });

    tasks.spawn({
        let (state, streams, cancel, worker_id) = (
            state.clone(),
            streams.clone(),
            cancel.clone(),
            worker_id.clone(),
        );
        async move {
            let token = cancel.clone();
            supervise("ingestion_consumer", cancel, move || {
                let deps = Arc::new(crate::services::ingestion_service::IngestionDeps::new(
                    state.clone(),
                    worker_id.clone(),
                ));
                ingestion_consumer::run(deps, streams.clone(), token.clone())
            })
            .await;
        }
    });

    tasks.spawn({
        let (state, cancel) = (state.clone(), cancel.clone());
        async move {
            let token = cancel.clone();
            supervise("sweeper", cancel, move || {
                sweeper::run(state.clone(), token.clone())
            })
            .await;
        }
    });

    tasks.spawn({
        let (state, cancel) = (state.clone(), cancel.clone());
        async move {
            let token = cancel.clone();
            supervise("storage_reconciler", cancel, move || {
                reconcile_loop(state.clone(), token.clone())
            })
            .await;
        }
    });

    tasks.spawn({
        let (state, streams, cancel, worker_id) = (
            state.clone(),
            streams.clone(),
            cancel.clone(),
            worker_id.clone(),
        );
        async move {
            let token = cancel.clone();
            supervise("heartbeat", cancel, move || {
                heartbeat_loop(
                    state.clone(),
                    streams.clone(),
                    worker_id.clone(),
                    token.clone(),
                )
            })
            .await;
        }
    });

    cancel.cancelled().await;
    info!(
        grace_secs = SHUTDOWN_GRACE.as_secs(),
        "shutdown requested, waiting for tasks"
    );
    let drained = tokio::time::timeout(SHUTDOWN_GRACE, async {
        while tasks.join_next().await.is_some() {}
    })
    .await;
    if drained.is_err() {
        warn!("shutdown grace period expired, aborting remaining tasks");
        tasks.abort_all();
    }
    info!("worker stopped");
    Ok(())
}

/// Calls the Phase 3 storage reconciler every 10 minutes (shallow passes only).
async fn reconcile_loop(state: AppState, cancel: CancellationToken) {
    let mut ticker = tokio::time::interval(RECONCILE_EVERY);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = ticker.tick() => {}
        }
        match storage_reconciler::reconcile(&state, false).await {
            Ok(report) => info!(
                retried = report.retried,
                deleted = report.deleted,
                still_failing = report.still_failing,
                finalized = report.finalized,
                "storage reconcile finished"
            ),
            Err(e) => warn!(error = %e, "storage reconcile failed"),
        }
    }
}

/// Oldest unpublished outbox row above this age means the publisher is stuck
/// or Redis is down.
const OUTBOX_AGE_WARN_SECS: f64 = 60.0;
/// Pending (delivered, unacked) entries above `concurrency × this` means
/// consumers are dying or falling behind.
const PENDING_WARN_FACTOR: u64 = 10;

/// What the heartbeat should warn about. Pure, so it is unit-tested.
#[derive(Debug, Default, PartialEq, Eq)]
struct HeartbeatAlerts {
    outbox_lagging: bool,
    stream_backed_up: bool,
}

fn heartbeat_alerts(
    outbox_oldest_age_secs: Option<f64>,
    stream_pending: Option<u64>,
    concurrency: usize,
) -> HeartbeatAlerts {
    let pending_limit = PENDING_WARN_FACTOR * concurrency.max(1) as u64;
    HeartbeatAlerts {
        outbox_lagging: outbox_oldest_age_secs.is_some_and(|age| age > OUTBOX_AGE_WARN_SECS),
        stream_backed_up: stream_pending.is_some_and(|p| p > pending_limit),
    }
}

/// Logs one line every 30 s (`target = "ingestion"`) so an operator can see
/// the worker is alive and how deep the backlogs are. The field names are the
/// metric names Phase 7 will export through OpenTelemetry. A field is absent
/// when its source (PostgreSQL or Redis) could not be read this tick.
async fn heartbeat_loop(
    state: AppState,
    streams: Arc<StreamClient>,
    worker_id: String,
    cancel: CancellationToken,
) {
    let concurrency = state.config.ingestion.worker_concurrency;
    let mut ticker = tokio::time::interval(HEARTBEAT_EVERY);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = ticker.tick() => {}
        }
        let backlog = outbox_repo::backlog(&state.db).await.ok();
        let stats = streams.stats().await.ok();
        let outbox_backlog = backlog.map(|b| b.0);
        let outbox_oldest_age_secs = backlog.and_then(|b| b.1);
        let stream_len = stats.map(|s| s.len);
        let stream_pending = stats.map(|s| s.pending);
        let jobs_in_flight = ingestion_consumer::jobs_in_flight();

        info!(
            target: "ingestion",
            %worker_id,
            outbox_backlog,
            outbox_oldest_age_secs,
            stream_len,
            stream_pending,
            jobs_in_flight,
            db_ok = backlog.is_some(),
            redis_ok = stats.is_some(),
            "worker heartbeat"
        );

        let alerts = heartbeat_alerts(outbox_oldest_age_secs, stream_pending, concurrency);
        if alerts.outbox_lagging {
            warn!(
                target: "ingestion",
                %worker_id,
                outbox_oldest_age_secs,
                "outbox is lagging: is Redis reachable and the publisher running?"
            );
        }
        if alerts.stream_backed_up {
            warn!(
                target: "ingestion",
                %worker_id,
                stream_pending,
                concurrency,
                "many stream entries are delivered but unacknowledged"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn worker_id_has_host_pid_and_8_hex() {
        let id = worker_id();
        let parts: Vec<&str> = id.rsplitn(3, ':').collect();
        assert_eq!(parts.len(), 3, "{id}");
        assert_eq!(parts[0].len(), 8);
        assert!(parts[0].chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(parts[1], std::process::id().to_string());
        assert!(!parts[2].is_empty());
        assert!(id.len() <= 200, "must fit locked_by CHECK");
    }

    #[test]
    fn heartbeat_alerts_thresholds() {
        assert_eq!(heartbeat_alerts(None, None, 4), HeartbeatAlerts::default());
        assert_eq!(
            heartbeat_alerts(Some(60.0), Some(40), 4),
            HeartbeatAlerts::default()
        );
        assert!(heartbeat_alerts(Some(60.5), None, 4).outbox_lagging);
        assert!(heartbeat_alerts(None, Some(41), 4).stream_backed_up);
        // concurrency 0 is treated as 1 so the limit never becomes 0.
        assert!(!heartbeat_alerts(None, Some(10), 0).stream_backed_up);
        assert!(heartbeat_alerts(None, Some(11), 0).stream_backed_up);
    }

    #[tokio::test]
    async fn supervise_restarts_a_panicking_task_then_exits_on_cancel() {
        let starts = Arc::new(AtomicUsize::new(0));
        let cancel = CancellationToken::new();

        let handle = tokio::spawn({
            let (starts, cancel) = (starts.clone(), cancel.clone());
            async move {
                let token = cancel.clone();
                supervise_with_delay("test", cancel, Duration::from_millis(5), move || {
                    let (starts, token) = (starts.clone(), token.clone());
                    async move {
                        let n = starts.fetch_add(1, Ordering::SeqCst);
                        if n < 2 {
                            panic!("boom {n}");
                        }
                        token.cancelled().await;
                    }
                })
                .await;
            }
        });

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while starts.load(Ordering::SeqCst) < 3 {
            assert!(tokio::time::Instant::now() < deadline, "no third start");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("supervise must stop after cancel")
            .unwrap();

        assert_eq!(starts.load(Ordering::SeqCst), 3, "two restarts, no more");
    }

    #[tokio::test]
    async fn supervise_restarts_a_task_that_returns() {
        let starts = Arc::new(AtomicUsize::new(0));
        let cancel = CancellationToken::new();
        let handle = tokio::spawn({
            let (starts, cancel) = (starts.clone(), cancel.clone());
            async move {
                supervise_with_delay("test", cancel, Duration::from_millis(5), move || {
                    let starts = starts.clone();
                    async move {
                        starts.fetch_add(1, Ordering::SeqCst);
                    }
                })
                .await;
            }
        });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while starts.load(Ordering::SeqCst) < 2 {
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .unwrap()
            .unwrap();
    }
}
