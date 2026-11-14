//! T9: Ingestion pipeline and idempotent persistence integration tests.
//!
//! Tests claim, download, parse, chunk, embed, and persist against a real DB
//! with RLS enforced under `app_runtime`, verifying idempotency, lease
//! enforcement, stale worker protection, and privacy logging.

use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use async_trait::async_trait;
use axum::http::StatusCode;
use bytes::Bytes;
use common_database::begin_system_tenant_tx;
use common_messaging::{IngestionEvent, StreamClient, StreamConfig};
use sqlx::Row;
use tenant_saas::{
    ai::{EmbeddingError, EmbeddingProvider},
    domain::ingestion::FailureCode,
    repositories::ingestion_repo,
    services::ingestion_service::{self, IngestionDeps, ProcessOutcome},
};
use test_support::{captured_logs, TestApp};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const FIXTURE_SIMPLE_TXT: &[u8] =
    include_bytes!("../../apps/tenant-saas/tests/fixtures/ingestion/simple.txt");
const FIXTURE_HEADINGS_MD: &[u8] =
    include_bytes!("../../apps/tenant-saas/tests/fixtures/ingestion/headings.md");
const FIXTURE_TWO_PAGES_PDF: &[u8] =
    include_bytes!("../../apps/tenant-saas/tests/fixtures/ingestion/two_pages.pdf");
const FIXTURE_CORRUPT_PDF: &[u8] =
    include_bytes!("../../apps/tenant-saas/tests/fixtures/ingestion/corrupt.pdf");
const FIXTURE_EMPTY_PDF: &[u8] =
    include_bytes!("../../apps/tenant-saas/tests/fixtures/ingestion/empty_text_layer.pdf");

struct Fixture {
    app: TestApp,
    tenant_id: Uuid,
    document_id: Uuid,
    job_id: Uuid,
    event: IngestionEvent,
    deps: IngestionDeps,
}

async fn create_fixture(filename: &str, content: &[u8]) -> Option<Fixture> {
    let app = TestApp::spawn().await?;
    let session = app.create_session().await;
    let tenant_id = app
        .seed_membership(session.user_id, "admin", "active")
        .await;

    let (status, body) = app
        .upload_file(
            tenant_id,
            &session.access_token,
            filename,
            content,
            Some("Pipeline Test Document"),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");

    let document_id: Uuid = body["id"].as_str().unwrap().parse().unwrap();
    let (job_id,): (Uuid,) =
        sqlx::query_as("SELECT id FROM ingestion_jobs WHERE tenant_id = $1 AND document_id = $2")
            .bind(tenant_id)
            .bind(document_id)
            .fetch_one(app.owner())
            .await
            .unwrap();

    let event = IngestionEvent::new(job_id, tenant_id, document_id);
    let worker_id = format!("worker-test-{}", Uuid::new_v4().simple());
    let deps = IngestionDeps::new(app.state.clone(), worker_id);

    Some(Fixture {
        app,
        tenant_id,
        document_id,
        job_id,
        event,
        deps,
    })
}

// ---------------------------------------------------------------------------
// Test 1: Happy paths (TXT, MD, PDF)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn happy_paths_txt_md_pdf() {
    // 1. Plain Text
    let fix = match create_fixture("simple.txt", FIXTURE_SIMPLE_TXT).await {
        Some(f) => f,
        None => return,
    };

    let outcome = ingestion_service::process(&fix.deps, &fix.event).await;
    match outcome {
        ProcessOutcome::Succeeded { chunks } => {
            assert!(chunks > 0, "expected at least one chunk");
        }
        other => panic!("expected Succeeded, got {other:?}"),
    }

    let (doc_status, chunk_count, failure_code): (String, Option<i32>, Option<String>) =
        sqlx::query_as("SELECT status, chunk_count, failure_code FROM documents WHERE id = $1")
            .bind(fix.document_id)
            .fetch_one(fix.app.owner())
            .await
            .unwrap();
    assert_eq!(doc_status, "ready");
    assert!(chunk_count.unwrap() > 0);
    assert!(failure_code.is_none());

    let rows = sqlx::query(
        "SELECT chunk_index, token_count, vector_dims(embedding) as dims, \
                embedding_model, embedding_version \
           FROM document_chunks \
          WHERE document_id = $1 \
          ORDER BY chunk_index ASC",
    )
    .bind(fix.document_id)
    .fetch_all(fix.app.owner())
    .await
    .unwrap();

    assert_eq!(rows.len(), chunk_count.unwrap() as usize);
    for (i, row) in rows.iter().enumerate() {
        let chunk_index: i32 = row.get("chunk_index");
        let dims: i32 = row.get("dims");
        let model: String = row.get("embedding_model");
        let version: String = row.get("embedding_version");

        assert_eq!(chunk_index, i as i32);
        assert_eq!(dims, 1536);
        assert_eq!(model, "text-embedding-3-small");
        assert_eq!(version, "p4-v1");
    }

    // 2. Markdown
    let fix_md = match create_fixture("headings.md", FIXTURE_HEADINGS_MD).await {
        Some(f) => f,
        None => return,
    };
    let outcome_md = ingestion_service::process(&fix_md.deps, &fix_md.event).await;
    assert!(matches!(outcome_md, ProcessOutcome::Succeeded { .. }));

    let (doc_status_md,): (String,) = sqlx::query_as("SELECT status FROM documents WHERE id = $1")
        .bind(fix_md.document_id)
        .fetch_one(fix_md.app.owner())
        .await
        .unwrap();
    assert_eq!(doc_status_md, "ready");

    // 3. PDF
    let fix_pdf = match create_fixture("two_pages.pdf", FIXTURE_TWO_PAGES_PDF).await {
        Some(f) => f,
        None => return,
    };
    let outcome_pdf = ingestion_service::process(&fix_pdf.deps, &fix_pdf.event).await;
    assert!(matches!(outcome_pdf, ProcessOutcome::Succeeded { .. }));

    let (doc_status_pdf,): (String,) = sqlx::query_as("SELECT status FROM documents WHERE id = $1")
        .bind(fix_pdf.document_id)
        .fetch_one(fix_pdf.app.owner())
        .await
        .unwrap();
    assert_eq!(doc_status_pdf, "ready");
}

