//! Integration tests for storage orphan reconciliation (Task 7).
//!
//! Validates the three passes of `services::storage_reconciler::reconcile`:
//! 1. Retrying recorded orphans from `storage_orphans`.
//! 2. Finalizing documents stuck in `deleting`.
//! 3. Deep sweep of unreferenced objects past retention window.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use chrono::Duration;
use common_storage::{FileStorage, ObjectWriter, StorageError, StoredObject};
use tenant_saas::services::storage_reconciler::reconcile;
use test_support::TestApp;
use uuid::Uuid;

/// Storage wrapper that simulates older object timestamps for deep-sweep tests.
struct OffsetStorage {
    inner: Arc<dyn FileStorage>,
    age_offset: Duration,
}

#[async_trait]
impl FileStorage for OffsetStorage {
    async fn begin_write(&self, key: &str) -> Result<Box<dyn ObjectWriter>, StorageError> {
        self.inner.begin_write(key).await
    }

    async fn put(&self, key: &str, bytes: Bytes) -> Result<(), StorageError> {
        self.inner.put(key, bytes).await
    }

    async fn get(&self, key: &str) -> Result<Bytes, StorageError> {
        self.inner.get(key).await
    }

    async fn delete(&self, key: &str) -> Result<(), StorageError> {
        self.inner.delete(key).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<StoredObject>, StorageError> {
        let mut items = self.inner.list(prefix).await?;
        for item in &mut items {
            item.last_modified -= self.age_offset;
        }
        Ok(items)
    }

    async fn health(&self) -> Result<(), StorageError> {
        self.inner.health().await
    }
}

// ── Test 1 ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn orphan_past_retention_window_is_removed_by_deep_pass() {
    let Some(app) = TestApp::spawn().await else {
        return;
    };

    let tenant_id = app.create_tenant().await;
    let key = format!("tenants/{tenant_id}/documents/{}", Uuid::new_v4());

    // Put an object directly into storage without any corresponding DB row.
    app.state
        .storage
        .put(&key, Bytes::from("abandoned content"))
        .await
        .expect("storage put");
    assert!(app.object_exists(&key).await);

    // Shift object timestamps into the past so it exceeds orphan_retention_secs.
    let offset = Duration::seconds(app.state.config.storage.orphan_retention_secs as i64 + 3600);
    let state_with_offset = app.state.clone().with_storage(Arc::new(OffsetStorage {
        inner: app.state.storage.clone(),
        age_offset: offset,
    }));

    let report = reconcile(&state_with_offset, true)
        .await
        .expect("reconcile");
    assert!(report.swept >= 1);
    assert!(
        !app.object_exists(&key).await,
        "aged orphan must be removed"
    );
}

// ── Test 2 ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn orphan_inside_retention_window_survives() {
    let Some(app) = TestApp::spawn().await else {
        return;
    };

    let tenant_id = app.create_tenant().await;
    let key = format!("tenants/{tenant_id}/documents/{}", Uuid::new_v4());

    // Put an object into storage without a DB row, but leave it young (< retention window).
    app.state
        .storage
        .put(&key, Bytes::from("fresh in-flight upload content"))
        .await
        .expect("storage put");
    assert!(app.object_exists(&key).await);

    let report = reconcile(&app.state, true).await.expect("reconcile");
    assert!(report.swept >= 1);
    assert!(
        app.object_exists(&key).await,
        "object inside retention window must survive deep pass"
    );
}

