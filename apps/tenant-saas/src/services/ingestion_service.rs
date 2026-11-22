//! The ingestion pipeline: claim, download, parse, chunk, embed, and persist.
//!
//! Follows the micro-scoped transaction pattern (plan §7.3.1, ADR-007):
//! 1. Short tx: claim.
//! 2. No tx: S3 download, parser, chunker, embedder.
//! 3. Short tx: persist chunks and update document status.
//!
//! Layout: [`process`] claims the job, runs [`run_pipeline`], and turns the
//! result into a [`ProcessOutcome`]. Each pipeline stage is its own small
//! function that returns `Result<_, Stop>`, so the stages read top to bottom
//! with `?` and failure handling lives in exactly one place.
//!
//! No log line ever contains document text, file names, or object keys.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use bytes::Bytes;
use chrono::{DateTime, Utc};
use common_database::{begin_system_tenant_tx, Tx};
use common_errors::AppError;
use common_messaging::IngestionEvent;
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;
use tracing::{info, warn};

use crate::{
    ai::{embed_in_batches, EmbeddingError},
    app_state::AppState,
    domain::ingestion::{retry_delay, FailureCode, FailureKind},
    ingestion::{
        chunker::{Chunk, ChunkError, Chunker, ChunkerConfig, EMBEDDING_VERSION},
        parser::{parse, ParseError, ParseLimits, ParsedDocument},
    },
    repositories::{
        chunk_repo::NewChunk,
        ingestion_repo::{self, ClaimResult, ClaimedJob},
    },
};

pub struct IngestionDeps {
    pub state: AppState,
    pub chunker: Arc<Chunker>,
    pub parse_slots: Arc<Semaphore>,
    pub worker_id: String,
}

