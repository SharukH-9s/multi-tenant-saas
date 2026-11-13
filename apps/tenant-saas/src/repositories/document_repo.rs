//! Document CRUD repository.
//!
//! All functions take `&mut Tx` and rely on RLS for the tenant filter
//! rather than adding `WHERE tenant_id = …` manually.  The one exception
//! is `insert_pending`, which binds `tenant_id` because INSERT needs the
//! value in the row itself; the RLS `WITH CHECK` policy validates it.
//!
//! `storage_orphans` functions take a raw `&PgPool` because the reconciler
//! runs without a tenant context.

use chrono::{DateTime, Utc};
use common_database::Tx;
use common_errors::AppError;
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use super::db_error::map_db_error;

// ── Row types ────────────────────────────────────────────────────────────────

/// Full document record.  Every column is present so the service layer never
/// needs a second query to fill a response DTO.
#[derive(Debug, FromRow)]
pub struct DocumentRow {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub uploaded_by: Option<Uuid>,
    pub title: String,
    pub original_filename: String,
    pub content_hash: String,
    pub object_key: String,
    pub mime_type: String,
    pub size_bytes: i64,
    pub status: String,
    pub failure_code: Option<String>,
    pub failure_detail: Option<String>,
    pub chunk_count: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
}

/// Input to [`insert_pending`].
pub struct NewDocument {
    pub tenant_id: Uuid,
    pub uploaded_by: Uuid,
    pub title: String,
    pub original_filename: String,
    pub content_hash: String,
    pub object_key: String,
    pub mime_type: String,
    pub size_bytes: i64,
}

/// Result of [`insert_pending`].
#[derive(Debug)]
pub enum InsertOutcome {
    /// A new row was inserted and returned.
    Created(DocumentRow),
    /// A live document with the same `(tenant_id, content_hash)` already
    /// exists; the existing row is returned.
    Duplicate(DocumentRow),
}

/// Keyset pagination cursor for [`list_page`].
pub struct Cursor {
    pub created_at: DateTime<Utc>,
    pub id: Uuid,
}

