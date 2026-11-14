//! T10: transient failures in the worker, and the manual retry endpoint.

use axum::http::{Method, StatusCode};
use chrono::Utc;
use serde_json::Value;
use tenant_saas::{
    domain::ingestion::FailureCode,
    services::ingestion_service::{self, ProcessOutcome},
};
use test_support::{Session, TestApp, INGEST_GLOBAL};
use uuid::Uuid;

use super::ingestion_support::{fixture, ScriptedEmbedder, Step, CORRUPT_PDF, SIMPLE_TXT};

// ---------------------------------------------------------------------------
// Worker: transient failures are retried, permanent ones are not
// ---------------------------------------------------------------------------

#[tokio::test]
async fn transient_failures_retry_until_success() {
    let _global = INGEST_GLOBAL.lock().await;
    let Some(fix) = fixture("simple.txt", SIMPLE_TXT).await else {
        return;
    };
    fix.publish_pending_events().await;
    let embedder = ScriptedEmbedder::new([Step::Transient(None), Step::Transient(None)]);
    let deps = fix.deps_with(embedder);

    for round in 1..=2 {
        let outcome = ingestion_service::process(&deps, &fix.event).await;
        assert!(
            matches!(outcome, ProcessOutcome::RetryScheduled { .. }),
            "round {round}: {outcome:?}"
        );
        assert_eq!(fix.job_status().await, "retry_scheduled");
        let (status, code) = fix.document_state().await;
        assert_eq!(status, "retry_scheduled");
        assert_eq!(code.as_deref(), Some("EMBEDDING_UNAVAILABLE"));

        // A retry_scheduled job is not claimable: the early duplicate cannot
        // skip the backoff (D1).
        assert_eq!(
            ingestion_service::process(&deps, &fix.event).await,
            ProcessOutcome::Skipped
        );
        assert_eq!(fix.attempts().await, round);

        // Only the sweeper brings it back, and only once it is due.
        assert!(fix.sweep().await.is_empty(), "not due yet");
        fix.make_retry_due().await;
        assert_eq!(fix.sweep().await, ["requeued"]);
        assert_eq!(fix.job_status().await, "pending");
        fix.publish_pending_events().await;
    }

    let outcome = ingestion_service::process(&deps, &fix.event).await;
    assert!(
        matches!(outcome, ProcessOutcome::Succeeded { .. }),
        "{outcome:?}"
    );
    assert_eq!(fix.attempts().await, 3);
    assert_eq!(fix.document_state().await, ("ready".into(), None));
    assert!(fix.chunk_count().await > 0);
}

#[tokio::test]
async fn retry_after_hint_delays_the_retry() {
    let Some(fix) = fixture("simple.txt", SIMPLE_TXT).await else {
        return;
    };
    let embedder =
        ScriptedEmbedder::new([Step::Transient(Some(std::time::Duration::from_secs(120)))]);
    let deps = fix.deps_with(embedder);

    let ProcessOutcome::RetryScheduled { at } = ingestion_service::process(&deps, &fix.event).await
    else {
        panic!("expected RetryScheduled");
    };
    assert!(
        at >= Utc::now() + chrono::Duration::seconds(115),
        "retry due too early: {at}"
    );
}

#[tokio::test]
async fn transient_failure_on_last_attempt_keeps_original_code() {
    let Some(fix) = fixture("simple.txt", SIMPLE_TXT).await else {
        return;
    };
    fix.set_max_attempts(1).await;
    let deps = fix.deps_with(ScriptedEmbedder::new([Step::Transient(None)]));

    assert_eq!(
        ingestion_service::process(&deps, &fix.event).await,
        ProcessOutcome::Failed {
            code: FailureCode::EmbeddingUnavailable
        }
    );
    assert_eq!(fix.job_status().await, "failed");
    let (status, code) = fix.document_state().await;
    assert_eq!(status, "failed");
    assert_eq!(code.as_deref(), Some("EMBEDDING_UNAVAILABLE"));
}