impl IngestionDeps {
    pub fn new(state: AppState, worker_id: String) -> Self {
        let parse_concurrency = (state.config.ingestion.worker_concurrency / 2).max(1);
        let parse_slots = Arc::new(Semaphore::new(parse_concurrency));

        let bpe = Arc::new(
            crate::ingestion::chunker::tokenizer_for_model(&state.config.ingestion.embedding_model)
                .expect("valid tokenizer for embedding model"),
        );
        let chunker_cfg = ChunkerConfig {
            target_tokens: state.config.ingestion.chunk_target_tokens,
            overlap_tokens: state.config.ingestion.chunk_overlap_tokens,
            max_chunks: state.config.ingestion.max_chunks_per_doc,
        };
        let chunker = Arc::new(Chunker::new(bpe, chunker_cfg));

        Self {
            state,
            chunker,
            parse_slots,
            worker_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessOutcome {
    Succeeded { chunks: usize },
    RetryScheduled { at: DateTime<Utc> },
    Failed { code: FailureCode },
    Cancelled,
    Skipped,
    Superseded,
    Poison,
    Unavailable,
}

impl ProcessOutcome {
    /// Whether the Redis Streams delivery should be acknowledged (D7 policy).
    pub fn should_ack(&self) -> bool {
        match self {
            Self::Succeeded { .. }
            | Self::RetryScheduled { .. }
            | Self::Failed { .. }
            | Self::Cancelled
            | Self::Skipped
            | Self::Superseded
            | Self::Poison => true,
            Self::Unavailable => false,
        }
    }
}

/// Why a pipeline stage stopped. Stages return this as the `Err` side so that
/// `process` is the only place that decides what each stop means.
enum Stop {
    /// The document cannot be ingested. `detail` is fixed text we wrote, never
    /// document content, a file name or an object key. `retry_after` is the
    /// provider's own back-off hint, if it sent one.
    Failed {
        code: FailureCode,
        detail: String,
        retry_after: Option<Duration>,
    },
    /// Another worker took over the job, or the document was deleted.
    Superseded,
    /// The database is unreachable. The job is left for redelivery or the sweeper.
    Unavailable,
}

fn failed(code: FailureCode, detail: impl Into<String>) -> Stop {
    Stop::Failed {
        code,
        detail: detail.into(),
        retry_after: None,
    }
}

fn log_step(step: &str, started: Instant) {
    info!(
        step,
        duration_ms = started.elapsed().as_millis() as u64,
        "step completed"
    );
}

/// Executes the ingestion pipeline for an event.
#[tracing::instrument(
    skip(deps),
    fields(
        job_id = %event.job_id,
        tenant_id = %event.tenant_id,
        document_id = %event.document_id,
        attempt
    )
)]
pub async fn process(deps: &IngestionDeps, event: &IngestionEvent) -> ProcessOutcome {
    let started = Instant::now();
    let outcome = run(deps, event).await;
    info!(
        outcome = ?outcome,
        duration_ms = started.elapsed().as_millis() as u64,
        "ingestion job processed"
    );
    outcome
}

async fn run(deps: &IngestionDeps, event: &IngestionEvent) -> ProcessOutcome {
    let claimed = match claim(deps, event).await {
        Ok(claimed) => claimed,
        Err(outcome) => return outcome,
    };

    match run_pipeline(deps, &claimed).await {
        Ok(chunks) => ProcessOutcome::Succeeded { chunks },
        Err(Stop::Failed {
            code,
            detail,
            retry_after,
        }) => record_failure(deps, &claimed, code, &detail, retry_after).await,
        Err(Stop::Superseded) => ProcessOutcome::Superseded,
        Err(Stop::Unavailable) => ProcessOutcome::Unavailable,
    }
}

/// Download, parse, chunk, embed, persist. Returns the chunk count.
async fn run_pipeline(deps: &IngestionDeps, claimed: &ClaimedJob) -> Result<usize, Stop> {
    let bytes = download(deps, claimed).await?;
    let document = parse_document(deps, claimed, bytes).await?;
    let chunks = chunk_document(deps, document).await?;
    let vectors = embed(deps, claimed, &chunks).await?;
    persist(deps, claimed, &chunks, &vectors).await?;
    Ok(chunks.len())
}

// ---------------------------------------------------------------------------
// Step 1: claim (short tx A)
// ---------------------------------------------------------------------------

/// `Err` carries the final outcome when there is nothing to process.
async fn claim(deps: &IngestionDeps, event: &IngestionEvent) -> Result<ClaimedJob, ProcessOutcome> {
    let started = Instant::now();
    let mut tx = begin_system_tenant_tx(&deps.state.db, event.tenant_id)
        .await
        .map_err(|e| {
            warn!(error = %e, "cannot open system tenant tx for claim");
            ProcessOutcome::Unavailable
        })?;

    let result = ingestion_repo::claim(
        &mut tx,
        event.job_id,
        event.document_id,
        &deps.worker_id,
        deps.state.config.ingestion.job_lease_secs,
    )
    .await;

    // Commit when the claim changed something (claimed, or cancelled because the
    // document is gone). Roll back otherwise.
    let (commit, outcome) = match result {
        Ok(ClaimResult::Claimed(claimed)) => {
            tracing::Span::current().record("attempt", claimed.attempts);
            (true, Ok(claimed))
        }
        Ok(ClaimResult::DocumentGone) => (true, Err(ProcessOutcome::Cancelled)),
        Ok(ClaimResult::NotClaimable) => (false, Err(ProcessOutcome::Skipped)),
        Ok(ClaimResult::Mismatch) => {
            warn!("cross-tenant mismatch in ingestion event");
            (false, Err(ProcessOutcome::Poison))
        }
        Err(e) => {
            warn!(error = %e, "claim query failed");
            (false, Err(ProcessOutcome::Unavailable))
        }
    };

    if commit {
        tx.commit().await.map_err(|e| {
            warn!(error = %e, "failed to commit claim transaction");
            ProcessOutcome::Unavailable
        })?;
    } else {
        let _ = tx.rollback().await;
    }

    log_step("claim", started);
    outcome
}

// ---------------------------------------------------------------------------
// Step 2: download and verify (no tx)
// ---------------------------------------------------------------------------

async fn download(deps: &IngestionDeps, claimed: &ClaimedJob) -> Result<Bytes, Stop> {
    let started = Instant::now();

    if claimed.size_bytes < 0
        || claimed.size_bytes as u64 > deps.state.config.storage.max_upload_bytes
    {
        return Err(failed(
            FailureCode::DocumentTooLarge,
            "document exceeds max upload size",
        ));
    }

    let bytes = deps
        .state
        .storage
        .get(&claimed.object_key)
        .await
        .map_err(|e| match e {
            common_storage::StorageError::NotFound => {
                failed(FailureCode::ObjectMissing, "storage object not found")
            }
            _ => failed(
                FailureCode::StorageUnavailable,
                "storage backend unavailable",
            ),
        })?;

    if bytes.len() as i64 != claimed.size_bytes {
        return Err(failed(FailureCode::ObjectCorrupt, "object size mismatch"));
    }
    let hash: String = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if hash != claimed.content_hash {
        return Err(failed(FailureCode::ObjectCorrupt, "content hash mismatch"));
    }

    log_step("download", started);
    Ok(bytes)
}

// ---------------------------------------------------------------------------
// Step 3: parse (spawn_blocking, bounded by `parse_slots` and a timeout)
// ---------------------------------------------------------------------------

async fn parse_document(
    deps: &IngestionDeps,
    claimed: &ClaimedJob,
    bytes: Bytes,
) -> Result<ParsedDocument, Stop> {
    let started = Instant::now();
    let cfg = &deps.state.config.ingestion;

    let permit = deps
        .parse_slots
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| Stop::Unavailable)?;