// ---------------------------------------------------------------------------
// Test 2: Same event twice, sequential
// ---------------------------------------------------------------------------

#[tokio::test]
async fn same_event_twice_sequential() {
    let fix = match create_fixture("simple.txt", FIXTURE_SIMPLE_TXT).await {
        Some(f) => f,
        None => return,
    };

    let outcome1 = ingestion_service::process(&fix.deps, &fix.event).await;
    assert!(matches!(outcome1, ProcessOutcome::Succeeded { .. }));

    let initial_chunk_ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM document_chunks WHERE document_id = $1 ORDER BY chunk_index ASC",
    )
    .bind(fix.document_id)
    .fetch_all(fix.app.owner())
    .await
    .unwrap();

    // Second execution with same event
    let outcome2 = ingestion_service::process(&fix.deps, &fix.event).await;
    assert_eq!(outcome2, ProcessOutcome::Skipped);

    let subsequent_chunk_ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM document_chunks WHERE document_id = $1 ORDER BY chunk_index ASC",
    )
    .bind(fix.document_id)
    .fetch_all(fix.app.owner())
    .await
    .unwrap();

    assert_eq!(initial_chunk_ids, subsequent_chunk_ids);
}

// ---------------------------------------------------------------------------
// Test 3: Same event, two concurrent tasks
// ---------------------------------------------------------------------------

#[tokio::test]
async fn same_event_two_concurrent_tasks() {
    let fix = match create_fixture("simple.txt", FIXTURE_SIMPLE_TXT).await {
        Some(f) => f,
        None => return,
    };

    let deps1 = IngestionDeps::new(fix.app.state.clone(), "worker-1".into());
    let deps2 = IngestionDeps::new(fix.app.state.clone(), "worker-2".into());
    let event1 = fix.event.clone();
    let event2 = fix.event.clone();

    let task1 = tokio::spawn(async move { ingestion_service::process(&deps1, &event1).await });
    let task2 = tokio::spawn(async move { ingestion_service::process(&deps2, &event2).await });

    let (res1, res2) = tokio::join!(task1, task2);
    let (res1, res2) = (res1.unwrap(), res2.unwrap());

    let (succeeded, skipped) = match (&res1, &res2) {
        (ProcessOutcome::Succeeded { chunks }, ProcessOutcome::Skipped) => (*chunks, true),
        (ProcessOutcome::Skipped, ProcessOutcome::Succeeded { chunks }) => (*chunks, true),
        other => panic!("expected exactly one Succeeded and one Skipped, got {other:?}"),
    };
    assert!(skipped);

    let (doc_chunk_count,): (Option<i32>,) =
        sqlx::query_as("SELECT chunk_count FROM documents WHERE id = $1")
            .bind(fix.document_id)
            .fetch_one(fix.app.owner())
            .await
            .unwrap();
    assert_eq!(doc_chunk_count.unwrap() as usize, succeeded);

    let db_chunks: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM document_chunks WHERE document_id = $1")
            .bind(fix.document_id)
            .fetch_one(fix.app.owner())
            .await
            .unwrap();
    assert_eq!(db_chunks as usize, succeeded);
}

