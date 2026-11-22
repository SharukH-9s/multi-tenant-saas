//! `ingestion_jobs` access.
//!
//! # Lock order (plan D4)
//!
//! Every transaction that touches both tables locks the `documents` row first
//! and the job row second. The sweeper uses `FOR UPDATE ... SKIP LOCKED` and
//! never waits. The API delete path locks only `documents` and
//! `document_chunks`, and never touches the job (D9).

use common_database::{begin_system_tenant_tx, Tx};
use common_errors::AppError;
use common_messaging::{IngestionEvent, INGESTION_REQUESTED};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use super::{db_error::map_db_error, outbox_repo};

/// Inserts a `pending` job and its `ingestion.requested` outbox event.
///
/// Must run on the same transaction as the document insert (§10.5). Returns
/// the job id.
pub async fn enqueue(
    tx: &mut Tx,
    tenant_id: Uuid,
    document_id: Uuid,
    max_attempts: u32,
) -> Result<Uuid, AppError> {
    let max_attempts = i32::try_from(max_attempts)
        .map_err(|_| AppError::Internal("job_max_attempts out of range".into()))?;

    let (job_id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO ingestion_jobs (tenant_id, document_id, max_attempts) \
         VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(tenant_id)
    .bind(document_id)
    .bind(max_attempts)
    .fetch_one(&mut **tx)
    .await
    .map_err(map_db_error)?;

    let event = serde_json::to_value(IngestionEvent::new(job_id, tenant_id, document_id))
        .map_err(|e| AppError::Internal(format!("event serialization: {e}")))?;
    outbox_repo::insert(
        tx,
        Some(tenant_id),
        "ingestion_job",
        job_id,
        INGESTION_REQUESTED,
        &event,
    )
    .await?;

    Ok(job_id)
}

/// A job this worker now holds a lease on, plus what it needs from the
/// document row so the download needs no further lookup.
#[derive(Debug, Clone)]
pub struct ClaimedJob {
    pub job_id: Uuid,
    pub tenant_id: Uuid,
    pub document_id: Uuid,
    pub attempts: i32,
    pub max_attempts: i32,
    pub lease_token: Uuid,
    pub object_key: String,
    pub mime_type: String,
    pub size_bytes: i64,
    pub content_hash: String,
}

#[derive(Debug, Clone)]
pub enum ClaimResult {
    Claimed(ClaimedJob),
    /// Done, leased by someone else, `retry_scheduled` (D1), or out of attempts.
    NotClaimable,
    /// The document is deleted. The job was set to `cancelled`.
    DocumentGone,
    /// No such document in this tenant, or the job does not belong to it.
    Mismatch,
}

#[derive(FromRow)]
struct DocumentForClaim {
    deleted_at: Option<chrono::DateTime<chrono::Utc>>,
    object_key: String,
    mime_type: String,
    size_bytes: i64,
    content_hash: String,
}

#[derive(FromRow)]
struct ClaimedRow {
    id: Uuid,
    tenant_id: Uuid,
    document_id: Uuid,
    attempts: i32,
    max_attempts: i32,
    lease_token: Uuid,
}

/// Claims a job for processing. Run it on a `begin_system_tenant_tx`
/// transaction for the event's tenant, and commit before any slow work.
///
/// Steps, in lock order (D4): lock the document, claim the job, cancel the job
/// if the document is deleted, otherwise mark the document `processing`.
/// A `processing` job whose lease has expired can be claimed again, which
/// gives it a new lease token.
pub async fn claim(
    tx: &mut Tx,
    job_id: Uuid,
    document_id: Uuid,
    worker_id: &str,
    lease_secs: u64,
) -> Result<ClaimResult, AppError> {
    let document: Option<DocumentForClaim> = sqlx::query_as(
        "SELECT deleted_at, object_key, mime_type, size_bytes, content_hash::text AS content_hash \
           FROM documents WHERE id = $1 FOR UPDATE",
    )
    .bind(document_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(map_db_error)?;
    let Some(document) = document else {
        return Ok(ClaimResult::Mismatch);
    };

    let claimed: Option<ClaimedRow> = sqlx::query_as(
        "UPDATE ingestion_jobs \
            SET status = 'processing', attempts = attempts + 1, \
                lease_token = gen_random_uuid(), locked_by = $2, \
                locked_until = now() + make_interval(secs => $3) \
          WHERE id = $1 AND document_id = $4 \
            AND attempts < max_attempts \
            AND (status = 'pending' OR (status = 'processing' AND locked_until < now())) \
      RETURNING id, tenant_id, document_id, attempts, max_attempts, lease_token",
    )
    .bind(job_id)
    .bind(worker_id)
    .bind(lease_secs as f64)
    .bind(document_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(map_db_error)?;
    let Some(job) = claimed else {
        return Ok(ClaimResult::NotClaimable);
    };

    if document.deleted_at.is_some() {
        sqlx::query(
            "UPDATE ingestion_jobs \
                SET status = 'cancelled', lease_token = NULL, locked_by = NULL, \
                    locked_until = NULL, completed_at = now() \
              WHERE id = $1",
        )
        .bind(job.id)
        .execute(&mut **tx)
        .await
        .map_err(map_db_error)?;
        return Ok(ClaimResult::DocumentGone);
    }

    sqlx::query(
        "UPDATE documents SET status = 'processing', failure_code = NULL, failure_detail = NULL \
          WHERE id = $1",
    )
    .bind(document_id)
    .execute(&mut **tx)
    .await
    .map_err(map_db_error)?;

    Ok(ClaimResult::Claimed(ClaimedJob {
        job_id: job.id,
        tenant_id: job.tenant_id,
        document_id: job.document_id,
        attempts: job.attempts,
        max_attempts: job.max_attempts,
        lease_token: job.lease_token,
        object_key: document.object_key,
        mime_type: document.mime_type,
        size_bytes: document.size_bytes,
        content_hash: document.content_hash,
    }))
}

/// Pushes `locked_until` forward. Single-row update that does not lock the
/// document, so it is safe with respect to D4. Returns `false` when the lease
/// is no longer ours (token changed or job no longer `processing`).
pub async fn extend_lease(
    pool: &PgPool,
    tenant_id: Uuid,
    job_id: Uuid,
    lease: Uuid,
    lease_secs: u64,
) -> Result<bool, AppError> {
    let mut tx = begin_system_tenant_tx(pool, tenant_id).await?;
    let result = sqlx::query(
        "UPDATE ingestion_jobs \
            SET locked_until = now() + make_interval(secs => $3) \
          WHERE id = $1 AND lease_token = $2 AND status = 'processing'",
    )
    .bind(job_id)
    .bind(lease)
    .bind(lease_secs as f64)
    .execute(&mut *tx)
    .await
    .map_err(map_db_error)?;
    tx.commit().await.map_err(map_db_error)?;
    Ok(result.rows_affected() == 1)
}

const MAX_FAILURE_DETAIL_CHARS: usize = 300;

/// Step 1 of every finishing transaction (D4): lock the document row. Returns
/// `false` when the document is no longer `processing` or has been deleted,
/// which means the caller has been superseded.
async fn lock_processing_document(tx: &mut Tx, document_id: Uuid) -> Result<bool, AppError> {
    let row: Option<(i32,)> = sqlx::query_as(
        "SELECT 1 FROM documents \
          WHERE id = $1 AND status = 'processing' AND deleted_at IS NULL FOR UPDATE",
    )
    .bind(document_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(map_db_error)?;
    Ok(row.is_some())
}

/// Completes an ingestion job and updates the document status to `ready`
/// with the new chunks atomically, obeying lock order (D4):
/// 1. Lock document: must still be `processing` and not deleted.
/// 2. Check lease on job: must be `processing` with matching lease token.
/// 3. Replace chunks (`chunk_repo::replace_for_document`).
/// 4. Update document to `ready`, setting chunk count.
///
/// Returns `Ok(true)` if succeeded, or `Ok(false)` if superseded (e.g. lease expired
/// or document deleted/not processing).
pub async fn complete(
    tx: &mut Tx,
    target: &crate::domain::chunk::ChunkTarget,
    job: &ClaimedJob,
    model: &str,
    version: &str,
    chunks: &[super::chunk_repo::NewChunk<'_>],
) -> Result<bool, AppError> {
    if !lock_processing_document(tx, job.document_id).await? {
        return Ok(false);
    }

    let job_updated = sqlx::query(
        "UPDATE ingestion_jobs \
            SET status = 'succeeded', lease_token = NULL, locked_until = NULL, \
                locked_by = NULL, completed_at = now(), last_error_code = NULL \
          WHERE id = $1 AND lease_token = $2 AND status = 'processing'",
    )
    .bind(job.job_id)
    .bind(job.lease_token)
    .execute(&mut **tx)
    .await
    .map_err(map_db_error)?;
    if job_updated.rows_affected() == 0 {
        return Ok(false);
    }

    super::chunk_repo::replace_for_document(
        tx,
        target,
        job.tenant_id,
        job.document_id,
        model,
        version,
        chunks,
    )
    .await?;

    let chunk_count = i32::try_from(chunks.len())
        .map_err(|_| AppError::Internal("chunk count exceeds i32".into()))?;

    sqlx::query(
        "UPDATE documents \
            SET status = 'ready', chunk_count = $1, failure_code = NULL, failure_detail = NULL \
          WHERE id = $2",
    )
    .bind(chunk_count)
    .bind(job.document_id)
    .execute(&mut **tx)
    .await
    .map_err(map_db_error)?;

    Ok(true)
}

/// Parks a job for a later retry after a transient failure. Same lock order
/// and lease check as [`mark_failed`].
///
/// The job becomes `retry_scheduled`, which is **not** claimable (D1): only the
/// sweeper moves it back to `pending` once `available_at` has passed.
///
/// Returns the time the retry becomes due, or `None` if superseded.
pub async fn schedule_retry(
    tx: &mut Tx,
    job: &ClaimedJob,
    code: crate::domain::ingestion::FailureCode,
    detail: &str,
    delay: std::time::Duration,
) -> Result<Option<chrono::DateTime<chrono::Utc>>, AppError> {
    if !lock_processing_document(tx, job.document_id).await? {
        return Ok(None);
    }

    let due: Option<(chrono::DateTime<chrono::Utc>,)> = sqlx::query_as(
        "UPDATE ingestion_jobs \
            SET status = 'retry_scheduled', lease_token = NULL, locked_until = NULL, \
                locked_by = NULL, available_at = now() + make_interval(secs => $3), \
                last_error_code = $4, last_error_at = now() \
          WHERE id = $1 AND lease_token = $2 AND status = 'processing' \
      RETURNING available_at",
    )
    .bind(job.job_id)
    .bind(job.lease_token)
    .bind(delay.as_secs_f64())
    .bind(code.as_str())
    .fetch_optional(&mut **tx)
    .await
    .map_err(map_db_error)?;
    let Some((available_at,)) = due else {
        return Ok(None);
    };

    let detail: String = detail.chars().take(MAX_FAILURE_DETAIL_CHARS).collect();
    sqlx::query(
        "UPDATE documents \
            SET status = 'retry_scheduled', failure_code = $1, failure_detail = $2 \
          WHERE id = $3",
    )
    .bind(code.as_str())
    .bind(&detail)
    .bind(job.document_id)
    .execute(&mut **tx)
    .await
    .map_err(map_db_error)?;

    Ok(Some(available_at))
}

/// What the sweeper did to one job.
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct SweptJob {
    pub job_id: Uuid,
    pub tenant_id: Uuid,
    /// `cancelled`, `failed` or `requeued`.
    pub action: String,
}

/// Runs the cross-tenant recovery rules in the database (`sweep_ingestion_jobs`,
/// migration 005). Works on the plain `app_runtime` pool: the function is
/// `SECURITY DEFINER`, so no other tenant data is reachable from this side.
pub async fn sweep(
    pool: &PgPool,
    batch: i32,
    pending_stale_secs: i32,
) -> Result<Vec<SweptJob>, AppError> {
    sqlx::query_as::<_, SweptJob>(
        "SELECT job_id, tenant_id, action FROM sweep_ingestion_jobs($1, $2)",
    )
    .bind(batch)
    .bind(pending_stale_secs)
    .fetch_all(pool)
    .await
    .map_err(map_db_error)
}

/// Marks an ingestion job and document as `failed` (permanent failure).
/// Obeying lock order (D4):
/// 1. Lock document: must still be `processing` and not deleted.
/// 2. Check lease on job: must be `processing` with matching lease token.
/// 3. Update job to `failed`.
/// 4. Update document to `failed`.
///
/// Returns `Ok(true)` if marked failed, or `Ok(false)` if superseded.
pub async fn mark_failed(
    tx: &mut Tx,
    job: &ClaimedJob,
    code: crate::domain::ingestion::FailureCode,
    detail: &str,
) -> Result<bool, AppError> {
    if !lock_processing_document(tx, job.document_id).await? {
        return Ok(false);
    }

    let detail: String = detail.chars().take(MAX_FAILURE_DETAIL_CHARS).collect();

    let job_updated = sqlx::query(
        "UPDATE ingestion_jobs \
            SET status = 'failed', lease_token = NULL, locked_until = NULL, \
                locked_by = NULL, last_error_code = $3, last_error_at = now(), completed_at = now() \
          WHERE id = $1 AND lease_token = $2 AND status = 'processing'",
    )
    .bind(job.job_id)
    .bind(job.lease_token)
    .bind(code.as_str())
    .execute(&mut **tx)
    .await
    .map_err(map_db_error)?;
    if job_updated.rows_affected() == 0 {
        return Ok(false);
    }

    sqlx::query(
        "UPDATE documents \
            SET status = 'failed', failure_code = $1, failure_detail = $2 \
          WHERE id = $3",
    )
    .bind(code.as_str())
    .bind(&detail)
    .bind(job.document_id)
    .execute(&mut **tx)
    .await
    .map_err(map_db_error)?;

    Ok(true)
}
