//! Phase 5 T4: `search_service` against a real database. Documents are ingested
//! through the Phase 4 pipeline; the query embedder is swapped per test.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use common_auth::{TenantContext, TenantRole};
use common_errors::AppError;
use sqlx::postgres::PgPoolOptions;
use tenant_saas::{
    ai::{EmbeddingError, EmbeddingProvider},
    auth::RequestContext,
    domain::search::SearchQuery,
    services::{
        ingestion_service::{self, ProcessOutcome},
        search_service::{self, SearchMode},
    },
};
use test_support::captured_logs;
use uuid::Uuid;

use crate::ingestion_support::{fixture, Fixture};

const TEXT: &[u8] = b"Quillmarsh retention policy. Employees keep records for seven years. \
Auditors review the archive every spring.";

async fn ingested() -> Option<Fixture> {
    let f = fixture("policy.txt", TEXT).await?;
    let out = ingestion_service::process(&f.deps, &f.event).await;
    assert!(matches!(out, ProcessOutcome::Succeeded { .. }), "{out:?}");
    Some(f)
}

fn ctx(tenant_id: Uuid, role: TenantRole) -> RequestContext {
    RequestContext::for_shared(TenantContext::from_verified_membership(
        tenant_id,
        Uuid::new_v4(),
        role,
    ))
}

fn query(text: &str) -> SearchQuery {
    SearchQuery::parse(text, None, None, &Default::default()).unwrap()
}

enum Behaviour {
    Sleep(Duration),
    Fail,
    WrongDim,
}

struct BadEmbedder(Behaviour);

#[async_trait]
impl EmbeddingProvider for BadEmbedder {
    fn model(&self) -> &str {
        "bad"
    }
    fn dim(&self) -> usize {
        1536
    }
    async fn embed(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        match &self.0 {
            Behaviour::Sleep(d) => {
                tokio::time::sleep(*d).await;
                Ok(inputs.iter().map(|_| vec![0.1; 1536]).collect())
            }
            Behaviour::Fail => Err(EmbeddingError::Transient {
                reason: "scripted outage".into(),
                retry_after: None,
            }),
            Behaviour::WrongDim => Ok(inputs.iter().map(|_| vec![0.1; 3]).collect()),
        }
    }
}

#[tokio::test]
async fn normal_provider_gives_hybrid_with_both_ranks() {
    let Some(f) = ingested().await else { return };
    let out = search_service::search(
        &f.app.state,
        &ctx(f.tenant_id, TenantRole::Member),
        query("quillmarsh"),
    )
    .await
    .unwrap();
    assert_eq!(out.mode, SearchMode::Hybrid);
    assert_eq!(out.hits.len(), 1);
    let hit = &out.hits[0];
    assert!(hit.vector_rank.is_some() && hit.lexical_rank.is_some());
    assert_eq!(hit.document_id, f.document_id);
    assert_eq!(hit.chunk_index, 0);
    assert!(hit.content.contains("Quillmarsh"));
}

#[tokio::test]
async fn slow_provider_degrades_to_lexical_only_within_the_timeout() {
    let Some(f) = ingested().await else { return };
    let mut state = f
        .app
        .state
        .clone()
        .with_embedder(Arc::new(BadEmbedder(Behaviour::Sleep(
            Duration::from_secs(30),
        ))));
    let mut cfg = (*state.config).clone();
    cfg.search.embed_timeout_ms = 200;
    state.config = Arc::new(cfg);

    let started = std::time::Instant::now();
    let out = search_service::search(
        &state,
        &ctx(f.tenant_id, TenantRole::Member),
        query("quillmarsh"),
    )
    .await
    .unwrap();
    assert!(started.elapsed() < Duration::from_millis(1200));
    assert_eq!(out.mode, SearchMode::LexicalOnly);
    assert_eq!(out.hits.len(), 1);
    assert!(out.hits[0].vector_rank.is_none());
}

#[tokio::test]
async fn failing_and_wrong_dimension_providers_degrade_to_lexical_only() {
    let Some(f) = ingested().await else { return };
    for behaviour in [Behaviour::Fail, Behaviour::WrongDim] {
        let state = f
            .app
            .state
            .clone()
            .with_embedder(Arc::new(BadEmbedder(behaviour)));
        let out = search_service::search(
            &state,
            &ctx(f.tenant_id, TenantRole::Member),
            query("quillmarsh"),
        )
        .await
        .unwrap();
        assert_eq!(out.mode, SearchMode::LexicalOnly);
        assert_eq!(out.hits.len(), 1);
    }
}

#[tokio::test]
async fn viewer_may_search() {
    let Some(f) = ingested().await else { return };
    let out = search_service::search(
        &f.app.state,
        &ctx(f.tenant_id, TenantRole::Viewer),
        query("quillmarsh"),
    )
    .await
    .unwrap();
    assert_eq!(out.hits.len(), 1);
}

#[tokio::test]
async fn query_text_is_never_logged() {
    let Some(f) = ingested().await else { return };
    let marker = format!("marker{}", Uuid::new_v4().simple());
    search_service::search(
        &f.app.state,
        &ctx(f.tenant_id, TenantRole::Member),
        query(&marker),
    )
    .await
    .unwrap();
    let logs = captured_logs();
    assert!(logs.contains("search completed"), "search log line missing");
    assert!(!logs.contains(&marker), "query text leaked into logs");
}

#[tokio::test]
async fn closed_database_pool_is_service_unavailable() {
    let Some(f) = ingested().await else { return };
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let pool = PgPoolOptions::new().connect(&url).await.unwrap();
    pool.close().await;
    let mut state = f.app.state.clone();
    state.db = pool;
    let err = search_service::search(
        &state,
        &ctx(f.tenant_id, TenantRole::Member),
        query("quillmarsh"),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, AppError::ExternalService(_) | AppError::Database(_)),
        "{err:?}"
    );
}