/// Row from `storage_orphans`.
#[derive(Debug, FromRow)]
pub struct OrphanRow {
    pub object_key: String,
    pub tenant_id: Option<Uuid>,
    pub reason: String,
    pub attempts: i32,
    pub last_error: Option<String>,
    pub next_attempt_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

// ── Document writes (tenant-scoped tx) ───────────────────────────────────────

/// Inserts a pending document, or returns the existing row when the same
/// content hash already exists for this tenant.
///
/// The partial unique index `documents_tenant_content_hash_uniq` is the
/// arbiter.  Because `ON CONFLICT … DO NOTHING` returns no row on
/// conflict, a second query reads the conflicting row.  In the extremely
/// rare window where a concurrent soft-delete removes the conflicting row
/// between the two statements, the function retries the INSERT once
/// before giving up with `AppError::Conflict`.
pub async fn insert_pending(tx: &mut Tx, new: NewDocument) -> Result<InsertOutcome, AppError> {
    // Try the insert.  ON CONFLICT (tenant_id, content_hash) WHERE deleted_at IS NULL
    // causes DO NOTHING when a live duplicate exists — no RETURNING row.
    let maybe_row = sqlx::query_as::<_, DocumentRow>(
        "INSERT INTO documents \
             (tenant_id, uploaded_by, title, original_filename, \
              content_hash, object_key, mime_type, size_bytes) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
         ON CONFLICT (tenant_id, content_hash) WHERE deleted_at IS NULL DO NOTHING \
         RETURNING id, tenant_id, uploaded_by, title, original_filename, \
                   content_hash, object_key, mime_type, size_bytes, \
                   status, failure_code, failure_detail, chunk_count, \
                   created_at, updated_at, deleted_at",
    )
    .bind(new.tenant_id)
    .bind(new.uploaded_by)
    .bind(&new.title)
    .bind(&new.original_filename)
    .bind(&new.content_hash)
    .bind(&new.object_key)
    .bind(&new.mime_type)
    .bind(new.size_bytes)
    .fetch_optional(&mut **tx)
    .await
    .map_err(map_db_error)?;

    if let Some(row) = maybe_row {
        return Ok(InsertOutcome::Created(row));
    }

    // No row returned → a live duplicate exists.  Read it.
    // RLS supplies the tenant restriction.
    if let Some(existing) = sqlx::query_as::<_, DocumentRow>(
        "SELECT id, tenant_id, uploaded_by, title, original_filename, \
                content_hash, object_key, mime_type, size_bytes, \
                status, failure_code, failure_detail, chunk_count, \
                created_at, updated_at, deleted_at \
           FROM documents \
          WHERE content_hash = $1 AND deleted_at IS NULL",
    )
    .bind(&new.content_hash)
    .fetch_optional(&mut **tx)
    .await
    .map_err(map_db_error)?
    {
        return Ok(InsertOutcome::Duplicate(existing));
    }

    // The conflicting row vanished between the INSERT and SELECT — an
    // extremely narrow race where it was soft-deleted concurrently.  Retry
    // the insert once.  If it fails again, something unexpected happened.
    let retry = sqlx::query_as::<_, DocumentRow>(
        "INSERT INTO documents \
             (tenant_id, uploaded_by, title, original_filename, \
              content_hash, object_key, mime_type, size_bytes) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
         ON CONFLICT (tenant_id, content_hash) WHERE deleted_at IS NULL DO NOTHING \
         RETURNING id, tenant_id, uploaded_by, title, original_filename, \
                   content_hash, object_key, mime_type, size_bytes, \
                   status, failure_code, failure_detail, chunk_count, \
                   created_at, updated_at, deleted_at",
    )
    .bind(new.tenant_id)
    .bind(new.uploaded_by)
    .bind(&new.title)
    .bind(&new.original_filename)
    .bind(&new.content_hash)
    .bind(&new.object_key)
    .bind(&new.mime_type)
    .bind(new.size_bytes)
    .fetch_optional(&mut **tx)
    .await
    .map_err(map_db_error)?;

    match retry {
        Some(row) => Ok(InsertOutcome::Created(row)),
        None => Err(AppError::Conflict {
            code: "CONTENT_HASH_CONFLICT",
            message: "A document with this content already exists.".into(),
        }),
    }
}

// ── Document reads (tenant-scoped tx) ────────────────────────────────────────

/// Fetches a single document by id.
///
/// RLS hides rows belonging to other tenants, so a cross-tenant id arrives
/// as `RowNotFound` → `AppError::NotFound` (concealment, no handler code).
pub async fn find_by_id(tx: &mut Tx, id: Uuid) -> Result<DocumentRow, AppError> {
    sqlx::query_as::<_, DocumentRow>(
        "SELECT id, tenant_id, uploaded_by, title, original_filename, \
                content_hash, object_key, mime_type, size_bytes, \
                status, failure_code, failure_detail, chunk_count, \
                created_at, updated_at, deleted_at \
           FROM documents \
          WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(map_db_error)?
    .ok_or(AppError::NotFound)
}

/// Keyset-paginated list, newest first.
///
/// Uses `(created_at DESC, id DESC)` to match `documents_tenant_created_idx`.
/// Never uses `OFFSET`.
pub async fn list_page(
    tx: &mut Tx,
    cursor: Option<&Cursor>,
    limit: i64,
    status: Option<&str>,
) -> Result<Vec<DocumentRow>, AppError> {
    let (cursor_ts, cursor_id) = match cursor {
        Some(c) => (Some(c.created_at), Some(c.id)),
        None => (None, None),
    };

    sqlx::query_as::<_, DocumentRow>(
        "SELECT id, tenant_id, uploaded_by, title, original_filename, \
                content_hash, object_key, mime_type, size_bytes, \
                status, failure_code, failure_detail, chunk_count, \
                created_at, updated_at, deleted_at \
           FROM documents \
          WHERE deleted_at IS NULL \
            AND ($1::timestamptz IS NULL OR (created_at, id) < ($1, $2)) \
            AND ($3::text IS NULL OR status = $3) \
          ORDER BY created_at DESC, id DESC \
          LIMIT $4",
    )
    .bind(cursor_ts)
    .bind(cursor_id)
    .bind(status)
    .bind(limit)
    .fetch_all(&mut **tx)
    .await
    .map_err(map_db_error)
}

/// Transitions a document to `deleting` status (soft-delete step 1).
///
/// Returns the full row so the service layer knows the `object_key`.
/// Invisible rows (cross-tenant) → `NotFound`.
pub async fn mark_deleting(tx: &mut Tx, id: Uuid) -> Result<DocumentRow, AppError> {
    sqlx::query_as::<_, DocumentRow>(
        "UPDATE documents \
            SET status = 'deleting', deleted_at = now() \
          WHERE id = $1 AND deleted_at IS NULL \
          RETURNING id, tenant_id, uploaded_by, title, original_filename, \
                    content_hash, object_key, mime_type, size_bytes, \
                    status, failure_code, failure_detail, chunk_count, \
                    created_at, updated_at, deleted_at",
    )
    .bind(id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(map_db_error)?
    .ok_or(AppError::NotFound)
}

/// Transitions a document from `deleting` to `deleted` (soft-delete step 2).
///
/// Called after the object has been removed from storage.
pub async fn mark_deleted(tx: &mut Tx, id: Uuid) -> Result<(), AppError> {
    let rows = sqlx::query(
        "UPDATE documents SET status = 'deleted' WHERE id = $1 AND status = 'deleting'",
    )
    .bind(id)
    .execute(&mut **tx)
    .await
    .map_err(map_db_error)?
    .rows_affected();

    if rows == 0 {
        return Err(AppError::NotFound);
    }
    Ok(())
}

// ── storage_orphans (no tenant scope, raw pool) ──────────────────────────────

/// Records a storage object that could not be cleaned up by the normal flow.
///
/// The reconciler will retry deletion later.  Upserts so that re-recording
/// the same key just bumps the error and timestamp.
pub async fn record_orphan(
    pool: &PgPool,
    key: &str,
    tenant_id: Uuid,
    reason: &str,
    err: &str,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO storage_orphans (object_key, tenant_id, reason, last_error) \
         VALUES ($1, $2, $3, $4) \
         ON CONFLICT (object_key) DO UPDATE \
            SET last_error = $4, \
                next_attempt_at = now()",
    )
    .bind(key)
    .bind(tenant_id)
    .bind(reason)
    .bind(err)
    .execute(pool)
    .await?;
    Ok(())
}

/// Returns orphan rows whose `next_attempt_at` has passed.
pub async fn due_orphans(pool: &PgPool, limit: i64) -> Result<Vec<OrphanRow>, AppError> {
    Ok(sqlx::query_as::<_, OrphanRow>(
        "SELECT object_key, tenant_id, reason, attempts, last_error, \
                next_attempt_at, created_at \
           FROM storage_orphans \
          WHERE next_attempt_at <= now() \
          ORDER BY next_attempt_at \
          LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?)
}

/// Removes an orphan record after successful cleanup.
pub async fn clear_orphan(pool: &PgPool, key: &str) -> Result<(), AppError> {
    sqlx::query("DELETE FROM storage_orphans WHERE object_key = $1")
        .bind(key)
        .execute(pool)
        .await?;
    Ok(())
}

/// Defers an orphan for later retry after a cleanup failure.
///
/// Increments the attempt counter and pushes `next_attempt_at` forward
/// using exponential backoff (2^attempts minutes, capped at 24 hours).
pub async fn defer_orphan(pool: &PgPool, key: &str, err: &str) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE storage_orphans \
            SET attempts = attempts + 1, \
                last_error = $2, \
                next_attempt_at = now() + \
                    LEAST(INTERVAL '1 minute' * power(2, attempts), INTERVAL '24 hours') \
          WHERE object_key = $1",
    )
    .bind(key)
    .bind(err)
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_outcome_created_is_debug() {
        // Smoke test that the enum derives Debug without needing a DB.
        let _msg = format!("{:?}", InsertOutcome::Created(dummy_row()));
    }

    #[test]
    fn insert_outcome_duplicate_is_debug() {
        let _msg = format!("{:?}", InsertOutcome::Duplicate(dummy_row()));
    }

    fn dummy_row() -> DocumentRow {
        DocumentRow {
            id: Uuid::nil(),
            tenant_id: Uuid::nil(),
            uploaded_by: None,
            title: String::new(),
            original_filename: String::new(),
            content_hash: "a".repeat(64),
            object_key: String::new(),
            mime_type: String::new(),
            size_bytes: 0,
            status: "pending".into(),
            failure_code: None,
            failure_detail: None,
            chunk_count: 0,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            deleted_at: None,
        }
    }
}
