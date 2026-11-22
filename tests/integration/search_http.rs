//! Phase 5 T5: `POST /api/v1/tenants/{id}/search` end to end, one test per
//! row of the status table, plus an exact check of the response field set.

use std::{collections::BTreeSet, time::Duration};

use axum::{
    body::Body,
    http::{Method, Request, StatusCode},
};
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use tenant_saas::{
    routes::build_router,
    services::ingestion_service::{self, ProcessOutcome},
};
use tower::ServiceExt;
use uuid::Uuid;

use crate::ingestion_support::{fixture, Fixture};

const TEXT: &[u8] = b"Brightfen onboarding guide. New hires receive a laptop on day one. \
Payroll runs on the last Friday of each month.";

async fn ingested() -> Option<Fixture> {
    let f = fixture("guide.txt", TEXT).await?;
    let out = ingestion_service::process(&f.deps, &f.event).await;
    assert!(matches!(out, ProcessOutcome::Succeeded { .. }), "{out:?}");
    Some(f)
}

fn path(tenant_id: Uuid) -> String {
    format!("/api/v1/tenants/{tenant_id}/search")
}

async fn post(
    f: &Fixture,
    tenant_id: Uuid,
    token: Option<&str>,
    body: Value,
) -> (StatusCode, Value) {
    f.app
        .request(Method::POST, &path(tenant_id), Some(body), token)
        .await
}

#[tokio::test]
async fn valid_request_returns_200_with_exact_field_set() {
    let Some(f) = ingested().await else { return };
    let (status, body) = post(
        &f,
        f.tenant_id,
        Some(&f.token),
        json!({"query": "brightfen"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["mode"], "hybrid");
    let top: BTreeSet<_> = body.as_object().unwrap().keys().cloned().collect();
    assert_eq!(
        top,
        BTreeSet::from(["mode".to_string(), "results".to_string()])
    );

    let item = &body["results"][0];
    let keys: BTreeSet<_> = item
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    let expected: BTreeSet<_> = [
        "document_id",
        "chunk_index",
        "chunk_id",
        "document_title",
        "snippet",
        "page_start",
        "page_end",
        "heading",
        "score",
        "vector_rank",
        "lexical_rank",
    ]
    .into_iter()
    .collect();
    assert_eq!(keys, expected);
    assert_eq!(item["document_id"], f.document_id.to_string());
    assert_eq!(item["document_title"], "Ingestion Test Document");
}

#[tokio::test]
async fn no_match_is_an_empty_200() {
    let Some(f) = ingested().await else { return };
    let (status, body) = post(
        &f,
        f.tenant_id,
        Some(&f.token),
        json!({"query": "nonexistentwordxyz", "document_ids": [Uuid::new_v4()]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["results"], json!([]));
}

#[tokio::test]
async fn missing_token_is_401() {
    let Some(f) = ingested().await else { return };
    let (status, body) = post(&f, f.tenant_id, None, json!({"query": "x"})).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "UNAUTHORIZED");
}

#[tokio::test]
async fn non_member_and_unknown_tenant_are_404() {
    let Some(f) = ingested().await else { return };
    let other = f.app.create_session().await;
    let (status, body) = post(
        &f,
        f.tenant_id,
        Some(&other.access_token),
        json!({"query": "x"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "NOT_FOUND");

    let (status, _) = post(&f, Uuid::new_v4(), Some(&f.token), json!({"query": "x"})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = f
        .app
        .request(
            Method::POST,
            "/api/v1/tenants/not-a-uuid/search",
            Some(json!({"query": "x"})),
            Some(&f.token),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn suspended_tenant_is_403() {
    let Some(f) = ingested().await else { return };
    f.app.set_tenant_status(f.tenant_id, "suspended").await;
    let (status, body) = post(&f, f.tenant_id, Some(&f.token), json!({"query": "x"})).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["code"], "FORBIDDEN");
}

#[tokio::test]
async fn invalid_requests_are_422() {
    let Some(f) = ingested().await else { return };
    let too_many: Vec<String> = (0..101).map(|_| Uuid::new_v4().to_string()).collect();
    let bodies = [
        json!({"query": ""}),
        json!({"query": "   "}),
        json!({"query": "a".repeat(1001)}),
        json!({"query": "x", "limit": 0}),
        json!({"query": "x", "limit": 51}),
        json!({"query": "x", "document_ids": too_many}),
        json!({"query": "x", "unexpected": true}),
        json!({"limit": 5}),
    ];
    for b in bodies {
        let (status, body) = post(&f, f.tenant_id, Some(&f.token), b.clone()).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{b} -> {body}");
        assert_eq!(body["error"]["code"], "VALIDATION_ERROR", "{b}");
    }
}

#[tokio::test]
async fn unreachable_database_is_503() {
    let Some(f) = ingested().await else { return };
    // Same app and JWT service, but the pool points at a dead port.
    let dead = PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(300))
        .connect_lazy("postgres://app_runtime:x@127.0.0.1:1/tenant_saas")
        .unwrap();
    let mut state = f.app.state.clone();
    state.db = dead;
    let req = Request::builder()
        .method("POST")
        .uri(path(f.tenant_id))
        .header("authorization", format!("Bearer {}", f.token))
        .header("content-type", "application/json")
        .body(Body::from(r#"{"query":"brightfen"}"#))
        .unwrap();
    let res = build_router(state).oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn malformed_json_is_400() {
    let Some(f) = ingested().await else { return };
    let req = Request::builder()
        .method("POST")
        .uri(path(f.tenant_id))
        .header("authorization", format!("Bearer {}", f.token))
        .header("content-type", "application/json")
        .body(Body::from("{not json"))
        .unwrap();
    let res = build_router(f.app.state.clone())
        .oneshot(req)
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}
