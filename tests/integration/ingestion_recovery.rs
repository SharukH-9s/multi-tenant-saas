//! T10: the sweeper and crash recovery. Every test holds `INGEST_GLOBAL`
//! because the sweeper acts on every tenant's rows.

use std::sync::Arc;

use axum::http::{Method, StatusCode};
use common_database::begin_system_tenant_tx;
use common_messaging::IngestionEvent;
use tenant_saas::{
    repositories::ingestion_repo::{self, ClaimResult},
    services::ingestion_service::{self, IngestionDeps, ProcessOutcome},
};
use test_support::{captured_logs, TestApp, INGEST_GLOBAL};
use uuid::Uuid;

use super::ingestion_support::{fixture, unique_worker, Fixture, SIMPLE_TXT};

/// A worker claims the job and then dies: it never finishes and never lets go.
async fn claim_and_crash(fix: &Fixture) {
    let mut tx = begin_system_tenant_tx(&fix.app.runtime_pool, fix.tenant_id)
        .await
        .unwrap();
    let claim = ingestion_repo::claim(&mut tx, fix.job_id, fix.document_id, "doomed-worker", 300)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(matches!(claim, ClaimResult::Claimed(_)), "{claim:?}");
}

fn succeeded(outcome: &ProcessOutcome) -> bool {
    matches!(outcome, ProcessOutcome::Succeeded { .. })
}

#[tokio::test]
async fn worker_crash_is_requeued_and_finished() {
    let _global = INGEST_GLOBAL.lock().await;
    let Some(fix) = fixture("simple.txt", SIMPLE_TXT).await else {
        return;
    };
    fix.publish_pending_events().await;
    claim_and_crash(&fix).await;
    assert!(fix.sweep().await.is_empty(), "lease still valid");

    fix.expire_lease().await;
    assert_eq!(fix.sweep().await, ["requeued"]);

    assert_eq!(fix.job_status().await, "pending");
    assert_eq!(fix.document_state().await.0, "retry_scheduled");
    assert_eq!(fix.unpublished_events().await, 1);

    let outcome = ingestion_service::process(&fix.deps, &fix.event).await;
    assert!(succeeded(&outcome), "{outcome:?}");
    assert_eq!(fix.attempts().await, 2);
    assert_eq!(fix.document_state().await.0, "ready");
}

#[tokio::test]
async fn sql_built_event_parses_as_an_ingestion_event() {
    let _global = INGEST_GLOBAL.lock().await;
    let Some(fix) = fixture("simple.txt", SIMPLE_TXT).await else {
        return;
    };
    fix.publish_pending_events().await;
    claim_and_crash(&fix).await;
    fix.expire_lease().await;
    assert_eq!(fix.sweep().await, ["requeued"]);

    let payload: String = sqlx::query_scalar(
        "SELECT payload::text FROM outbox_events \
          WHERE aggregate_id = $1 AND published_at IS NULL",
    )
    .bind(fix.job_id)
    .fetch_one(fix.app.owner())
    .await
    .unwrap();

    // The Rust and SQL producers of this JSON must not drift apart.
    assert_eq!(IngestionEvent::parse(&payload).unwrap(), fix.event);
}

#[tokio::test]
async fn crash_on_the_final_attempt_fails_the_job() {
    let _global = INGEST_GLOBAL.lock().await;
    let Some(fix) = fixture("simple.txt", SIMPLE_TXT).await else {
        return;
    };
    fix.publish_pending_events().await;
    fix.set_max_attempts(1).await;
    claim_and_crash(&fix).await;
    fix.expire_lease().await;

    assert_eq!(fix.sweep().await, ["failed"]);
    assert_eq!(fix.job_status().await, "failed");
    let (status, code) = fix.document_state().await;
    assert_eq!(status, "failed");
    assert_eq!(code.as_deref(), Some("ATTEMPTS_EXHAUSTED"));
}

#[tokio::test]
async fn lost_doorbell_is_announced_once() {
    let _global = INGEST_GLOBAL.lock().await;
    let Some(fix) = fixture("simple.txt", SIMPLE_TXT).await else {
        return;
    };
    // The event vanished (stream trimmed, Redis flushed) and the job is old.
    sqlx::query("DELETE FROM outbox_events WHERE aggregate_id = $1")
        .bind(fix.job_id)
        .execute(fix.app.owner())
        .await
        .unwrap();
    sqlx::query(
        "UPDATE ingestion_jobs SET last_enqueued_at = now() - interval '10 minutes' \
          WHERE id = $1",
    )
    .bind(fix.job_id)
    .execute(fix.app.owner())
    .await
    .unwrap();

    assert_eq!(fix.sweep().await, ["requeued"]);
    assert_eq!(fix.unpublished_events().await, 1);
    assert!(fix.sweep().await.is_empty(), "already announced");
    assert_eq!(fix.unpublished_events().await, 1);
}