#[tokio::test]
async fn permanent_failure_is_not_retried() {
    let Some(fix) = fixture("broken.pdf", CORRUPT_PDF).await else {
        return;
    };

    assert_eq!(
        ingestion_service::process(&fix.deps, &fix.event).await,
        ProcessOutcome::Failed {
            code: FailureCode::ParseFailed
        }
    );
    assert_eq!(fix.job_status().await, "failed");
    assert_eq!(fix.attempts().await, 1);
}

// ---------------------------------------------------------------------------
// Retry endpoint
// ---------------------------------------------------------------------------

struct Team {
    app: TestApp,
    tenant_id: Uuid,
    owner: Session,
    admin: Session,
    member_a: Session,
    member_b: Session,
    viewer: Session,
}

async fn team() -> Option<Team> {
    let app = TestApp::spawn().await?;
    let tenant_id = app.create_tenant().await;
    let mut sessions = Vec::new();
    for role in ["owner", "admin", "member", "member", "viewer"] {
        let session = app.create_session().await;
        app.add_member(tenant_id, session.user_id, role, "active")
            .await;
        sessions.push(session);
    }
    let mut it = sessions.into_iter();
    Some(Team {
        app,
        tenant_id,
        owner: it.next()?,
        admin: it.next()?,
        member_a: it.next()?,
        member_b: it.next()?,
        viewer: it.next()?,
    })
}

