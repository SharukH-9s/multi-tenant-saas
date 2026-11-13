//! Document lifecycle: upload, get, list, delete.
//!
//! Permission checks happen **before** any transaction opens, matching the
//! `tenant_service` pattern.  The DB transaction covers **only** the
//! `insert_pending` call — no storage I/O, hashing, or Redis touches
//! happen while a transaction is open (see §7.3.1).
//!
//! # Upload flow (D7)
//!
//! 1. Permission check (`DocumentsUpload`).
//! 2. Validate + hash + stream to object storage (no tx open).
//! 3. Open a short-lived tx → `insert_pending` → commit.
//! 4. If deduplicated, delete the just-written object.
//! 5. Audit log.
//!
//! Every exit path between `begin_write` and `finish` aborts the writer
//! via the [`write_validated_stream`] helper.

use bytes::Bytes;
use common_auth::{Permission, TenantContext};
use common_database::begin_tenant_tx;
use common_errors::AppError;
use common_storage::{FileStorage, ObjectWriter, StorageError};
use uuid::Uuid;

use crate::{
    app_state::AppState,
    domain::document::{ContentHash, SupportedFormat},
    repositories::document_repo::{self, Cursor, DocumentRow, InsertOutcome, NewDocument},
    services::{object_key, upload_validator::UploadValidator},
};

// ── Public types ─────────────────────────────────────────────────────────────

/// Returned by [`upload`] so the handler knows whether to respond 202 or 200.
pub struct UploadResult {
    pub document: DocumentRow,
    pub deduplicated: bool,
}

/// Pre-validated fields extracted by the handler's multipart loop.
pub struct IncomingUpload {
    pub title: String,
    pub original_filename: String,
    pub format: SupportedFormat,
    /// The multipart `file` field's body, already sitting in memory or as a
    /// `Bytes` stream.  The handler reads field chunks and forwards them here.
    pub chunks: Vec<Bytes>,
}

/// Query parameters for [`list`].
pub struct ListQuery {
    pub cursor: Option<Cursor>,
    pub limit: i64,
    pub status: Option<String>,
}

// ── Service functions ────────────────────────────────────────────────────────

