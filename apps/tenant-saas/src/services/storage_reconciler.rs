//! Storage orphan reconciler.
//!
//! Runs three sequential passes to close gaps in the normal upload/delete flows:
//!
//! | Pass | What it does |
//! |------|--------------|
//! | 1    | Retry recorded orphans (`storage_orphans`) whose backoff timer has expired. |
//! | 2    | Finalize documents stuck in `deleting` for more than 1 minute. |
//! | 3    | Deep sweep: enumerate all storage objects and remove unreferenced aged ones. |
//!
//! **Pass 3 is opt-in** (`deep = true`). It is expensive on large buckets and
//! should only be run from a maintenance cron, not the hot request path.
//!
//! # RLS note
//!
//! The reconciler has no tenant context, so it **must not** use
//! `begin_tenant_tx`.  `storage_orphans` has no RLS policy; the owner pool
//! (if present in `AppState`) bypasses the `FORCE ROW LEVEL SECURITY` on the
//! `documents` table.  When `owner_pool` is absent, Pass 2 and Pass 3 database
//! cross-checks are skipped (they log a warning).

use chrono::{Duration, Utc};
use common_errors::AppError;
use sqlx::FromRow;
use tracing::{info, warn};
use uuid::Uuid;

use crate::{
    app_state::AppState,
    repositories::document_repo::{clear_orphan, defer_orphan, due_orphans},
};

// ── Constants ────────────────────────────────────────────────────────────────

/// Give up retrying an orphan after this many attempts.
const MAX_ORPHAN_ATTEMPTS: i32 = 10;

/// A document must have been stuck in `deleting` for at least this long before
/// the reconciler finalizes it.  Prevents racing with the normal delete path.
const STUCK_DELETING_MINS: i64 = 1;

// ── Report ───────────────────────────────────────────────────────────────────

/// Summary returned (and logged) after each reconcile run.
#[derive(Debug, Default)]
pub struct ReconcileReport {
    /// Orphan rows from `storage_orphans` that were attempted this run.
    pub retried: usize,
    /// Of those, keys successfully deleted from storage.
    pub deleted: usize,
    /// Of those, keys that failed again (and were deferred or abandoned).
    pub still_failing: usize,
    /// Objects examined by the deep sweep (Pass 3).
    pub swept: usize,
    /// Documents finalized from `deleting` → `deleted` by Pass 2.
    pub finalized: usize,
}

// ── Pass-2 row type ───────────────────────────────────────────────────────────