// ---------------------------------------------------------------------------
// Test 4: Duplicate after a forced reprocess
// ---------------------------------------------------------------------------

#[tokio::test]
async fn duplicate_after_forced_reprocess() {
    let fix = match create_fixture("simple.txt", FIXTURE_SIMPLE_TXT).await {
        Some(f) => f,
        None => return,
    };

    let outcome1 = ingestion_service::process(&fix.deps, &fix.event).await;
    assert!(matches!(outcome1, ProcessOutcome::Succeeded { .. }));

    // Reset job to pending and document to pending through owner pool
    sqlx::query(
        "UPDATE ingestion_jobs \
            SET status = 'pending', lease_token = NULL, locked_until = NULL, attempts = 0 \
          WHERE id = $1",
    )
    .bind(fix.job_id)
    .execute(fix.app.owner())
    .await
    .unwrap();

    sqlx::query("UPDATE documents SET status = 'pending' WHERE id = $1")
        .bind(fix.document_id)
        .execute(fix.app.owner())
        .await
        .unwrap();

    // Process again with same event
    let outcome2 = ingestion_service::process(&fix.deps, &fix.event).await;
    assert!(matches!(outcome2, ProcessOutcome::Succeeded { .. }));

    let (status, count): (String, Option<i32>) =
        sqlx::query_as("SELECT status, chunk_count FROM documents WHERE id = $1")
            .bind(fix.document_id)
            .fetch_one(fix.app.owner())
            .await
            .unwrap();
    assert_eq!(status, "ready");

    let chunk_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM document_chunks WHERE document_id = $1")
            .bind(fix.document_id)
            .fetch_one(fix.app.owner())
            .await
            .unwrap();
    assert_eq!(chunk_rows, count.unwrap() as i64);
}

