//! T2: an accepted upload creates one job and one outbox event in the same
//! transaction; deleting a document removes its chunks in the same commit as
//! `deleting`.

use axum::http::{Method, StatusCode};
use serde_json::{json, Value};
use test_support::{TestApp, INGEST_GLOBAL};
use uuid::Uuid;

const TEXT: &[u8] = b"hello ingestion pipeline";

struct JobRow {
    id: Uuid,
    status: String,
    attempts: i32,
    max_attempts: i32,
}

async fn jobs_for(app: &TestApp, tenant_id: Uuid) -> Vec<JobRow> {
    let rows: Vec<(Uuid, String, i32, i32)> = sqlx::query_as(
        "SELECT id, status, attempts, max_attempts FROM ingestion_jobs WHERE tenant_id = $1",
    )
    .bind(tenant_id)
    .fetch_all(app.owner())
    .await
    .unwrap();
    rows.into_iter()
        .map(|(id, status, attempts, max_attempts)| JobRow {
            id,
            status,
            attempts,
            max_attempts,
        })
        .collect()
}

async fn events_for(app: &TestApp, tenant_id: Uuid) -> Vec<(Uuid, String, String, Value, bool)> {
    sqlx::query_as(
        "SELECT aggregate_id, aggregate_type, event_type, payload, published_at IS NOT NULL \
           FROM outbox_events WHERE tenant_id = $1",
    )
    .bind(tenant_id)
    .fetch_all(app.owner())
    .await
    .unwrap()
}

async fn assert_one_job_and_event(app: &TestApp, tenant_id: Uuid, doc_id: Uuid) {
    let jobs = jobs_for(app, tenant_id).await;
    assert_eq!(jobs.len(), 1, "exactly one job");
    let job = &jobs[0];
    assert_eq!(job.status, "pending");
    assert_eq!(job.attempts, 0);
    assert_eq!(job.max_attempts, 5);

    let events = events_for(app, tenant_id).await;
    assert_eq!(events.len(), 1, "exactly one outbox event");
    let (aggregate_id, aggregate_type, event_type, payload, published) = &events[0];
    assert_eq!(*aggregate_id, job.id);
    assert_eq!(aggregate_type, "ingestion_job");
    assert_eq!(event_type, "ingestion.requested");
    assert!(!published, "nothing publishes in this test");
    assert_eq!(
        *payload,
        json!({
            "v": 1,
            "job_id": job.id.to_string(),
            "tenant_id": tenant_id.to_string(),
            "document_id": doc_id.to_string(),
        })
    );
}

async fn upload(app: &TestApp, tenant_id: Uuid, token: &str, bytes: &[u8]) -> (StatusCode, Value) {
    app.upload_file(tenant_id, token, "notes.txt", bytes, None)
        .await
}

fn doc_id(body: &Value) -> Uuid {
    body["id"].as_str().expect("id").parse().expect("uuid")
}

#[tokio::test]
async fn upload_creates_pending_job_and_outbox_event() {
    let _global = INGEST_GLOBAL.lock().await;
    let Some(app) = TestApp::spawn().await else {
        return;
    };
    let session = app.create_session().await;
    let tenant_id = app
        .seed_membership(session.user_id, "admin", "active")
        .await;

    let (status, body) = upload(&app, tenant_id, &session.access_token, TEXT).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_one_job_and_event(&app, tenant_id, doc_id(&body)).await;
}

#[tokio::test]
async fn upload_succeeds_with_dead_redis() {
    let _global = INGEST_GLOBAL.lock().await;
    let Some(app) = TestApp::spawn_with_dead_redis().await else {
        return;
    };
    let session = app.create_session().await;
    let tenant_id = app
        .seed_membership(session.user_id, "admin", "active")
        .await;

    let (status, body) = upload(&app, tenant_id, &session.access_token, TEXT).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_one_job_and_event(&app, tenant_id, doc_id(&body)).await;
}

