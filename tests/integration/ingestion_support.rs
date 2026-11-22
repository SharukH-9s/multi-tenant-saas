//! Shared fixtures for the T10 retry and recovery tests.
//!
//! The sweeper and the unpublished outbox rows are global, so every test that
//! sweeps must hold `INGEST_GLOBAL`. Tests only look at their own job, because
//! the shared database can hold rows left by other tests.

use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use async_trait::async_trait;
use axum::http::StatusCode;
use common_messaging::IngestionEvent;
use tenant_saas::{
    ai::{EmbeddingError, EmbeddingProvider},
    repositories::ingestion_repo,
    services::ingestion_service::IngestionDeps,
};
use test_support::TestApp;
use uuid::Uuid;

pub const SIMPLE_TXT: &[u8] =
    include_bytes!("../../apps/tenant-saas/tests/fixtures/ingestion/simple.txt");
pub const CORRUPT_PDF: &[u8] =
    include_bytes!("../../apps/tenant-saas/tests/fixtures/ingestion/corrupt.pdf");

/// A pending job for one freshly uploaded document.
pub struct Fixture {
    pub app: TestApp,
    pub token: String,
    pub tenant_id: Uuid,
    pub document_id: Uuid,
    pub job_id: Uuid,
    pub event: IngestionEvent,
    pub deps: IngestionDeps,
}

pub async fn fixture(filename: &str, content: &[u8]) -> Option<Fixture> {
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
            Some("Ingestion Test Document"),
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
    let deps = IngestionDeps::new(app.state.clone(), unique_worker());
    Some(Fixture {
        app,
        token: session.access_token.clone(),
        tenant_id,
        document_id,
        job_id,
        event,
        deps,
    })
}

pub fn unique_worker() -> String {
    format!("worker-test-{}", Uuid::new_v4().simple())
}

impl Fixture {
    /// Same app and database, different embedding provider.
    pub fn deps_with(&self, embedder: Arc<dyn EmbeddingProvider>) -> IngestionDeps {
        let state = self.app.state.clone().with_embedder(embedder);
        IngestionDeps::new(state, unique_worker())
    }

    /// No publisher runs in these tests, so the event written at upload time
    /// stays unpublished. The sweeper's "already announced?" guard would then
    /// block every requeue. Call this to play the part of the publisher.
    pub async fn publish_pending_events(&self) {
        self.sql(
            "UPDATE outbox_events SET published_at = now() \
                   WHERE aggregate_id = $1 AND published_at IS NULL",
        )
        .await;
    }

    pub async fn unpublished_events(&self) -> i64 {
        sqlx::query_scalar(
            "SELECT count(*) FROM outbox_events \
              WHERE aggregate_id = $1 AND published_at IS NULL AND dead_lettered_at IS NULL",
        )
        .bind(self.job_id)
        .fetch_one(self.app.owner())
        .await
        .unwrap()
    }

    /// Makes a `retry_scheduled` job due.
    pub async fn make_retry_due(&self) {
        self.sql(
            "UPDATE ingestion_jobs SET available_at = now() - interval '1 second' \
                   WHERE id = $1",
        )
        .await;
    }

    /// Expires the lease of a `processing` job.
    pub async fn expire_lease(&self) {
        self.sql(
            "UPDATE ingestion_jobs SET locked_until = now() - interval '1 second' \
                   WHERE id = $1",
        )
        .await;
    }

    pub async fn set_max_attempts(&self, n: i32) {
        sqlx::query("UPDATE ingestion_jobs SET max_attempts = $2 WHERE id = $1")
            .bind(self.job_id)
            .bind(n)
            .execute(self.app.owner())
            .await
            .unwrap();
    }

    /// Runs the sweeper once and returns what it did to *this* job.
    pub async fn sweep(&self) -> Vec<String> {
        let swept = ingestion_repo::sweep(&self.app.state.db, 500, 300)
            .await
            .expect("sweep");
        swept
            .into_iter()
            .filter(|s| s.job_id == self.job_id)
            .map(|s| s.action)
            .collect()
    }

    pub async fn job_status(&self) -> String {
        sqlx::query_scalar("SELECT status FROM ingestion_jobs WHERE id = $1")
            .bind(self.job_id)
            .fetch_one(self.app.owner())
            .await
            .unwrap()
    }

    pub async fn attempts(&self) -> i32 {
        sqlx::query_scalar("SELECT attempts FROM ingestion_jobs WHERE id = $1")
            .bind(self.job_id)
            .fetch_one(self.app.owner())
            .await
            .unwrap()
    }

    /// `(status, failure_code)` of the document.
    pub async fn document_state(&self) -> (String, Option<String>) {
        sqlx::query_as("SELECT status, failure_code FROM documents WHERE id = $1")
            .bind(self.document_id)
            .fetch_one(self.app.owner())
            .await
            .unwrap()
    }

    pub async fn chunk_count(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM document_chunks WHERE document_id = $1")
            .bind(self.document_id)
            .fetch_one(self.app.owner())
            .await
            .unwrap()
    }

    /// One statement with the job id as `$1`.
    async fn sql(&self, statement: &'static str) {
        sqlx::query(statement)
            .bind(self.job_id)
            .execute(self.app.owner())
            .await
            .unwrap();
    }
}

/// What the scripted embedder does on one `embed` call.
pub enum Step {
    Ok,
    Transient(Option<Duration>),
}

/// Plays a fixed script of outcomes, one per `embed` call, then succeeds.
pub struct ScriptedEmbedder {
    script: Mutex<VecDeque<Step>>,
    pub calls: AtomicUsize,
}

impl ScriptedEmbedder {
    pub fn new(script: impl IntoIterator<Item = Step>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script.into_iter().collect()),
            calls: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl EmbeddingProvider for ScriptedEmbedder {
    fn model(&self) -> &str {
        "scripted"
    }
    fn dim(&self) -> usize {
        1536
    }
    async fn embed(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let step = self.script.lock().unwrap().pop_front().unwrap_or(Step::Ok);
        match step {
            Step::Ok => Ok(inputs.iter().map(|_| vec![0.1f32; 1536]).collect()),
            Step::Transient(retry_after) => Err(EmbeddingError::Transient {
                reason: "scripted outage".into(),
                retry_after,
            }),
        }
    }
}