/// Uploads a document: streams to storage, hashes, validates, then inserts.
///
/// The transaction covers **only** `insert_pending`.  No storage call, no
/// hashing, and no Redis touch happens while it is open (§7.3.1).
pub async fn upload(
    state: &AppState,
    ctx: &TenantContext,
    upload: IncomingUpload,
) -> Result<UploadResult, AppError> {
    // ── 1. Permission gate (before any I/O) ──────────────────────────────
    ctx.require(Permission::DocumentsUpload)?;

    let max_bytes = state.config.storage.max_upload_bytes;
    let document_id = Uuid::new_v4();
    let key = object_key::for_document(ctx.tenant_id(), document_id);

    // ── 2. Stream to storage + validate + hash ───────────────────────────
    let (hash, size_bytes) = write_validated_stream(
        state.storage.as_ref(),
        &key,
        upload.format,
        max_bytes,
        upload.chunks,
    )
    .await?;

    // ── 3. Short-lived transaction: insert_pending ───────────────────────
    let outcome = {
        let mut tx = begin_tenant_tx(&state.db, ctx).await?;
        let outcome = document_repo::insert_pending(
            &mut tx,
            NewDocument {
                tenant_id: ctx.tenant_id(),
                uploaded_by: ctx.user_id(),
                title: upload.title,
                original_filename: upload.original_filename,
                content_hash: hash.as_str().to_owned(),
                object_key: key.clone(),
                mime_type: upload.format.mime_type().to_owned(),
                size_bytes: size_bytes as i64,
            },
        )
        .await;

        match outcome {
            Ok(o) => {
                tx.commit().await?;
                o
            }
            Err(e) => {
                // Transaction failed — try to delete the uploaded object.
                // A failure here becomes an orphan, but the client sees the
                // original database error.
                drop(tx); // rollback
                if let Err(del_err) = state.storage.delete(&key).await {
                    tracing::warn!(
                        object_key = %key,
                        error = %del_err,
                        "failed to delete object after transaction failure"
                    );
                    let _ = document_repo::record_orphan(
                        &state.db,
                        &key,
                        ctx.tenant_id(),
                        "insert_failed",
                        &del_err.to_string(),
                    )
                    .await;
                }
                return Err(e);
            }
        }
    };

    // ── 4. Handle outcome ────────────────────────────────────────────────
    match outcome {
        InsertOutcome::Created(row) => {
            tracing::info!(
                target: "audit",
                event = "document.uploaded",
                tenant_id = %ctx.tenant_id(),
                document_id = %row.id,
                actor = %ctx.user_id(),
                size_bytes = row.size_bytes,
                mime_type = %row.mime_type,
                deduplicated = false,
            );
            Ok(UploadResult {
                document: row,
                deduplicated: false,
            })
        }
        InsertOutcome::Duplicate(existing) => {
            // Delete the just-written object — the existing row has the
            // canonical copy.  A failure becomes a `storage_orphans` row,
            // but the request still returns 200 because the client's view
            // of the world is correct either way.
            if let Err(del_err) = state.storage.delete(&key).await {
                tracing::warn!(
                    object_key = %key,
                    error = %del_err,
                    "failed to delete duplicate object"
                );
                let _ = document_repo::record_orphan(
                    &state.db,
                    &key,
                    ctx.tenant_id(),
                    "duplicate_cleanup_failed",
                    &del_err.to_string(),
                )
                .await;
            }

            tracing::info!(
                target: "audit",
                event = "document.uploaded",
                tenant_id = %ctx.tenant_id(),
                document_id = %existing.id,
                actor = %ctx.user_id(),
                size_bytes = existing.size_bytes,
                mime_type = %existing.mime_type,
                deduplicated = true,
            );
            Ok(UploadResult {
                document: existing,
                deduplicated: true,
            })
        }
    }
}

/// Returns a single document by id.
pub async fn get(state: &AppState, ctx: &TenantContext, id: Uuid) -> Result<DocumentRow, AppError> {
    ctx.require(Permission::DocumentsSearch)?;

    let mut tx = begin_tenant_tx(&state.db, ctx).await?;
    let row = document_repo::find_by_id(&mut tx, id).await?;
    tx.commit().await?;
    Ok(row)
}

/// Lists documents with keyset pagination.
pub async fn list(
    state: &AppState,
    ctx: &TenantContext,
    query: ListQuery,
) -> Result<Vec<DocumentRow>, AppError> {
    ctx.require(Permission::DocumentsSearch)?;

    let mut tx = begin_tenant_tx(&state.db, ctx).await?;
    let rows = document_repo::list_page(
        &mut tx,
        query.cursor.as_ref(),
        query.limit,
        query.status.as_deref(),
    )
    .await?;
    tx.commit().await?;
    Ok(rows)
}

