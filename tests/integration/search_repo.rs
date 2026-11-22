//! Phase 5 T3: the four search statements against a real database with RLS.
//! Each test ingests a real document through the Phase 4 pipeline (fake
//! embedder), then calls the repository under `begin_tenant_tx`.

use common_auth::{TenantContext, TenantRole};
use common_config::SearchConfig;
use common_database::{begin_tenant_tx, Tx};
use common_errors::AppError;
use tenant_saas::{
    domain::chunk::ChunkTarget,
    repositories::search_repo,
    services::ingestion_service::{self, ProcessOutcome},
};
use uuid::Uuid;

use crate::ingestion_support::{fixture, Fixture};

const TEXT: &[u8] =
    b"Zephyrquartz maintenance manual. The annual leave policy allows twenty days. \
Contractors submit invoices monthly. SKU-4471 is the replacement valve.";

/// The shared chunk target used by all unpromoted test tenants.
const SHARED_TARGET: &ChunkTarget = &ChunkTarget::Shared;

async fn ingested() -> Option<Fixture> {
    let f = fixture("manual.txt", TEXT).await?;
    let out = ingestion_service::process(&f.deps, &f.event).await;
    assert!(matches!(out, ProcessOutcome::Succeeded { .. }), "{out:?}");
    Some(f)
}

fn ctx(tenant_id: Uuid) -> TenantContext {
    TenantContext::from_verified_membership(tenant_id, Uuid::new_v4(), TenantRole::Member)
}

async fn tx_for(f: &Fixture, tenant_id: Uuid) -> Tx {
    begin_tenant_tx(&f.app.state.db, &ctx(tenant_id))
        .await
        .unwrap()
}

async fn embed(f: &Fixture, text: &str) -> Vec<f32> {
    f.app
        .state
        .embedder
        .embed(&[text.to_string()])
        .await
        .unwrap()
        .remove(0)
}

async fn chunk_id(f: &Fixture) -> (Uuid, String) {
    sqlx::query_as("SELECT id, content FROM document_chunks WHERE document_id = $1 LIMIT 1")
        .bind(f.document_id)
        .fetch_one(f.app.owner())
        .await
        .unwrap()
}

#[tokio::test]
async fn apply_settings_enables_relaxed_order_iterative_scan() {
    let Some(f) = ingested().await else { return };
    let mut tx = tx_for(&f, f.tenant_id).await;
    let v = embed(&f, "anything").await;
    search_repo::apply_settings(&mut tx, &SearchConfig::default())
        .await
        .unwrap();
    search_repo::vector_candidates(&mut tx, SHARED_TARGET, f.tenant_id, &v, None, 5)
        .await
        .unwrap();
    let shown: String = sqlx::query_scalar("SHOW hnsw.iterative_scan")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(shown, "relaxed_order");
}

#[tokio::test]
async fn relaxed_is_never_a_working_iterative_scan_value() {
    let Some(f) = ingested().await else { return };
    let mut tx = tx_for(&f, f.tenant_id).await;
    let v = embed(&f, "anything").await;
    let set = sqlx::query("SELECT set_config('hnsw.iterative_scan', 'relaxed', true)")
        .execute(&mut *tx)
        .await;
    if set.is_err() {
        return; // library already loaded on this connection: value rejected
    }
    let _ = search_repo::vector_candidates(&mut tx, SHARED_TARGET, f.tenant_id, &v, None, 5).await;
    let shown: Result<String, _> = sqlx::query_scalar("SHOW hnsw.iterative_scan")
        .fetch_one(&mut *tx)
        .await;
    if let Ok(s) = shown {
        assert_ne!(s, "relaxed");
    }
}

#[tokio::test]
async fn vector_search_with_own_vector_ranks_that_chunk_first() {
    let Some(f) = ingested().await else { return };
    let (id, content) = chunk_id(&f).await;
    let v = embed(&f, &content).await;
    let mut tx = tx_for(&f, f.tenant_id).await;
    let out = search_repo::vector_candidates(&mut tx, SHARED_TARGET, f.tenant_id, &v, None, 5)
        .await
        .unwrap();
    assert_eq!(out[0].chunk_id, id);
    assert_eq!(out[0].rank, 1);
}

#[tokio::test]
async fn lexical_search_finds_word_and_returns_nothing_for_missing_word() {
    let Some(f) = ingested().await else { return };
    let (id, _) = chunk_id(&f).await;
    let mut tx = tx_for(&f, f.tenant_id).await;
    let hit = search_repo::lexical_candidates(
        &mut tx,
        SHARED_TARGET,
        f.tenant_id,
        "zephyrquartz",
        None,
        5,
    )
    .await
    .unwrap();
    assert_eq!(hit[0].chunk_id, id);
    let ident =
        search_repo::lexical_candidates(&mut tx, SHARED_TARGET, f.tenant_id, "SKU-4471", None, 5)
            .await
            .unwrap();
    assert_eq!(ident.len(), 1);
    let none = search_repo::lexical_candidates(
        &mut tx,
        SHARED_TARGET,
        f.tenant_id,
        "nonexistentwordxyz",
        None,
        5,
    )
    .await
    .unwrap();
    assert!(none.is_empty());
}