#[tokio::test]
async fn duplicate_upload_enqueues_nothing() {
    let _global = INGEST_GLOBAL.lock().await;
    let Some(app) = TestApp::spawn().await else {
        return;
    };
    let session = app.create_session().await;
    let tenant_id = app
        .seed_membership(session.user_id, "admin", "active")
        .await;

    let (first, body) = upload(&app, tenant_id, &session.access_token, TEXT).await;
    assert_eq!(first, StatusCode::ACCEPTED, "{body}");
    let (second, dup_body) = upload(&app, tenant_id, &session.access_token, TEXT).await;
    assert_eq!(second, StatusCode::OK, "{dup_body}");
    assert_eq!(doc_id(&body), doc_id(&dup_body));

    assert_one_job_and_event(&app, tenant_id, doc_id(&body)).await;
}

/// Injects the failure at the database. The constraint is scoped to this test's
/// tenant, so uploads in parallel tests are unaffected (NOT VALID still checks
/// new rows).
#[tokio::test]
async fn enqueue_failure_rolls_back_document_and_removes_object() {
    let Some(app) = TestApp::spawn().await else {
        return;
    };
    let session = app.create_session().await;
    let tenant_id = app
        .seed_membership(session.user_id, "admin", "active")
        .await;

    let constraint = format!("outbox_inject_fail_{}", tenant_id.simple());
    // Test-only DDL built from a constant prefix and a UUID. Drop first so a
    // leftover from an aborted run can never cause "already exists".
    let drop_sql = format!("ALTER TABLE outbox_events DROP CONSTRAINT IF EXISTS {constraint}");
    sqlx::query(sqlx::AssertSqlSafe(drop_sql.clone()))
        .execute(app.owner())
        .await
        .expect("drop stale constraint");
    let add = format!(
        "ALTER TABLE outbox_events ADD CONSTRAINT {constraint} \
         CHECK (tenant_id IS DISTINCT FROM '{tenant_id}') NOT VALID"
    );
    sqlx::query(sqlx::AssertSqlSafe(add))
        .execute(app.owner())
        .await
        .expect("add blocking constraint");

    let (status, _body) = upload(&app, tenant_id, &session.access_token, TEXT).await;

    sqlx::query(sqlx::AssertSqlSafe(drop_sql))
        .execute(app.owner())
        .await
        .expect("drop blocking constraint");

    assert!(status.is_server_error(), "expected 5xx, got {status}");

    let (documents,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM documents WHERE tenant_id = $1")
            .bind(tenant_id)
            .fetch_one(app.owner())
            .await
            .unwrap();
    assert_eq!(documents, 0, "document insert must roll back");
    assert!(jobs_for(&app, tenant_id).await.is_empty(), "no job");
    assert!(events_for(&app, tenant_id).await.is_empty(), "no event");

    let objects = app
        .state
        .storage
        .list(&format!("tenants/{tenant_id}/"))
        .await
        .unwrap();
    assert!(objects.is_empty(), "uploaded object must be deleted");
}

#[tokio::test]
async fn delete_removes_chunks_in_the_same_commit() {
    let Some(app) = TestApp::spawn().await else {
        return;
    };
    let session = app.create_session().await;
    let tenant_id = app
        .seed_membership(session.user_id, "admin", "active")
        .await;
    let (status, body) = upload(&app, tenant_id, &session.access_token, TEXT).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let doc = doc_id(&body);

    let zeros = format!("[{}]", vec!["0.0"; 1536].join(","));
    for index in 0..2 {
        sqlx::query(
            "INSERT INTO document_chunks \
                (tenant_id, document_id, chunk_index, content, token_count, \
                 embedding, embedding_model, embedding_version) \
             VALUES ($1, $2, $3, 'chunk', 5, $4::vector, 'fake-model', 'v1')",
        )
        .bind(tenant_id)
        .bind(doc)
        .bind(index)
        .bind(&zeros)
        .execute(app.owner())
        .await
        .expect("seed chunk");
    }

    let (status, body) = app
        .request(
            Method::DELETE,
            &format!("/api/v1/tenants/{tenant_id}/documents/{doc}"),
            None,
            Some(&session.access_token),
        )
        .await;
    assert!(status.is_success(), "delete failed: {status} {body}");

    let (chunks,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM document_chunks WHERE document_id = $1")
            .bind(doc)
            .fetch_one(app.owner())
            .await
            .unwrap();
    assert_eq!(chunks, 0);
    let row = app.document_row(tenant_id, doc).await.expect("row kept");
    assert!(row.deleted_at.is_some());
}