/// Soft-deletes a document and removes its object from storage.
///
/// # Permission rules (D11)
///
/// - `DocumentsDeleteAny`: Owner and Admin can delete any document.
/// - `DocumentsDeleteOwn`: Member can delete only documents they uploaded.
/// - Viewer: 403 (no delete permission at all).
///
/// Returning 403 rather than 404 for "not your document" is deliberate:
/// concealment protects *other tenants*; this document is inside the
/// caller's own tenant, so hiding it would only confuse a legitimate member.
pub async fn delete(state: &AppState, ctx: &TenantContext, id: Uuid) -> Result<(), AppError> {
    // ── Permission: check delete-any first, then fall back to delete-own ─
    if !ctx.has(Permission::DocumentsDeleteAny) {
        ctx.require(Permission::DocumentsDeleteOwn)?; // Viewer stops here with 403
    }

    // ── Step 1: mark_deleting (sets status + deleted_at in one UPDATE) ───
    let mut tx = begin_tenant_tx(&state.db, ctx).await?;
    let document = document_repo::mark_deleting(&mut tx, id).await?;

    // Ownership check for delete-own callers.
    // 403, not 404: the row is in the caller's own tenant — concealment
    // protects other tenants, not same-tenant members.
    if !ctx.has(Permission::DocumentsDeleteAny) && document.uploaded_by != Some(ctx.user_id()) {
        // Rollback the mark_deleting so the row stays live.
        tx.rollback().await?;
        return Err(AppError::Forbidden);
    }
    tx.commit().await?;

    // ── Step 2: delete the object from storage ──────────────────────────
    if let Err(del_err) = state.storage.delete(&document.object_key).await {
        tracing::warn!(
            document_id = %id,
            error = %del_err,
            "failed to delete object from storage"
        );
        let _ = document_repo::record_orphan(
            &state.db,
            &document.object_key,
            ctx.tenant_id(),
            "delete_failed",
            &del_err.to_string(),
        )
        .await;
        // Continue — the document is already marked deleting in the DB.
    }

    // ── Step 3: mark_deleted (status → deleted) ─────────────────────────
    let mut tx = begin_tenant_tx(&state.db, ctx).await?;
    // Ignore errors — the reconciler will catch documents stuck in
    // `deleting` status.
    let _ = document_repo::mark_deleted(&mut tx, id).await;
    tx.commit().await?;

    tracing::info!(
        target: "audit",
        event = "document.deleted",
        tenant_id = %ctx.tenant_id(),
        document_id = %id,
        actor = %ctx.user_id(),
        size_bytes = document.size_bytes,
        mime_type = %document.mime_type,
        deduplicated = false,
    );

    Ok(())
}

// ── Private helpers ──────────────────────────────────────────────────────────

/// Streams validated chunks into object storage and returns the content hash
/// and total byte count.
///
/// Guarantees: if any step between `begin_write` and `finish` fails, the
/// writer is aborted.  The caller never needs to clean up the writer itself.
async fn write_validated_stream(
    storage: &dyn FileStorage,
    key: &str,
    format: SupportedFormat,
    max_bytes: u64,
    chunks: Vec<Bytes>,
) -> Result<(ContentHash, u64), AppError> {
    let mut writer = storage.begin_write(key).await.map_err(storage_err_to_app)?;

    match write_inner(&mut *writer, format, max_bytes, chunks).await {
        Ok(result) => {
            writer.finish().await.map_err(|e| {
                // finish failed — can't abort after that; just map the error.
                storage_err_to_app(e)
            })?;
            Ok(result)
        }
        Err(e) => {
            // Abort the writer on any validation or I/O failure.
            let _ = writer.abort().await;
            Err(e)
        }
    }
}

/// The inner loop: validate + hash + write chunks.  Separated so the
/// abort-on-error guard in the caller stays clean.
async fn write_inner(
    writer: &mut dyn ObjectWriter,
    format: SupportedFormat,
    max_bytes: u64,
    chunks: Vec<Bytes>,
) -> Result<(ContentHash, u64), AppError> {
    let mut validator = UploadValidator::new(format, max_bytes);

    for chunk in chunks {
        let validated = validator.push(chunk)?;
        if !validated.is_empty() {
            writer.write(validated).await.map_err(storage_err_to_app)?;
        }
    }

    let (hash, total, tail) = validator.finish()?;

    // Write any bytes released by finish (e.g. the sniff buffer for short files).
    if !tail.is_empty() {
        writer.write(tail).await.map_err(storage_err_to_app)?;
    }

    Ok((hash, total))
}

/// Maps `StorageError` to `AppError`.
fn storage_err_to_app(e: StorageError) -> AppError {
    match e {
        StorageError::Unavailable(msg) => AppError::ExternalService(format!("storage: {msg}")),
        StorageError::NotFound => AppError::NotFound,
        other => AppError::Internal(format!("storage: {other}")),
    }
}
