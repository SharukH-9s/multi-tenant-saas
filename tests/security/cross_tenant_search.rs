//! Security test: search never crosses tenant boundaries (Phase 5 T6).
//!
//! Documents are ingested through the real pipeline, then searched over HTTP
//! and directly through the repository under `app_runtime` with RLS enforced.

use axum::http::{Method, StatusCode};
use common_auth::{TenantContext, TenantRole};
use common_config::SearchConfig;
use common_database::begin_tenant_tx;
use common_messaging::IngestionEvent;
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use tenant_saas::{
    auth::request_context::RequestContext,
    repositories::search_repo,
    services::{
        ingestion_service::{self, IngestionDeps, ProcessOutcome},
        search_service,
    },
};
use test_support::TestApp;
use uuid::Uuid;

const SHARED_TEXT: &[u8] = b"Harborlight quarterly review. The vendor contract renews in March.";

struct Tenant {
    id: Uuid,
    user_id: Uuid,
    token: String,
    document_id: Uuid,
}

/// Creates a user and tenant, uploads `content` and runs the ingestion pipeline.
async fn tenant_with_document(app: &TestApp, content: &[u8]) -> Tenant {
    let session = app.create_session().await;
    let id = app
        .seed_membership(session.user_id, "owner", "active")
        .await;
    let (status, body) = app
        .upload_file(
            id,
            &session.access_token,
            "review.txt",
            content,
            Some("Review"),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let document_id: Uuid = body["id"].as_str().unwrap().parse().unwrap();

    let (job_id,): (Uuid,) =
        sqlx::query_as("SELECT id FROM ingestion_jobs WHERE tenant_id = $1 AND document_id = $2")
            .bind(id)
            .bind(document_id)
            .fetch_one(app.owner())
            .await
            .unwrap();
    let deps = IngestionDeps::new(app.state.clone(), format!("sec-{}", Uuid::new_v4()));
    let out =
        ingestion_service::process(&deps, &IngestionEvent::new(job_id, id, document_id)).await;
    assert!(matches!(out, ProcessOutcome::Succeeded { .. }), "{out:?}");

    Tenant {
        id,
        user_id: session.user_id,
        token: session.access_token,
        document_id,
    }
}

async fn search(app: &TestApp, t: &Tenant, tenant_path: Uuid, body: Value) -> (StatusCode, Value) {
    app.request(
        Method::POST,
        &format!("/api/v1/tenants/{tenant_path}/search"),
        Some(body),
        Some(&t.token),
    )
    .await
}

fn document_ids(body: &Value) -> Vec<String> {
    body["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["document_id"].as_str().unwrap().to_string())
        .collect()
}

/// Builds a `RequestContext` for the given tenant, using the shared chunk table.
/// All test tenants here are unpromoted — they use `public.document_chunks`.
fn shared_rc(tenant_id: Uuid, user_id: Uuid) -> RequestContext {
    RequestContext::for_shared(TenantContext::from_verified_membership(
        tenant_id,
        user_id,
        TenantRole::Owner,
    ))
}

#[tokio::test]
async fn same_file_in_two_tenants_returns_only_the_callers_document() {
    let Some(app) = TestApp::spawn().await else {
        return;
    };
    let a = tenant_with_document(&app, SHARED_TEXT).await;
    let b = tenant_with_document(&app, SHARED_TEXT).await;

    let (status, body) = search(
        &app,
        &b,
        b.id,
        json!({"query": "harborlight vendor contract"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(document_ids(&body), vec![b.document_id.to_string()]);
    assert!(!body.to_string().contains(&a.document_id.to_string()));
}

#[tokio::test]
async fn word_only_in_another_tenant_finds_nothing() {
    let Some(app) = TestApp::spawn().await else {
        return;
    };
    let marker = format!("zq{}", Uuid::new_v4().simple());
    let a = tenant_with_document(&app, format!("Secret {marker} roadmap.").as_bytes()).await;
    let b = tenant_with_document(&app, SHARED_TEXT).await;

    // The vector branch always returns the caller's nearest chunks, so B sees
    // its own document but never A's.
    let (status, body) = search(&app, &b, b.id, json!({"query": marker})).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!document_ids(&body).contains(&a.document_id.to_string()));
    assert!(!body.to_string().contains(&marker));

    // A tenant with no documents gets an empty result.
    let session = app.create_session().await;
    let empty_id = app
        .seed_membership(session.user_id, "owner", "active")
        .await;
    let empty = Tenant {
        id: empty_id,
        user_id: session.user_id,
        token: session.access_token,
        document_id: Uuid::nil(),
    };
    let (status, body) = search(&app, &empty, empty.id, json!({"query": marker})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["results"], json!([]));

    // Sanity: the owner of the document does find it.
    let (_, own) = search(&app, &a, a.id, json!({"query": marker})).await;
    assert_eq!(document_ids(&own), vec![a.document_id.to_string()]);
}

#[tokio::test]
async fn foreign_document_id_filter_returns_empty_200() {
    let Some(app) = TestApp::spawn().await else {
        return;
    };
    let a = tenant_with_document(&app, SHARED_TEXT).await;
    let b = tenant_with_document(&app, SHARED_TEXT).await;

    let (status, body) = search(
        &app,
        &b,
        b.id,
        json!({"query": "harborlight", "document_ids": [a.document_id]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["results"], json!([]));
}

#[tokio::test]
async fn searching_another_tenants_path_is_an_identical_404() {
    let Some(app) = TestApp::spawn().await else {
        return;
    };
    let a = tenant_with_document(&app, SHARED_TEXT).await;
    let b = tenant_with_document(&app, SHARED_TEXT).await;

    let (s1, body_a) = search(&app, &b, a.id, json!({"query": "harborlight"})).await;
    let (s2, body_random) = search(&app, &b, Uuid::new_v4(), json!({"query": "harborlight"})).await;
    assert_eq!(s1, StatusCode::NOT_FOUND);
    assert_eq!(s2, StatusCode::NOT_FOUND);
    // request_id is unique per request; everything else must be identical.
    assert_eq!(body_a["error"]["code"], body_random["error"]["code"]);
    assert_eq!(body_a["error"]["message"], body_random["error"]["message"]);
}

#[tokio::test]
async fn rls_stops_repository_queries_even_with_the_wrong_tenant_bound() {
    let Some(app) = TestApp::spawn().await else {
        return;
    };
    let a = tenant_with_document(&app, SHARED_TEXT).await;
    let b = tenant_with_document(&app, SHARED_TEXT).await;

    // Open a transaction in B's context — RLS will block reads for A's tenant_id.
    let ctx = TenantContext::from_verified_membership(b.id, b.user_id, TenantRole::Owner);
    let mut tx = begin_tenant_tx(&app.state.db, &ctx).await.unwrap();
    let vec = vec![0.1_f32; 1536];
    // All test tenants are unpromoted — shared table.
    let chunk_table = "public.document_chunks";

    let lex = search_repo::lexical_candidates(&mut tx, chunk_table, a.id, "harborlight", None, 10)
        .await
        .unwrap();
    let nearest =
        search_repo::vector_candidates(&mut tx, chunk_table, a.id, &vec, None, 10)
            .await
            .unwrap();
    assert!(lex.is_empty() && nearest.is_empty());

    // Even hydrating A's real chunk ids returns nothing under B's context.
    let a_chunks: Vec<(Uuid,)> =
        sqlx::query_as("SELECT id FROM document_chunks WHERE tenant_id = $1")
            .bind(a.id)
            .fetch_all(app.owner())
            .await
            .unwrap();
    let ids: Vec<Uuid> = a_chunks.into_iter().map(|r| r.0).collect();
    assert!(!ids.is_empty());
    let rows = search_repo::hydrate(&mut tx, chunk_table, a.id, &ids, "harborlight")
        .await
        .unwrap();
    assert!(rows.is_empty());
}

#[tokio::test]
async fn search_on_a_single_connection_pool_does_not_leak_between_tenants() {
    let Some(app) = TestApp::spawn().await else {
        return;
    };
    let a = tenant_with_document(&app, SHARED_TEXT).await;
    let b = tenant_with_document(&app, SHARED_TEXT).await;

    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();
    let mut state = app.state.clone();
    state.db = pool.clone();

    for _ in 0..3 {
        for t in [&a, &b] {
            // Wrap in RequestContext so search_service::search gets chunk routing.
            let rc = shared_rc(t.id, t.user_id);
            let q = tenant_saas::domain::search::SearchQuery::parse(
                "harborlight",
                None,
                None,
                &SearchConfig::default(),
            )
            .unwrap();
            let out = search_service::search(&state, &rc, q).await.unwrap();
            let docs: Vec<Uuid> = out.hits.iter().map(|h| h.document_id).collect();
            assert_eq!(docs, vec![t.document_id]);
        }
    }

    // The search settings were transaction-local: nothing persists on the connection.
    let cfg = SearchConfig::default();
    let ef: Option<String> = sqlx::query_scalar("SELECT current_setting('hnsw.ef_search', true)")
        .fetch_one(&pool)
        .await
        .unwrap();
    let scan: Option<String> =
        sqlx::query_scalar("SELECT current_setting('hnsw.iterative_scan', true)")
            .fetch_one(&pool)
            .await
            .unwrap();
    let timeout: String = sqlx::query_scalar("SHOW statement_timeout")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_ne!(ef.as_deref(), Some(cfg.ef_search.to_string().as_str()));
    assert_ne!(scan.as_deref(), Some("relaxed_order"));
    assert_ne!(timeout, format!("{}ms", cfg.statement_timeout_ms));
    let tenant_guc: Option<String> =
        sqlx::query_scalar("SELECT current_setting('app.current_tenant_id', true)")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(tenant_guc.unwrap_or_default().is_empty());
}

#[tokio::test]
async fn every_role_can_search() {
    let Some(app) = TestApp::spawn().await else {
        return;
    };
    let owner = tenant_with_document(&app, SHARED_TEXT).await;

    for role in ["owner", "admin", "member", "viewer"] {
        let session = app.create_session().await;
        app.add_member(owner.id, session.user_id, role, "active")
            .await;
        let (status, body) = app
            .request(
                Method::POST,
                &format!("/api/v1/tenants/{}/search", owner.id),
                Some(json!({"query": "harborlight"})),
                Some(&session.access_token),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{role}: {body}");
        assert_eq!(
            document_ids(&body),
            vec![owner.document_id.to_string()],
            "{role}"
        );
    }
}

#[tokio::test]
async fn deleted_document_is_never_returned() {
    let Some(app) = TestApp::spawn().await else {
        return;
    };
    let t = tenant_with_document(&app, SHARED_TEXT).await;

    let (_, before) = search(&app, &t, t.id, json!({"query": "harborlight"})).await;
    assert_eq!(document_ids(&before).len(), 1);

    let (status, _) = app
        .request(
            Method::DELETE,
            &format!("/api/v1/tenants/{}/documents/{}", t.id, t.document_id),
            None,
            Some(&t.token),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, after) = search(&app, &t, t.id, json!({"query": "harborlight"})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(after["results"], json!([]));
}