    let limits = ParseLimits {
        max_pdf_pages: cfg.max_pdf_pages,
        max_chars: cfg.max_extracted_chars,
    };
    let mime = claimed.mime_type.clone();
    let handle = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        parse(&bytes, &mime, &limits)
    });

    let document =
        match tokio::time::timeout(Duration::from_secs(cfg.parse_timeout_secs), handle).await {
            Err(_) => return Err(failed(FailureCode::ParseTimeout, "parse timed out")),
            Ok(Err(join)) if join.is_panic() => {
                return Err(failed(FailureCode::ParseFailed, "parser panic"))
            }
            Ok(Err(_)) => return Err(failed(FailureCode::Internal, "parser task cancelled")),
            Ok(Ok(result)) => result.map_err(parse_failure)?,
        };

    log_step("parse", started);
    Ok(document)
}

fn parse_failure(e: ParseError) -> Stop {
    match e {
        ParseError::Unsupported => failed(
            FailureCode::UnsupportedFormat,
            "unsupported document format",
        ),
        ParseError::TooLarge => failed(
            FailureCode::DocumentTooLarge,
            "document text exceeds character limit",
        ),
        ParseError::Empty => failed(FailureCode::DocumentEmpty, "document text is empty"),
        ParseError::Corrupt(why) => {
            failed(FailureCode::ParseFailed, format!("corrupt document: {why}"))
        }
    }
}

// ---------------------------------------------------------------------------
// Step 4: chunk (spawn_blocking)
// ---------------------------------------------------------------------------

async fn chunk_document(
    deps: &IngestionDeps,
    document: ParsedDocument,
) -> Result<Vec<Chunk>, Stop> {
    let started = Instant::now();
    let chunker = deps.chunker.clone();

    let chunks = match tokio::task::spawn_blocking(move || chunker.chunk(&document)).await {
        Ok(Ok(chunks)) => chunks,
        Ok(Err(ChunkError::Empty)) => {
            return Err(failed(
                FailureCode::DocumentEmpty,
                "document produced zero chunks",
            ))
        }
        Ok(Err(ChunkError::TooMany)) => {
            return Err(failed(
                FailureCode::DocumentTooLarge,
                "document exceeds maximum chunk count",
            ))
        }
        Err(join) if join.is_panic() => return Err(failed(FailureCode::Internal, "chunker panic")),
        Err(_) => return Err(failed(FailureCode::Internal, "chunker task error")),
    };

    log_step("chunk", started);
    Ok(chunks)
}