#[derive(Debug, FromRow)]
struct StuckDeletingRow {
    id: Uuid,
    tenant_id: Uuid,
    object_key: String,
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Runs all reconciliation passes against the given application state.
///
/// When `deep` is `false`, Pass 3 (full storage enumeration) is skipped.
/// Pass 1 and Pass 2 are always run.
pub async fn reconcile(state: &AppState, deep: bool) -> Result<ReconcileReport, AppError> {
    let mut report = ReconcileReport::default();

    pass1_retry_orphans(state, &mut report).await?;
    pass2_finalize_stuck_deleting(state, &mut report).await?;

    if deep {
        pass3_deep_sweep(state, &mut report).await?;
    }

    info!(
        retried = report.retried,
        deleted = report.deleted,
        still_failing = report.still_failing,
        finalized = report.finalized,
        swept = report.swept,
        "reconcile complete"
    );
    Ok(report)
}

// ── Pass 1: retry recorded orphans ──────────────────────────────────────────

async fn pass1_retry_orphans(
    state: &AppState,
    report: &mut ReconcileReport,
) -> Result<(), AppError> {
    let orphans = due_orphans(&state.db, 100).await?;

    for row in orphans {
        report.retried += 1;

        match state.storage.delete(&row.object_key).await {
            Ok(()) => {
                // Key is gone; remove the tracking row.
                clear_orphan(&state.db, &row.object_key).await?;
                report.deleted += 1;
            }
            Err(e) => {
                report.still_failing += 1;
                let err_str = e.to_string();

                if row.attempts + 1 >= MAX_ORPHAN_ATTEMPTS {
                    // Too many attempts — abandon rather than retry forever.
                    warn!(
                        key   = %row.object_key,
                        attempts = row.attempts + 1,
                        err   = %err_str,
                        "orphan exceeded max attempts; abandoning"
                    );
                    clear_orphan(&state.db, &row.object_key).await?;
                } else {
                    defer_orphan(&state.db, &row.object_key, &err_str).await?;
                }
            }
        }
    }

    Ok(())
}

// ── Pass 2: finalize documents stuck in `deleting` ───────────────────────────

async fn pass2_finalize_stuck_deleting(
    state: &AppState,
    report: &mut ReconcileReport,
) -> Result<(), AppError> {
    let Some(owner) = &state.owner_pool else {
        warn!("owner_pool not configured; skipping Pass 2 (stuck-deleting finalization)");
        return Ok(());
    };

    let cutoff = Utc::now() - Duration::minutes(STUCK_DELETING_MINS);

    // Query through the owner pool to bypass FORCE ROW LEVEL SECURITY on documents.
    let stuck: Vec<StuckDeletingRow> = sqlx::query_as::<_, StuckDeletingRow>(
        "SELECT id, tenant_id, object_key \
           FROM documents \
          WHERE status = 'deleting' \
            AND deleted_at < $1 \
          LIMIT 100",
    )
    .bind(cutoff)
    .fetch_all(owner)
    .await
    .map_err(|e| AppError::Internal(format!("reconciler pass2 query failed: {e}")))?;

    for row in stuck {
        // Delete the storage object first.  If this fails, leave the document
        // in `deleting` so the next reconcile run tries again.
        match state.storage.delete(&row.object_key).await {
            Ok(()) => {}
            Err(e) => {
                warn!(
                    doc_id = %row.id,
                    tenant_id = %row.tenant_id,
                    err = %e,
                    "pass2: failed to delete storage object; will retry next run"
                );
                continue;
            }
        }

        // Mark the document row as deleted, again through the owner pool.
        let affected = sqlx::query(
            "UPDATE documents SET status = 'deleted' \
              WHERE id = $1 AND status = 'deleting'",
        )
        .bind(row.id)
        .execute(owner)
        .await
        .map_err(|e| AppError::Internal(format!("reconciler pass2 update failed: {e}")))?
        .rows_affected();

        if affected > 0 {
            info!(
                doc_id    = %row.id,
                tenant_id = %row.tenant_id,
                "pass2: finalized stuck-deleting document"
            );
            report.finalized += 1;
        }
    }

    Ok(())
}

// ── Pass 3: deep sweep ───────────────────────────────────────────────────────

async fn pass3_deep_sweep(state: &AppState, report: &mut ReconcileReport) -> Result<(), AppError> {
    let Some(owner) = &state.owner_pool else {
        warn!("owner_pool not configured; skipping Pass 3 (deep sweep)");
        return Ok(());
    };

    let retention_secs = state.config.storage.orphan_retention_secs;

    // List every object under the tenant prefix.
    let objects = state
        .storage
        .list("tenants/")
        .await
        .map_err(|e| AppError::Internal(format!("reconciler list failed: {e}")))?;

    for obj in objects {
        report.swept += 1;

        // ⚠️  AGE GUARD — this is the ONLY protection against deleting objects
        // that are still being written.  An in-flight upload can keep an object
        // key in storage for up to `max_upload_bytes / network_speed` seconds
        // before the `insert_pending` row appears.  We therefore refuse to
        // touch any object whose `last_modified` timestamp is more recent than
        // `orphan_retention_secs` ago.  Lowering this value below the maximum
        // expected upload duration WILL cause data loss.  Do not remove this
        // guard without a thorough audit.
        let age_cutoff = Utc::now() - Duration::seconds(retention_secs as i64);
        if obj.last_modified > age_cutoff {
            // Object is too young; skip regardless of database state.
            continue;
        }

        // Check whether any live document references this key (bypassing RLS
        // by using the owner pool).
        let referenced: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM documents \
              WHERE object_key = $1 AND deleted_at IS NULL \
              LIMIT 1",
        )
        .bind(&obj.key)
        .fetch_optional(owner)
        .await
        .map_err(|e| AppError::Internal(format!("reconciler sweep query failed: {e}")))?;

        if referenced.is_some() {
            // A live document owns this key; leave it alone.
            continue;
        }

        // Key is old enough and unreferenced: delete it.
        match state.storage.delete(&obj.key).await {
            Ok(()) => {
                info!(key = %obj.key, "pass3: removed unreferenced orphan object");
            }
            Err(e) => {
                warn!(key = %obj.key, err = %e, "pass3: failed to delete orphan object");
            }
        }
    }

    Ok(())
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_default_is_all_zeros() {
        let r = ReconcileReport::default();
        assert_eq!(r.retried, 0);
        assert_eq!(r.deleted, 0);
        assert_eq!(r.still_failing, 0);
        assert_eq!(r.swept, 0);
        assert_eq!(r.finalized, 0);
    }

    #[test]
    fn report_is_debug() {
        let _ = format!("{:?}", ReconcileReport::default());
    }

    #[test]
    fn stuck_deleting_mins_is_positive() {
        const { assert!(STUCK_DELETING_MINS > 0) };
    }

    #[test]
    fn max_orphan_attempts_is_reasonable() {
        // Between 3 and 100 — avoids both "too aggressive" and "retry forever" mistakes.
        assert!((3..=100).contains(&MAX_ORPHAN_ATTEMPTS));
    }
}