// ---------------------------------------------------------------------------
// Test 5: Stale worker superseded
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stale_worker_superseded() {
    // Expires a lease on purpose, so it must not overlap a sweep (sweeper tests).
    let _global = test_support::INGEST_GLOBAL.lock().await;
    let fix = match create_fixture("simple.txt", FIXTURE_SIMPLE_TXT).await {
        Some(f) => f,
        None => return,
    };

    // Worker A claims
    let mut tx_a = begin_system_tenant_tx(&fix.app.runtime_pool, fix.tenant_id)
        .await
        .unwrap();
    let claim_a = ingestion_repo::claim(&mut tx_a, fix.job_id, fix.document_id, "worker-a", 300)
        .await
        .unwrap();
    tx_a.commit().await.unwrap();

    let claimed_a = match claim_a {
        ingestion_repo::ClaimResult::Claimed(c) => c,
        other => panic!("expected Claimed, got {other:?}"),
    };

    // Expire Worker A's lease via owner pool
    sqlx::query(
        "UPDATE ingestion_jobs SET locked_until = now() - interval '1 second' WHERE id = $1",
    )
    .bind(fix.job_id)
    .execute(fix.app.owner())
    .await
    .unwrap();

    // Worker B claims
    let mut tx_b = begin_system_tenant_tx(&fix.app.runtime_pool, fix.tenant_id)
        .await
        .unwrap();
    let claim_b = ingestion_repo::claim(&mut tx_b, fix.job_id, fix.document_id, "worker-b", 300)
        .await
        .unwrap();
    tx_b.commit().await.unwrap();

    let claimed_b = match claim_b {
        ingestion_repo::ClaimResult::Claimed(c) => c,
        other => panic!("expected Claimed, got {other:?}"),
    };

    // Worker B completes
    let mut tx_b_persist = begin_system_tenant_tx(&fix.app.runtime_pool, fix.tenant_id)
        .await
        .unwrap();
    let emb_b = vec![0.1f32; 1536];
    let chunks_b = [tenant_saas::repositories::chunk_repo::NewChunk {
        index: 0,
        content: "chunk by b",
        token_count: 3,
        embedding: &emb_b,
        metadata: serde_json::json!({}),
    }];
    let ok_b = ingestion_repo::complete(
        &mut tx_b_persist,
        &claimed_b,
        "text-embedding-3-small",
        "p4-v1",
        &chunks_b,
    )
    .await
    .unwrap();
    assert!(ok_b);
    tx_b_persist.commit().await.unwrap();

    let chunk_ids_b: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM document_chunks WHERE document_id = $1")
            .bind(fix.document_id)
            .fetch_all(fix.app.owner())
            .await
            .unwrap();
    assert_eq!(chunk_ids_b.len(), 1);

    // Worker A now attempts to complete with its stale lease token
    let mut tx_a_persist = begin_system_tenant_tx(&fix.app.runtime_pool, fix.tenant_id)
        .await
        .unwrap();
    let emb_a = vec![0.2f32; 1536];
    let chunks_a = [tenant_saas::repositories::chunk_repo::NewChunk {
        index: 0,
        content: "chunk by a",
        token_count: 3,
        embedding: &emb_a,
        metadata: serde_json::json!({}),
    }];
    let ok_a = ingestion_repo::complete(
        &mut tx_a_persist,
        &claimed_a,
        "text-embedding-3-small",
        "p4-v1",
        &chunks_a,
    )
    .await
    .unwrap();
    assert!(!ok_a, "stale worker A must not complete");
    tx_a_persist.rollback().await.unwrap();

    // Verify B's chunk IDs survived
    let final_chunk_ids: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM document_chunks WHERE document_id = $1")
            .bind(fix.document_id)
            .fetch_all(fix.app.owner())
            .await
            .unwrap();
    assert_eq!(chunk_ids_b, final_chunk_ids);
}

// ---------------------------------------------------------------------------
// Test 6: Lease lost during embedding
// ---------------------------------------------------------------------------

struct LeaseExpiringEmbedder {
    calls: AtomicUsize,
    owner_pool: sqlx::PgPool,
    job_id: Uuid,
}

#[async_trait]
impl EmbeddingProvider for LeaseExpiringEmbedder {
    fn model(&self) -> &str {
        "lease-expiring"
    }
    fn dim(&self) -> usize {
        1536
    }
    async fn embed(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        let call_idx = self.calls.fetch_add(1, Ordering::SeqCst);
        if call_idx == 0 {
            // Invalidate the lease token in DB during batch 0
            sqlx::query("UPDATE ingestion_jobs SET lease_token = gen_random_uuid() WHERE id = $1")
                .bind(self.job_id)
                .execute(&self.owner_pool)
                .await
                .unwrap();
        }
        Ok(inputs.iter().map(|_| vec![0.1f32; 1536]).collect())
    }
}

#[tokio::test]
async fn lease_lost_during_embedding() {
    let fix = match create_fixture("headings.md", FIXTURE_HEADINGS_MD).await {
        Some(f) => f,
        None => return,
    };

    let embedder = Arc::new(LeaseExpiringEmbedder {
        calls: AtomicUsize::new(0),
        owner_pool: fix.app.owner().clone(),
        job_id: fix.job_id,
    });

    let state = fix.app.state.clone().with_embedder(embedder.clone());
    let deps = IngestionDeps::new(state, "lease-test-worker".into());

    let outcome = ingestion_service::process(&deps, &fix.event).await;
    assert_eq!(outcome, ProcessOutcome::Superseded);

    // Verify no further calls beyond the first
    assert_eq!(embedder.calls.load(Ordering::SeqCst), 1);
}

// ---------------------------------------------------------------------------
// Test 7: Document deleted after claim, before persist
// ---------------------------------------------------------------------------

struct DeletingEmbedder {
    owner_pool: sqlx::PgPool,
    document_id: Uuid,
}