// ---------------------------------------------------------------------------
// Step 5: embed (no tx, lease extended after every batch)
// ---------------------------------------------------------------------------

async fn embed(
    deps: &IngestionDeps,
    claimed: &ClaimedJob,
    chunks: &[Chunk],
) -> Result<Vec<Vec<f32>>, Stop> {
    let started = Instant::now();
    let cfg = &deps.state.config.ingestion;
    let texts: Vec<String> = chunks.iter().map(|c| c.content.clone()).collect();

    // Stop only when the lease is definitely gone (`Ok(false)`). A database
    // error here is not a reason to throw away paid-for embeddings: `complete`
    // re-checks the lease token before anything is written.
    let still_ours = |_embedded: usize| async move {
        !matches!(
            ingestion_repo::extend_lease(
                &deps.state.db,
                claimed.tenant_id,
                claimed.job_id,
                claimed.lease_token,
                cfg.job_lease_secs,
            )
            .await,
            Ok(false)
        )
    };

    let vectors = embed_in_batches(
        deps.state.embedder.as_ref(),
        &texts,
        cfg.embedding_batch,
        still_ours,
    )
    .await
    .map_err(|e| match e {
        EmbeddingError::Transient { retry_after, .. } => Stop::Failed {
            code: FailureCode::EmbeddingUnavailable,
            detail: "embedding failed".into(),
            retry_after,
        },
        EmbeddingError::Permanent { .. } => {
            failed(FailureCode::EmbeddingRejected, "embedding failed")
        }
        EmbeddingError::InvalidResponse { .. } => {
            failed(FailureCode::EmbeddingInvalid, "embedding failed")
        }
    })?
    .ok_or(Stop::Superseded)?;

    log_step("embed", started);
    Ok(vectors)
}

// ---------------------------------------------------------------------------
// Step 6: persist (short tx B)
// ---------------------------------------------------------------------------

async fn persist(
    deps: &IngestionDeps,
    claimed: &ClaimedJob,
    chunks: &[Chunk],
    vectors: &[Vec<f32>],
) -> Result<(), Stop> {
    let started = Instant::now();

    let new_chunks: Vec<NewChunk<'_>> = chunks
        .iter()
        .zip(vectors)
        .map(|(chunk, embedding)| NewChunk {
            index: chunk.index as i32,
            content: &chunk.content,
            token_count: chunk.token_count as i32,
            embedding,
            metadata: serde_json::json!({
                "page_start": chunk.page_start,
                "page_end": chunk.page_end,
                "heading": chunk.heading,
            }),
        })
        .collect();

    let mut tx = begin_system_tenant_tx(&deps.state.db, claimed.tenant_id)
        .await
        .map_err(|e| {
            warn!(error = %e, "database unavailable for persistence");
            Stop::Unavailable
        })?;

    let chunk_target =
        crate::repositories::tenant_repo::get_dedicated_schema(&mut tx, claimed.tenant_id)
            .await
            .map_err(|e| {
                warn!(error = %e, "failed to resolve dedicated schema for tenant");
                Stop::Unavailable
            })?;

    let completed = ingestion_repo::complete(
        &mut tx,
        &chunk_target.table(),
        claimed,
        &deps.state.config.ingestion.embedding_model,
        EMBEDDING_VERSION,
        &new_chunks,
    )
    .await;

    match completed {
        Ok(true) => {}
        Ok(false) => {
            let _ = tx.rollback().await;
            return Err(Stop::Superseded);
        }
        Err(e) => {
            warn!(error = %e, "complete query failed");
            let _ = tx.rollback().await;
            return Err(Stop::Unavailable);
        }
    }

    tx.commit().await.map_err(|e| {
        warn!(error = %e, "complete commit failed");
        Stop::Unavailable
    })?;

    log_step("persist", started);
    Ok(())
}

