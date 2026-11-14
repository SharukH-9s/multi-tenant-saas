//! Transactional outbox. The table has no RLS (ADR-007 §3), so these
//! functions work on any transaction, scoped or not.

use common_database::Tx;
use common_errors::AppError;
use serde_json::Value;
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use super::db_error::map_db_error;

/// Inserts an unpublished event. Call it on the same transaction as the state
/// change it announces, so both commit or neither does.
pub async fn insert(
    tx: &mut Tx,
    tenant_id: Option<Uuid>,
    aggregate_type: &str,
    aggregate_id: Uuid,
    event_type: &str,
    payload: &Value,
) -> Result<Uuid, AppError> {
    let (id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO outbox_events (tenant_id, aggregate_type, aggregate_id, event_type, payload) \
         VALUES ($1, $2, $3, $4, $5) RETURNING id",
    )
    .bind(tenant_id)
    .bind(aggregate_type)
    .bind(aggregate_id)
    .bind(event_type)
    .bind(payload)
    .fetch_one(&mut **tx)
    .await
    .map_err(map_db_error)?;
    Ok(id)
}

/// A claimed, unpublished event.
#[derive(Debug, Clone, FromRow)]
pub struct OutboxRow {
    pub id: Uuid,
    pub aggregate_id: Uuid,
    pub event_type: String,
    pub payload: Value,
}

/// Claims up to `limit` unpublished rows for `claim_secs` (ADR-007 §1).
///
/// One auto-committed statement: the `FOR UPDATE SKIP LOCKED` row locks end
/// with the statement, so no lock or connection is held while the caller
/// talks to Redis. Concurrent publishers get disjoint rows.
pub async fn claim_batch(
    pool: &PgPool,
    limit: i64,
    claim_secs: u64,
) -> Result<Vec<OutboxRow>, AppError> {
    sqlx::query_as::<_, OutboxRow>(
        "UPDATE outbox_events \
            SET claimed_until = now() + make_interval(secs => $2), \
                publish_attempts = publish_attempts + 1 \
          WHERE id IN ( \
                SELECT id FROM outbox_events \
                 WHERE published_at IS NULL \
                   AND dead_lettered_at IS NULL \
                   AND (claimed_until IS NULL OR claimed_until < now()) \
                 ORDER BY created_at \
                 LIMIT $1 \
                 FOR UPDATE SKIP LOCKED) \
      RETURNING id, aggregate_id, event_type, payload",
    )
    .bind(limit)
    .bind(claim_secs as f64)
    .fetch_all(pool)
    .await
    .map_err(map_db_error)
}

/// Marks rows as published. A no-op for an empty slice.
pub async fn mark_published(pool: &PgPool, ids: &[Uuid]) -> Result<(), AppError> {
    if ids.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "UPDATE outbox_events SET published_at = now(), claimed_until = NULL, last_error = NULL \
          WHERE id = ANY($1)",
    )
    .bind(ids)
    .execute(pool)
    .await
    .map_err(map_db_error)?;
    Ok(())
}

/// Gives claims back (`claimed_until = NULL`) so another cycle can retry now.
pub async fn release(pool: &PgPool, ids: &[Uuid]) -> Result<(), AppError> {
    if ids.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "UPDATE outbox_events SET claimed_until = NULL \
          WHERE id = ANY($1) AND published_at IS NULL",
    )
    .bind(ids)
    .execute(pool)
    .await
    .map_err(map_db_error)?;
    Ok(())
}

/// Parks an event that can never be published (bad payload). The error text
/// is truncated to the column's 500-character limit.
pub async fn dead_letter(pool: &PgPool, id: Uuid, err: &str) -> Result<(), AppError> {
    let err: String = err.chars().take(500).collect();
    sqlx::query(
        "UPDATE outbox_events SET dead_lettered_at = now(), claimed_until = NULL, last_error = $2 \
          WHERE id = $1",
    )
    .bind(id)
    .bind(err)
    .execute(pool)
    .await
    .map_err(map_db_error)?;
    Ok(())
}

/// Deletes published rows older than `older_than_days`, at most `limit` per
/// call. Returns how many were deleted.
pub async fn purge_published(
    pool: &PgPool,
    older_than_days: i32,
    limit: i64,
) -> Result<u64, AppError> {
    let result = sqlx::query(
        "DELETE FROM outbox_events WHERE id IN ( \
             SELECT id FROM outbox_events \
              WHERE published_at IS NOT NULL \
                AND published_at < now() - make_interval(days => $1) \
              LIMIT $2)",
    )
    .bind(older_than_days)
    .bind(limit)
    .execute(pool)
    .await
    .map_err(map_db_error)?;
    Ok(result.rows_affected())
}

/// Unpublished, not dead-lettered rows: count and age in seconds of the oldest.
pub async fn backlog(pool: &PgPool) -> Result<(i64, Option<f64>), AppError> {
    let (count, oldest): (i64, Option<f64>) = sqlx::query_as(
        "SELECT count(*), EXTRACT(EPOCH FROM now() - min(created_at))::float8 \
           FROM outbox_events \
          WHERE published_at IS NULL AND dead_lettered_at IS NULL",
    )
    .fetch_one(pool)
    .await
    .map_err(map_db_error)?;
    Ok((count, oldest))
}