#[async_trait]
impl EmbeddingProvider for DeletingEmbedder {
    fn model(&self) -> &str {
        "deleting"
    }
    fn dim(&self) -> usize {
        1536
    }
    async fn embed(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        // Soft delete the document during embedding
        sqlx::query("UPDATE documents SET status = 'deleted', deleted_at = now() WHERE id = $1")
            .bind(self.document_id)
            .execute(&self.owner_pool)
            .await
            .unwrap();
        Ok(inputs.iter().map(|_| vec![0.1f32; 1536]).collect())
    }
}

#[tokio::test]
async fn document_deleted_after_claim_before_persist() {
    let fix = match create_fixture("simple.txt", FIXTURE_SIMPLE_TXT).await {
        Some(f) => f,
        None => return,
    };

    let embedder = Arc::new(DeletingEmbedder {
        owner_pool: fix.app.owner().clone(),
        document_id: fix.document_id,
    });

    let state = fix.app.state.clone().with_embedder(embedder);
    let deps = IngestionDeps::new(state, "del-worker".into());

    let outcome = ingestion_service::process(&deps, &fix.event).await;
    assert_eq!(outcome, ProcessOutcome::Superseded);

    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM document_chunks WHERE document_id = $1")
            .bind(fix.document_id)
            .fetch_one(fix.app.owner())
            .await
            .unwrap();
    assert_eq!(count, 0);

    let deleted_at: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT deleted_at FROM documents WHERE id = $1")
            .bind(fix.document_id)
            .fetch_one(fix.app.owner())
            .await
            .unwrap();
    assert!(deleted_at.is_some());
}

// ---------------------------------------------------------------------------
// Test 8: Event with Tenant B and Tenant A's job
// ---------------------------------------------------------------------------

#[tokio::test]
async fn event_cross_tenant_mismatch_poison() {
    let fix = match create_fixture("simple.txt", FIXTURE_SIMPLE_TXT).await {
        Some(f) => f,
        None => return,
    };

    let other_tenant_id = Uuid::new_v4();
    let poison_event = IngestionEvent::new(fix.job_id, other_tenant_id, fix.document_id);

    let outcome = ingestion_service::process(&fix.deps, &poison_event).await;
    assert_eq!(outcome, ProcessOutcome::Poison);

    // Job and document remain untouched in Tenant A
    let (status,): (String,) = sqlx::query_as("SELECT status FROM ingestion_jobs WHERE id = $1")
        .bind(fix.job_id)
        .fetch_one(fix.app.owner())
        .await
        .unwrap();
    assert_eq!(status, "pending");
}

// ---------------------------------------------------------------------------
// Test 9: Object removed from storage
// ---------------------------------------------------------------------------

#[tokio::test]
async fn object_missing_from_storage() {
    let fix = match create_fixture("simple.txt", FIXTURE_SIMPLE_TXT).await {
        Some(f) => f,
        None => return,
    };

    let (object_key,): (String,) = sqlx::query_as("SELECT object_key FROM documents WHERE id = $1")
        .bind(fix.document_id)
        .fetch_one(fix.app.owner())
        .await
        .unwrap();

    fix.app.state.storage.delete(&object_key).await.unwrap();

    let outcome = ingestion_service::process(&fix.deps, &fix.event).await;
    assert_eq!(
        outcome,
        ProcessOutcome::Failed {
            code: FailureCode::ObjectMissing
        }
    );

    let (status, failure_code): (String, Option<String>) =
        sqlx::query_as("SELECT status, failure_code FROM documents WHERE id = $1")
            .bind(fix.document_id)
            .fetch_one(fix.app.owner())
            .await
            .unwrap();
    assert_eq!(status, "failed");
    assert_eq!(failure_code.as_deref(), Some("OBJECT_MISSING"));
}

// ---------------------------------------------------------------------------
// Test 10: Object bytes replaced
// ---------------------------------------------------------------------------

