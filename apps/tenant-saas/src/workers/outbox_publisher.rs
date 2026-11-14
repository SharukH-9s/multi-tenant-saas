//! Outbox publisher: moves `outbox_events` rows to the Redis stream.
//!
//! One cycle is claim, publish, mark. The claim is a single auto-committed
//! statement, so no transaction or row lock is open while `XADD` runs
//! (ADR-007 §1). A crash between `XADD` and the mark leaves rows claimed but
//! unpublished; the claim expires and the rows are published again. Consumers
//! tolerate that duplicate.

use std::{sync::Arc, time::Duration};

use common_config::IngestionConfig;
use common_errors::AppError;
use common_messaging::{IngestionEvent, StreamClient, StreamError, INGESTION_REQUESTED};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::repositories::outbox_repo::{self, OutboxRow};

const BACKOFF_START: Duration = Duration::from_millis(200);
const BACKOFF_MAX: Duration = Duration::from_secs(5);

/// What one `publish_batch` call did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PublishReport {
    pub published: usize,
    pub dead_lettered: usize,
    pub released: usize,
}

impl PublishReport {
    /// Rows this cycle claimed and resolved one way or the other.
    pub fn handled(&self) -> usize {
        self.published + self.dead_lettered + self.released
    }
}

#[derive(Debug)]
pub enum PublisherError {
    /// Redis could not be reached. Unpublished rows were released.
    RedisUnavailable(String),
    /// Redis answered with something unexpected. Unpublished rows were released.
    Redis(String),
    Database(AppError),
}

impl std::fmt::Display for PublisherError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RedisUnavailable(e) => write!(f, "redis unavailable: {e}"),
            Self::Redis(e) => write!(f, "redis error: {e}"),
            Self::Database(e) => write!(f, "database error: {e}"),
        }
    }
}

impl std::error::Error for PublisherError {}

impl From<AppError> for PublisherError {
    fn from(error: AppError) -> Self {
        Self::Database(error)
    }
}

/// Validates a row's payload. Anything that can never become a valid stream
/// event is reported as an error string for the dead-letter column.
fn parse_row(row: &OutboxRow) -> Result<IngestionEvent, String> {
    if row.event_type != INGESTION_REQUESTED {
        return Err(format!("unknown event_type '{}'", row.event_type));
    }
    IngestionEvent::parse(&row.payload.to_string()).map_err(|e| e.to_string())
}

/// One claim, publish, mark cycle.
///
/// For each claimed row, in order:
/// 1. A payload that does not parse is dead-lettered and skipped.
/// 2. On a Redis failure this row and every remaining one are released, rows
///    already published are still marked, and the error is returned.
/// 3. Successes are marked with one `mark_published` call at the end.
pub async fn publish_batch(
    pool: &PgPool,
    streams: &StreamClient,
    cfg: &IngestionConfig,
) -> Result<PublishReport, PublisherError> {
    let rows =
        outbox_repo::claim_batch(pool, cfg.outbox_batch as i64, cfg.outbox_claim_secs).await?;
    let mut report = PublishReport::default();
    let mut published: Vec<Uuid> = Vec::with_capacity(rows.len());
    let mut failure: Option<PublisherError> = None;
    let mut to_release: Vec<Uuid> = Vec::new();

    for row in &rows {
        if failure.is_some() {
            to_release.push(row.id);
            continue;
        }
        let event = match parse_row(row) {
            Ok(event) => event,
            Err(reason) => {
                warn!(outbox_id = %row.id, %reason, "dead-lettering unpublishable outbox event");
                outbox_repo::dead_letter(pool, row.id, &reason).await?;
                report.dead_lettered += 1;
                continue;
            }
        };
        match streams.publish(&event, row.id).await {
            Ok(_) => published.push(row.id),
            Err(e) => {
                to_release.push(row.id);
                failure = Some(match e {
                    StreamError::Unavailable(m) => PublisherError::RedisUnavailable(m),
                    StreamError::Protocol(m) => PublisherError::Redis(m),
                });
            }
        }
    }

    // Mark first: those events are already in the stream.
    let mark = outbox_repo::mark_published(pool, &published).await;
    report.published = published.len();
    if let Err(e) = outbox_repo::release(pool, &to_release).await {
        // The claims expire on their own, so this only delays the retry.
        warn!(error = %e, "failed to release outbox claims");
    }
    report.released = to_release.len();

    mark?;
    match failure {
        Some(e) => Err(e),
        None => Ok(report),
    }
}

/// Loops `publish_batch` until `cancel` fires. Sleeps `outbox_poll_ms` when a
/// cycle found nothing, loops at once when it did work, and backs off from
/// 200 ms to 5 s while Redis is unavailable.
pub async fn run(
    pool: PgPool,
    streams: Arc<StreamClient>,
    cfg: IngestionConfig,
    cancel: CancellationToken,
) {
    let poll = Duration::from_millis(cfg.outbox_poll_ms);
    let mut backoff = BACKOFF_START;
    info!("outbox publisher started");

    while !cancel.is_cancelled() {
        let delay = match publish_batch(&pool, &streams, &cfg).await {
            Ok(report) => {
                backoff = BACKOFF_START;
                if report.handled() == 0 {
                    Some(poll)
                } else {
                    None
                }
            }
            Err(PublisherError::RedisUnavailable(reason)) => {
                warn!(%reason, retry_in_ms = backoff.as_millis() as u64, "redis unavailable, outbox publishing paused");
                let delay = backoff;
                backoff = (backoff * 2).min(BACKOFF_MAX);
                Some(delay)
            }
            Err(e) => {
                error!(error = %e, "outbox publish cycle failed");
                let delay = backoff;
                backoff = (backoff * 2).min(BACKOFF_MAX);
                Some(delay)
            }
        };
        if let Some(delay) = delay {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(delay) => {}
            }
        }
    }
    info!("outbox publisher stopped");
}