#[tokio::test]
async fn unpublished_event_blocks_a_duplicate_announcement() {
    let _global = INGEST_GLOBAL.lock().await;
    let Some(fix) = fixture("simple.txt", SIMPLE_TXT).await else {
        return;
    };
    // The upload's own event is still waiting for the publisher.
    sqlx::query(
        "UPDATE ingestion_jobs SET last_enqueued_at = now() - interval '10 minutes' \
          WHERE id = $1",
    )
    .bind(fix.job_id)
    .execute(fix.app.owner())
    .await
    .unwrap();

    for _ in 0..2 {
        assert!(fix.sweep().await.is_empty());
    }
    assert_eq!(fix.unpublished_events().await, 1, "no pile of duplicates");
}

#[tokio::test]
async fn duplicate_events_after_a_publisher_crash_are_harmless() {
    let Some(fix) = fixture("simple.txt", SIMPLE_TXT).await else {
        return;
    };

    // The publisher crashed after XADD, so the same event arrives twice.
    let first = ingestion_service::process(&fix.deps, &fix.event).await;
    let second = ingestion_service::process(&fix.deps, &fix.event).await;

    assert!(succeeded(&first), "{first:?}");
    assert_eq!(second, ProcessOutcome::Skipped);
    let (_, chunks): (String, i32) =
        sqlx::query_as("SELECT status, chunk_count FROM documents WHERE id = $1")
            .bind(fix.document_id)
            .fetch_one(fix.app.owner())
            .await
            .unwrap();
    assert_eq!(fix.chunk_count().await, i64::from(chunks));
}

#[tokio::test]
async fn active_job_of_a_deleted_document_is_cancelled() {
    let _global = INGEST_GLOBAL.lock().await;
    let Some(fix) = fixture("simple.txt", SIMPLE_TXT).await else {
        return;
    };
    sqlx::query("UPDATE documents SET status = 'deleting', deleted_at = now() WHERE id = $1")
        .bind(fix.document_id)
        .execute(fix.app.owner())
        .await
        .unwrap();

    assert_eq!(fix.sweep().await, ["cancelled"]);
    assert_eq!(fix.job_status().await, "cancelled");
}

#[tokio::test]
async fn sweeping_as_app_runtime_does_not_open_other_tenants_rows() {
    let _global = INGEST_GLOBAL.lock().await;
    let Some(fix) = fixture("simple.txt", SIMPLE_TXT).await else {
        return;
    };

    // The definer function works for the runtime role...
    ingestion_repo::sweep(&fix.app.runtime_pool, 10, 300)
        .await
        .expect("app_runtime may call the sweeper");

    // ...but the same role still sees no jobs of its own without a tenant.
    let visible: i64 = sqlx::query_scalar("SELECT count(*) FROM ingestion_jobs")
        .fetch_one(&fix.app.runtime_pool)
        .await
        .unwrap();
    assert_eq!(visible, 0);
}

#[tokio::test]
async fn concurrent_claims_completions_deletes_and_sweeps_do_not_deadlock() {
    let _global = INGEST_GLOBAL.lock().await;
    let Some(app) = TestApp::spawn().await else {
        return;
    };
    let session = app.create_session().await;
    let tenant_id = app
        .seed_membership(session.user_id, "admin", "active")
        .await;
    let deps = Arc::new(IngestionDeps::new(app.state.clone(), unique_worker()));

    let mut documents = Vec::new();
    for _ in 0..6 {
        let content = format!("deadlock test {}", Uuid::new_v4());
        let (status, body) = app
            .upload_file(
                tenant_id,
                &session.access_token,
                "d.txt",
                content.as_bytes(),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let document_id: Uuid = body["id"].as_str().unwrap().parse().unwrap();
        let job_id: Uuid =
            sqlx::query_scalar("SELECT id FROM ingestion_jobs WHERE document_id = $1")
                .bind(document_id)
                .fetch_one(app.owner())
                .await
                .unwrap();
        documents.push(IngestionEvent::new(job_id, tenant_id, document_id));
    }

    let mut processing = Vec::new();
    let mut deleting = Vec::new();
    let mut sweeping = Vec::new();
    for (i, event) in documents.iter().enumerate() {
        let doc = event.document_id;
        let (deps, event) = (deps.clone(), event.clone());
        processing.push(tokio::spawn(async move {
            ingestion_service::process(&deps, &event).await
        }));
        if i % 2 == 0 {
            let (app, token, doc) = (app.clone(), session.access_token.clone(), doc);
            deleting.push(tokio::spawn(async move {
                app.request(
                    Method::DELETE,
                    &format!("/api/v1/tenants/{tenant_id}/documents/{doc}"),
                    None,
                    Some(&token),
                )
                .await
                .0
            }));
        }
        let pool = app.state.db.clone();
        sweeping.push(tokio::spawn(async move {
            ingestion_repo::sweep(&pool, 500, 300).await
        }));
    }

    for task in processing {
        let outcome = task.await.unwrap();
        assert_ne!(outcome, ProcessOutcome::Unavailable, "a query failed");
    }
    for task in deleting {
        let status = task.await.unwrap();
        assert!(
            matches!(status, StatusCode::NO_CONTENT | StatusCode::NOT_FOUND),
            "{status}"
        );
    }
    for task in sweeping {
        task.await.unwrap().expect("sweep must not fail");
    }
    assert!(
        !captured_logs().contains("deadlock detected"),
        "lock order violated (D4)"
    );
}