#[tokio::test]
async fn object_corrupt_mismatch() {
    let fix = match create_fixture("simple.txt", FIXTURE_SIMPLE_TXT).await {
        Some(f) => f,
        None => return,
    };

    let (object_key,): (String,) = sqlx::query_as("SELECT object_key FROM documents WHERE id = $1")
        .bind(fix.document_id)
        .fetch_one(fix.app.owner())
        .await
        .unwrap();

    fix.app
        .state
        .storage
        .put(&object_key, Bytes::from_static(b"tampered content"))
        .await
        .unwrap();

    let outcome = ingestion_service::process(&fix.deps, &fix.event).await;
    assert_eq!(
        outcome,
        ProcessOutcome::Failed {
            code: FailureCode::ObjectCorrupt
        }
    );

    let (status, failure_code): (String, Option<String>) =
        sqlx::query_as("SELECT status, failure_code FROM documents WHERE id = $1")
            .bind(fix.document_id)
            .fetch_one(fix.app.owner())
            .await
            .unwrap();
    assert_eq!(status, "failed");
    assert_eq!(failure_code.as_deref(), Some("OBJECT_CORRUPT"));
}

// ---------------------------------------------------------------------------
// Test 11: Corrupt PDF fixture
// ---------------------------------------------------------------------------

#[tokio::test]
async fn corrupt_pdf_fixture() {
    let fix = match create_fixture("corrupt.pdf", FIXTURE_CORRUPT_PDF).await {
        Some(f) => f,
        None => return,
    };

    let outcome = ingestion_service::process(&fix.deps, &fix.event).await;
    assert_eq!(
        outcome,
        ProcessOutcome::Failed {
            code: FailureCode::ParseFailed
        }
    );

    let (status, failure_code, failure_detail): (String, Option<String>, Option<String>) =
        sqlx::query_as("SELECT status, failure_code, failure_detail FROM documents WHERE id = $1")
            .bind(fix.document_id)
            .fetch_one(fix.app.owner())
            .await
            .unwrap();
    assert_eq!(status, "failed");
    assert_eq!(failure_code.as_deref(), Some("PARSE_FAILED"));
    let detail = failure_detail.unwrap();
    assert!(detail.contains("corrupt document"));
    assert!(!detail.contains("raw/"));
}

// ---------------------------------------------------------------------------
// Test 12: Empty-text-layer PDF
// ---------------------------------------------------------------------------

#[tokio::test]
async fn empty_text_layer_pdf() {
    let fix = match create_fixture("empty_text_layer.pdf", FIXTURE_EMPTY_PDF).await {
        Some(f) => f,
        None => return,
    };

    let outcome = ingestion_service::process(&fix.deps, &fix.event).await;
    assert_eq!(
        outcome,
        ProcessOutcome::Failed {
            code: FailureCode::DocumentEmpty
        }
    );

    let (status, failure_code): (String, Option<String>) =
        sqlx::query_as("SELECT status, failure_code FROM documents WHERE id = $1")
            .bind(fix.document_id)
            .fetch_one(fix.app.owner())
            .await
            .unwrap();
    assert_eq!(status, "failed");
    assert_eq!(failure_code.as_deref(), Some("DOCUMENT_EMPTY"));
}

// ---------------------------------------------------------------------------
// Test 13: Wrong embedding dimension
// ---------------------------------------------------------------------------

struct WrongDimEmbedder;

#[async_trait]
impl EmbeddingProvider for WrongDimEmbedder {
    fn model(&self) -> &str {
        "wrong-dim"
    }
    fn dim(&self) -> usize {
        1536
    }
    async fn embed(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        // Return 768 dimensions instead of 1536
        Ok(inputs.iter().map(|_| vec![0.1f32; 768]).collect())
    }
}

#[tokio::test]
async fn scripted_embedder_wrong_dimension() {
    let fix = match create_fixture("simple.txt", FIXTURE_SIMPLE_TXT).await {
        Some(f) => f,
        None => return,
    };

    let state = fix
        .app
        .state
        .clone()
        .with_embedder(Arc::new(WrongDimEmbedder));
    let deps = IngestionDeps::new(state, "dim-worker".into());

    let outcome = ingestion_service::process(&deps, &fix.event).await;
    assert_eq!(
        outcome,
        ProcessOutcome::Failed {
            code: FailureCode::EmbeddingInvalid
        }
    );

    let (status, failure_code): (String, Option<String>) =
        sqlx::query_as("SELECT status, failure_code FROM documents WHERE id = $1")
            .bind(fix.document_id)
            .fetch_one(fix.app.owner())
            .await
            .unwrap();
    assert_eq!(status, "failed");
    assert_eq!(failure_code.as_deref(), Some("EMBEDDING_INVALID_RESPONSE"));
}