// ---------------------------------------------------------------------------
// Failure path
// ---------------------------------------------------------------------------

/// Records a failed attempt. A transient failure with attempts left is parked
/// for the sweeper to retry later. Everything else is final, and keeps the
/// original code so the document shows *why* it failed.
async fn record_failure(
    deps: &IngestionDeps,
    claimed: &ClaimedJob,
    code: FailureCode,
    detail: &str,
    retry_after: Option<Duration>,
) -> ProcessOutcome {
    let can_retry =
        code.kind() == FailureKind::Transient && claimed.attempts < claimed.max_attempts;
    if can_retry {
        let delay = backoff(deps, claimed.attempts, retry_after);
        park_for_retry(deps, claimed, code, detail, delay).await
    } else {
        mark_failed(deps, claimed, code, detail).await
    }
}

/// `max(exponential backoff with jitter, provider hint)`. The hint is capped at
/// `retry_max_secs` so a provider cannot park a job for days.
fn backoff(deps: &IngestionDeps, attempts: i32, retry_after: Option<Duration>) -> Duration {
    let cfg = &deps.state.config.ingestion;
    let max = Duration::from_secs(cfg.retry_max_secs);
    let own = retry_delay(
        attempts.max(1) as u32,
        Duration::from_secs(cfg.retry_base_secs),
        max,
        rand::random::<f64>(),
    );
    own.max(retry_after.unwrap_or_default().min(max))
}

async fn park_for_retry(
    deps: &IngestionDeps,
    claimed: &ClaimedJob,
    code: FailureCode,
    detail: &str,
    delay: Duration,
) -> ProcessOutcome {
    let Ok(mut tx) = begin_failure_tx(deps, claimed).await else {
        return ProcessOutcome::Unavailable;
    };
    let result = ingestion_repo::schedule_retry(&mut tx, claimed, code, detail, delay).await;
    match finish_failure_tx(tx, result, "schedule_retry").await {
        Ok(at) => ProcessOutcome::RetryScheduled { at },
        Err(outcome) => outcome,
    }
}

async fn mark_failed(
    deps: &IngestionDeps,
    claimed: &ClaimedJob,
    code: FailureCode,
    detail: &str,
) -> ProcessOutcome {
    let Ok(mut tx) = begin_failure_tx(deps, claimed).await else {
        return ProcessOutcome::Unavailable;
    };
    let result = ingestion_repo::mark_failed(&mut tx, claimed, code, detail)
        .await
        .map(|marked| marked.then_some(()));
    match finish_failure_tx(tx, result, "mark_failed").await {
        Ok(()) => ProcessOutcome::Failed { code },
        Err(outcome) => outcome,
    }
}

async fn begin_failure_tx(deps: &IngestionDeps, claimed: &ClaimedJob) -> Result<Tx, ()> {
    begin_system_tenant_tx(&deps.state.db, claimed.tenant_id)
        .await
        .map_err(|e| warn!(error = %e, "database unavailable for failure marking"))
}

/// Commits when the repo call produced a value. `Ok(None)` means another worker
/// took over (`Superseded`), and an error leaves the job for the sweeper.
async fn finish_failure_tx<T>(
    tx: Tx,
    result: Result<Option<T>, AppError>,
    what: &str,
) -> Result<T, ProcessOutcome> {
    match result {
        Ok(Some(value)) => match tx.commit().await {
            Ok(()) => Ok(value),
            Err(e) => {
                warn!(error = %e, what, "commit failed");
                Err(ProcessOutcome::Unavailable)
            }
        },
        Ok(None) => {
            let _ = tx.rollback().await;
            Err(ProcessOutcome::Superseded)
        }
        Err(e) => {
            warn!(error = %e, what, "query failed");
            let _ = tx.rollback().await;
            Err(ProcessOutcome::Unavailable)
        }
    }
}