#[tokio::test]
async fn stop_words_and_odd_syntax_never_error() {
    let Some(f) = ingested().await else { return };
    let mut tx = tx_for(&f, f.tenant_id).await;
    let stop =
        search_repo::lexical_candidates(&mut tx, SHARED_TARGET, f.tenant_id, "the and of", None, 5)
            .await
            .unwrap();
    assert!(stop.is_empty());
    for q in [
        "\"annual leave\" -sick",
        "\"unbalanced quote",
        "OR",
        "a & | ! (",
        "\u{27}",
    ] {
        search_repo::lexical_candidates(&mut tx, SHARED_TARGET, f.tenant_id, q, None, 5)
            .await
            .unwrap_or_else(|e| panic!("query {q:?} failed: {e:?}"));
    }
}

#[tokio::test]
async fn document_filter_limits_both_branches_and_does_not_leak() {
    let Some(a) = ingested().await else { return };
    let Some(b) = ingested().await else { return };
    let v = embed(&a, "anything").await;
    let mut tx = tx_for(&a, a.tenant_id).await;

    let own = [a.document_id];
    let lex = search_repo::lexical_candidates(
        &mut tx,
        SHARED_TARGET,
        a.tenant_id,
        "zephyrquartz",
        Some(&own),
        5,
    )
    .await
    .unwrap();
    let vec =
        search_repo::vector_candidates(&mut tx, SHARED_TARGET, a.tenant_id, &v, Some(&own), 5)
            .await
            .unwrap();
    assert_eq!(lex.len(), 1);
    assert_eq!(vec.len(), 1);

    // Another tenant's document id: nothing, and no error.
    let foreign = [b.document_id];
    let lex = search_repo::lexical_candidates(
        &mut tx,
        SHARED_TARGET,
        a.tenant_id,
        "zephyrquartz",
        Some(&foreign),
        5,
    )
    .await
    .unwrap();
    let vec =
        search_repo::vector_candidates(&mut tx, SHARED_TARGET, a.tenant_id, &v, Some(&foreign), 5)
            .await
            .unwrap();
    assert!(lex.is_empty() && vec.is_empty());
}

#[tokio::test]
async fn rls_blocks_other_tenant_even_with_explicit_tenant_id() {
    let Some(a) = ingested().await else { return };
    let Some(b) = ingested().await else { return };
    let mut tx = tx_for(&b, b.tenant_id).await;
    let out = search_repo::lexical_candidates(
        &mut tx,
        SHARED_TARGET,
        a.tenant_id,
        "zephyrquartz",
        None,
        5,
    )
    .await
    .unwrap();
    assert!(out.is_empty());
}

#[tokio::test]
async fn hydrate_returns_plain_snippet_and_drops_deleted_documents() {
    let Some(f) = ingested().await else { return };
    let (id, _) = chunk_id(&f).await;
    let mut tx = tx_for(&f, f.tenant_id).await;
    let rows = search_repo::hydrate(&mut tx, SHARED_TARGET, f.tenant_id, &[id], "annual leave")
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert!(!rows[0].snippet.contains('<'), "{}", rows[0].snippet);
    assert!(rows[0].snippet.to_lowercase().contains("annual leave"));
    drop(tx);

    sqlx::query("UPDATE documents SET status = 'deleting', deleted_at = now() WHERE id = $1")
        .bind(f.document_id)
        .execute(f.app.owner())
        .await
        .unwrap();
    let mut tx = tx_for(&f, f.tenant_id).await;
    let rows = search_repo::hydrate(&mut tx, SHARED_TARGET, f.tenant_id, &[id], "annual leave")
        .await
        .unwrap();
    assert!(rows.is_empty());
}

#[tokio::test]
async fn statement_timeout_maps_to_service_unavailable() {
    let Some(f) = ingested().await else { return };
    let mut tx = tx_for(&f, f.tenant_id).await;
    sqlx::query("SELECT set_config('statement_timeout', '1', true)")
        .execute(&mut *tx)
        .await
        .unwrap();
    let err = sqlx::query("SELECT pg_sleep(1)")
        .execute(&mut *tx)
        .await
        .unwrap_err();
    let mapped = tenant_saas::repositories::db_error::classify_sqlx_error(err);
    assert!(matches!(mapped, AppError::ExternalService(_)), "{mapped:?}");
}