// ---------------------------------------------------------------------------
// Test 14: Log privacy check
// ---------------------------------------------------------------------------

#[tokio::test]
async fn logs_contain_no_object_key_or_content() {
    let secret_sentence = "SecretSentenceUniqueToThisPrivacyCheck2026";
    let content = format!("Header\n\n{secret_sentence}\n\nFooter");

    let fix = match create_fixture("privacy.txt", content.as_bytes()).await {
        Some(f) => f,
        None => return,
    };

    let (object_key,): (String,) = sqlx::query_as("SELECT object_key FROM documents WHERE id = $1")
        .bind(fix.document_id)
        .fetch_one(fix.app.owner())
        .await
        .unwrap();

    let outcome = ingestion_service::process(&fix.deps, &fix.event).await;
    assert!(matches!(outcome, ProcessOutcome::Succeeded { .. }));

    let logs = captured_logs();
    assert!(
        !logs.contains(secret_sentence),
        "captured logs must never contain document text"
    );
    assert!(
        !logs.contains(&object_key),
        "captured logs must never contain object storage keys"
    );
}

// ---------------------------------------------------------------------------
// Test 15: Consumer end to end
// ---------------------------------------------------------------------------

#[tokio::test]
async fn consumer_end_to_end() {
    let app = match TestApp::spawn().await {
        Some(a) => a,
        None => return,
    };

    let stream = format!("test:consumer:{}", Uuid::new_v4());
    let streams = Arc::new(
        StreamClient::new_lazy(StreamConfig::new(
            &app.state.config.redis.url,
            &stream,
            "test-group",
        ))
        .unwrap(),
    );

    if !streams.health().await {
        assert!(
            std::env::var_os("CI").is_none(),
            "Redis must be reachable in CI for the consumer end-to-end test"
        );
        eprintln!("Redis unreachable: skipping consumer end-to-end test");
        return;
    }
    streams.ensure_group().await.unwrap();

    let cancel = CancellationToken::new();
    let deps = Arc::new(IngestionDeps::new(
        app.state.clone(),
        format!("consumer-test-{}", Uuid::new_v4().simple()),
    ));

    let consumer_handle = tokio::spawn(tenant_saas::workers::ingestion_consumer::run(
        deps.clone(),
        streams.clone(),
        cancel.clone(),
    ));

    let session = app.create_session().await;
    let tenant_id = app
        .seed_membership(session.user_id, "admin", "active")
        .await;

    let (status, body) = app
        .upload_file(
            tenant_id,
            &session.access_token,
            "consumer.txt",
            FIXTURE_SIMPLE_TXT,
            Some("Consumer Document"),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");

    let document_id: Uuid = body["id"].as_str().unwrap().parse().unwrap();
    let (job_id,): (Uuid,) =
        sqlx::query_as("SELECT id FROM ingestion_jobs WHERE tenant_id = $1 AND document_id = $2")
            .bind(tenant_id)
            .bind(document_id)
            .fetch_one(app.owner())
            .await
            .unwrap();

    let event = IngestionEvent::new(job_id, tenant_id, document_id);
    let outbox_id = Uuid::new_v4();

    streams.publish(&event, outbox_id).await.unwrap();

    // Wait up to 10 seconds for document to become ready
    let mut ready = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let (status,): (String,) = sqlx::query_as("SELECT status FROM documents WHERE id = $1")
            .bind(document_id)
            .fetch_one(app.owner())
            .await
            .unwrap();
        if status == "ready" {
            ready = true;
            break;
        }
    }
    assert!(ready, "document did not reach 'ready' status within 10s");

    // Check message is acknowledged (pending == 0)
    let stats = streams.stats().await.unwrap();
    assert_eq!(
        stats.pending, 0,
        "expected pending stream messages to be 0 after ack"
    );

    cancel.cancel();
    let _ = consumer_handle.await;

    // Clean up Redis stream
    let client = redis::Client::open(app.state.config.redis.url.as_str()).unwrap();
    if let Ok(mut conn) = client.get_multiplexed_async_connection().await {
        let _: Result<(), _> = redis::AsyncCommands::del(&mut conn, &stream).await;
    }
}