impl Team {
    /// Uploads a unique document as `who` and returns its id.
    async fn upload(&self, who: &Session) -> Uuid {
        let content = format!("retry endpoint test {}", Uuid::new_v4());
        let (status, body) = self
            .app
            .upload_file(
                self.tenant_id,
                &who.access_token,
                "doc.txt",
                content.as_bytes(),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        body["id"].as_str().unwrap().parse().unwrap()
    }

    /// Puts the document and its job into the state a failed run leaves.
    async fn make_failed(&self, doc: Uuid) {
        sqlx::query(
            "UPDATE ingestion_jobs SET status = 'failed', completed_at = now(), \
                    last_error_code = 'PARSE_FAILED' WHERE document_id = $1",
        )
        .bind(doc)
        .execute(self.app.owner())
        .await
        .unwrap();
        self.set_document_status(doc, "failed").await;
        sqlx::query("UPDATE documents SET failure_code = 'PARSE_FAILED' WHERE id = $1")
            .bind(doc)
            .execute(self.app.owner())
            .await
            .unwrap();
    }

    async fn set_document_status(&self, doc: Uuid, status: &str) {
        sqlx::query("UPDATE documents SET status = $2 WHERE id = $1")
            .bind(doc)
            .bind(status)
            .execute(self.app.owner())
            .await
            .unwrap();
    }

    async fn retry_as(&self, who: &Session, doc: Uuid) -> (StatusCode, Value) {
        self.retry_path(&who.access_token, self.tenant_id, doc)
            .await
    }

    async fn retry_path(&self, token: &str, tenant: Uuid, doc: Uuid) -> (StatusCode, Value) {
        self.app
            .request(
                Method::POST,
                &format!("/api/v1/tenants/{tenant}/documents/{doc}/retry"),
                None,
                Some(token),
            )
            .await
    }

    async fn job_count(&self, doc: Uuid) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM ingestion_jobs WHERE document_id = $1")
            .bind(doc)
            .fetch_one(self.app.owner())
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn owner_admin_and_own_member_can_retry() {
    let Some(t) = team().await else { return };

    for who in [&t.owner, &t.admin, &t.member_a] {
        let doc = t.upload(who).await;
        t.make_failed(doc).await;

        let (status, body) = t.retry_as(who, doc).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        assert_eq!(body["status"], "pending");
        assert_eq!(body["failure_code"], Value::Null);

        // A new job next to the old failed one, and its event.
        assert_eq!(t.job_count(doc).await, 2);
        let (active_job,): (Uuid,) = sqlx::query_as(
            "SELECT id FROM ingestion_jobs WHERE document_id = $1 AND status = 'pending'",
        )
        .bind(doc)
        .fetch_one(t.app.owner())
        .await
        .unwrap();
        let events: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM outbox_events WHERE aggregate_id = $1 AND published_at IS NULL",
        )
        .bind(active_job)
        .fetch_one(t.app.owner())
        .await
        .unwrap();
        assert_eq!(events, 1);
    }
}

#[tokio::test]
async fn member_cannot_retry_another_members_document() {
    let Some(t) = team().await else { return };
    let doc = t.upload(&t.member_b).await;
    t.make_failed(doc).await;

    let (status, _) = t.retry_as(&t.member_a, doc).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(t.job_count(doc).await, 1, "no new job");
}

#[tokio::test]
async fn viewer_cannot_retry() {
    let Some(t) = team().await else { return };
    let doc = t.upload(&t.owner).await;
    t.make_failed(doc).await;

    let (status, _) = t.retry_as(&t.viewer, doc).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn only_failed_documents_can_be_retried() {
    let Some(t) = team().await else { return };

    for state in ["pending", "processing", "ready", "retry_scheduled"] {
        let doc = t.upload(&t.owner).await;
        t.set_document_status(doc, state).await;

        let (status, body) = t.retry_as(&t.owner, doc).await;
        assert_eq!(status, StatusCode::CONFLICT, "{state}: {body}");
        assert_eq!(body["error"]["code"], "DOCUMENT_NOT_RETRYABLE", "{state}");
        assert_eq!(t.job_count(doc).await, 1, "{state}: no new job");
    }
}

#[tokio::test]
async fn hidden_documents_look_like_missing_ones() {
    let Some(t) = team().await else { return };

    let deleted = t.upload(&t.owner).await;
    t.make_failed(deleted).await;
    sqlx::query("UPDATE documents SET status = 'deleted', deleted_at = now() WHERE id = $1")
        .bind(deleted)
        .execute(t.app.owner())
        .await
        .unwrap();

    // A real, retryable document that belongs to another tenant.
    let other = team().await.expect("second team");
    let foreign = other.upload(&other.owner).await;
    other.make_failed(foreign).await;

    let mut seen = Vec::new();
    for doc in [deleted, foreign, Uuid::new_v4()] {
        let (status, body) = t.retry_as(&t.owner, doc).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        seen.push((
            body["error"]["code"].clone(),
            body["error"]["message"].clone(),
        ));
    }
    assert!(seen.windows(2).all(|w| w[0] == w[1]), "{seen:?}");
    assert_eq!(other.job_count(foreign).await, 1, "other tenant untouched");
}

#[tokio::test]
async fn concurrent_retries_create_one_job() {
    let Some(t) = team().await else { return };
    let doc = t.upload(&t.owner).await;
    t.make_failed(doc).await;

    let token = t.owner.access_token.clone();
    let tasks: Vec<_> = (0..5)
        .map(|_| {
            let (app, token, tenant) = (t.app.clone(), token.clone(), t.tenant_id);
            tokio::spawn(async move {
                app.request(
                    Method::POST,
                    &format!("/api/v1/tenants/{tenant}/documents/{doc}/retry"),
                    None,
                    Some(&token),
                )
                .await
                .0
            })
        })
        .collect();

    let mut statuses = Vec::new();
    for task in tasks {
        statuses.push(task.await.unwrap());
    }
    let accepted = statuses
        .iter()
        .filter(|s| **s == StatusCode::ACCEPTED)
        .count();
    let conflicts = statuses
        .iter()
        .filter(|s| **s == StatusCode::CONFLICT)
        .count();
    assert_eq!((accepted, conflicts), (1, 4), "{statuses:?}");
    assert_eq!(t.job_count(doc).await, 2);
}
