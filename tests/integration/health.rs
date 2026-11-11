use axum::http::{Method, StatusCode};
use test_support::TestApp;

#[tokio::test]
async fn health_endpoints_behave_as_in_phase_1() {
    let Some(app) = TestApp::spawn().await else {
        return;
    };

    let (status, body) = app.request(Method::GET, "/health/live", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");

    let (status, body) = app.request(Method::GET, "/health/ready", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["db"], "ok");
}

#[tokio::test]
async fn error_bodies_include_request_id() {
    let Some(app) = TestApp::spawn().await else {
        return;
    };

    let (status, body) = app
        .request(Method::GET, "/api/v1/does-not-exist", None, None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "NOT_FOUND");
    assert!(body["error"]["request_id"]
        .as_str()
        .is_some_and(|id| !id.is_empty()));
}