// ── Test 3 ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn referenced_object_survives_deep_pass() {
    let Some(app) = TestApp::spawn().await else {
        return;
    };

    let session = app.create_session().await;
    let tenant_id = app
        .seed_membership(session.user_id, "admin", "active")
        .await;

    let doc_id = app.create_document(tenant_id, &session.access_token).await;
    let row = app
        .document_row(tenant_id, doc_id)
        .await
        .expect("document row");
    assert!(app.object_exists(&row.object_key).await);

    // Even if the object timestamp is artificially backdated past retention...
    let offset = Duration::seconds(app.state.config.storage.orphan_retention_secs as i64 + 3600);
    let state_with_offset = app.state.clone().with_storage(Arc::new(OffsetStorage {
        inner: app.state.storage.clone(),
        age_offset: offset,
    }));

    let report = reconcile(&state_with_offset, true)
        .await
        .expect("reconcile");
    assert!(report.swept >= 1);
    assert!(
        app.object_exists(&row.object_key).await,
        "referenced object must survive deep pass"
    );
}

// ── Test 4 ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn orphan_row_with_missing_key_is_cleared() {
    let Some(app) = TestApp::spawn().await else {
        return;
    };

    let tenant_id = app.create_tenant().await;
    let missing_key = format!("tenants/{tenant_id}/documents/{}", Uuid::new_v4());

    tenant_saas::repositories::document_repo::record_orphan(
        &app.runtime_pool,
        &missing_key,
        tenant_id,
        "insert_failed",
        "storage write failed during initial upload",
    )
    .await
    .expect("record orphan");

    assert!(!app.object_exists(&missing_key).await);

    let report = reconcile(&app.state, false).await.expect("reconcile");
    assert!(report.retried >= 1);
    assert!(report.deleted >= 1);

    let orphans = tenant_saas::repositories::document_repo::due_orphans(&app.runtime_pool, 10)
        .await
        .expect("due orphans");
    assert!(
        !orphans.iter().any(|o| o.object_key == missing_key),
        "cleared orphan must no longer be present in storage_orphans"
    );
}

// ── Test 5 ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn stuck_deleting_document_is_finalized() {
    let Some(app) = TestApp::spawn().await else {
        return;
    };

    let session = app.create_session().await;
    let tenant_id = app
        .seed_membership(session.user_id, "admin", "active")
        .await;

    let doc_id = app.create_document(tenant_id, &session.access_token).await;
    let row = app
        .document_row(tenant_id, doc_id)
        .await
        .expect("document row");
    assert!(app.object_exists(&row.object_key).await);

    // Simulate an interrupted delete: status is 'deleting' and deleted_at is > 1 min ago.
    let mut tx = app.owner_scoped_tx(tenant_id, session.user_id).await;
    sqlx::query(
        "UPDATE documents \
            SET status = 'deleting', \
                deleted_at = now() - INTERVAL '2 minutes' \
          WHERE id = $1",
    )
    .bind(doc_id)
    .execute(&mut *tx)
    .await
    .expect("backdate stuck deleting document");
    tx.commit().await.expect("commit");

    let report = reconcile(&app.state, false).await.expect("reconcile");
    assert!(report.finalized >= 1);

    // Object should now be gone from storage.
    assert!(
        !app.object_exists(&row.object_key).await,
        "storage object must be deleted by reconciler"
    );

    // Document status in DB should now be 'deleted'.
    let updated = app
        .document_row(tenant_id, doc_id)
        .await
        .expect("document row");
    assert_eq!(
        updated.status, "deleted",
        "document must reach deleted status"
    );
}

// ── Test 6 ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn pending_document_object_survives_pass() {
    let Some(app) = TestApp::spawn().await else {
        return;
    };

    let session = app.create_session().await;
    let tenant_id = app
        .seed_membership(session.user_id, "admin", "active")
        .await;

    let doc_id = app.create_document(tenant_id, &session.access_token).await;
    let row = app
        .document_row(tenant_id, doc_id)
        .await
        .expect("document row");
    assert_eq!(row.status, "pending");
    assert!(app.object_exists(&row.object_key).await);

    let report = reconcile(&app.state, true).await.expect("reconcile");
    assert_eq!(report.finalized, 0);

    assert!(
        app.object_exists(&row.object_key).await,
        "pending document's object must survive"
    );

    let current = app
        .document_row(tenant_id, doc_id)
        .await
        .expect("document row");
    assert_eq!(current.status, "pending");
}
