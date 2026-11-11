//! Security test: cross-tenant isolation and immediate effect of membership changes.
//!
//! Verifies:
//! 1. User B calling Tenant A endpoints gets uniform 404s identical to querying
//!    non-existent tenants (no existence oracle).
//! 2. Revoking or suspending a membership terminates access immediately on the
//!    next request, even with a still-valid JWT access token.
//! 3. Disabling a user terminates access immediately across all tenants.

use axum::http::{Method, StatusCode};
use serde_json::{json, Value};
use test_support::{unique, TestApp};
use uuid::Uuid;

async fn spawn() -> Option<TestApp> {
    TestApp::spawn().await
}

#[tokio::test]
async fn cross_tenant_access_returns_identical_404_no_existence_oracle() {
    let Some(app) = spawn().await else { return };

    let user_a = app.create_session().await;
    let user_b = app.create_session().await;

    // User A creates Tenant A
    let (status, t_a) = app
        .request(
            Method::POST,
            "/api/v1/tenants",
            Some(json!({ "name": "Tenant A", "slug": unique("iso-a") })),
            Some(&user_a.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let tenant_a_id: Uuid = t_a["id"].as_str().unwrap().parse().unwrap();

    let nonexistent_id = Uuid::new_v4();

    // Baseline 404 for a non-existent tenant
    let (base_status, base_body) = app
        .request(
            Method::GET,
            &format!("/api/v1/tenants/{nonexistent_id}"),
            None,
            Some(&user_b.access_token),
        )
        .await;
    assert_eq!(base_status, StatusCode::NOT_FOUND);
    assert_eq!(base_body["error"]["code"], "NOT_FOUND");
    let standard_message = base_body["error"]["message"].as_str().unwrap();

    // Helper to check 404 status and identical code + message
    let assert_concealed = |status: StatusCode, body: Value, desc: &str| {
        assert_eq!(status, StatusCode::NOT_FOUND, "{desc} status");
        assert_eq!(body["error"]["code"], "NOT_FOUND", "{desc} code");
        assert_eq!(body["error"]["message"], standard_message, "{desc} message");
    };

    // User B tries GET /tenants/{A}
    let (s, b) = app
        .request(
            Method::GET,
            &format!("/api/v1/tenants/{tenant_a_id}"),
            None,
            Some(&user_b.access_token),
        )
        .await;
    assert_concealed(s, b, "GET /tenants/{A}");

    // User B tries PATCH /tenants/{A}
    let (s, b) = app
        .request(
            Method::PATCH,
            &format!("/api/v1/tenants/{tenant_a_id}"),
            Some(json!({ "name": "Hacked" })),
            Some(&user_b.access_token),
        )
        .await;
    assert_concealed(s, b, "PATCH /tenants/{A}");

    // User B tries GET /tenants/{A}/members
    let (s, b) = app
        .request(
            Method::GET,
            &format!("/api/v1/tenants/{tenant_a_id}/members"),
            None,
            Some(&user_b.access_token),
        )
        .await;
    assert_concealed(s, b, "GET /tenants/{A}/members");

    // User B tries PATCH /tenants/{A}/members/{user_a}
    let (s, b) = app
        .request(
            Method::PATCH,
            &format!("/api/v1/tenants/{tenant_a_id}/members/{}", user_a.user_id),
            Some(json!({ "role": "viewer" })),
            Some(&user_b.access_token),
        )
        .await;
    assert_concealed(s, b, "PATCH /tenants/{A}/members/{user_a}");

    // User B tries DELETE /tenants/{A}/members/{user_a}
    let (s, b) = app
        .request(
            Method::DELETE,
            &format!("/api/v1/tenants/{tenant_a_id}/members/{}", user_a.user_id),
            None,
            Some(&user_b.access_token),
        )
        .await;
    assert_concealed(s, b, "DELETE /tenants/{A}/members/{user_a}");

    // User B tries POST /tenants/{A}/invitations
    let (s, b) = app
        .request(
            Method::POST,
            &format!("/api/v1/tenants/{tenant_a_id}/invitations"),
            Some(json!({ "email": "any@example.com", "role": "member" })),
            Some(&user_b.access_token),
        )
        .await;
    assert_concealed(s, b, "POST /tenants/{A}/invitations");
}

#[tokio::test]
async fn revoked_membership_terminates_access_immediately() {
    let Some(app) = spawn().await else { return };

    let owner = app.create_session().await;
    let member = app.create_session().await;

    // Create tenant
    let (status, t) = app
        .request(
            Method::POST,
            "/api/v1/tenants",
            Some(json!({ "name": "Revoke Test", "slug": unique("rev-test") })),
            Some(&owner.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let tenant_id: Uuid = t["id"].as_str().unwrap().parse().unwrap();

    // Invite and accept
    let (status, _) = app
        .request(
            Method::POST,
            &format!("/api/v1/tenants/{tenant_id}/invitations"),
            Some(json!({ "email": member.email, "role": "member" })),
            Some(&owner.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let (status, _) = app
        .request(
            Method::POST,
            &format!("/api/v1/invitations/{tenant_id}/accept"),
            None,
            Some(&member.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Member has access right now
    let (status, _) = app
        .request(
            Method::GET,
            &format!("/api/v1/tenants/{tenant_id}"),
            None,
            Some(&member.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    // Owner removes member
    let (status, _) = app
        .request(
            Method::DELETE,
            &format!("/api/v1/tenants/{tenant_id}/members/{}", member.user_id),
            None,
            Some(&owner.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // On the very next request, with the SAME unexpired token, member gets 404
    let (status, resp) = app
        .request(
            Method::GET,
            &format!("/api/v1/tenants/{tenant_id}"),
            None,
            Some(&member.access_token),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "revoked member must get 404 immediately"
    );
    assert_eq!(resp["error"]["code"], "NOT_FOUND");
}

#[tokio::test]
async fn suspended_membership_terminates_access_immediately() {
    let Some(app) = spawn().await else { return };

    let owner = app.create_session().await;
    let member = app.create_session().await;

    // Create tenant
    let (status, t) = app
        .request(
            Method::POST,
            "/api/v1/tenants",
            Some(json!({ "name": "Suspend Test", "slug": unique("sus-test") })),
            Some(&owner.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let tenant_id: Uuid = t["id"].as_str().unwrap().parse().unwrap();

    // Invite and accept
    let (status, _) = app
        .request(
            Method::POST,
            &format!("/api/v1/tenants/{tenant_id}/invitations"),
            Some(json!({ "email": member.email, "role": "member" })),
            Some(&owner.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let (status, _) = app
        .request(
            Method::POST,
            &format!("/api/v1/invitations/{tenant_id}/accept"),
            None,
            Some(&member.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Member has access
    let (status, _) = app
        .request(
            Method::GET,
            &format!("/api/v1/tenants/{tenant_id}"),
            None,
            Some(&member.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    // Owner suspends member
    let (status, _) = app
        .request(
            Method::PATCH,
            &format!("/api/v1/tenants/{tenant_id}/members/{}", member.user_id),
            Some(json!({ "status": "suspended" })),
            Some(&owner.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    // On next request with same token: 403 Forbidden
    let (status, resp) = app
        .request(
            Method::GET,
            &format!("/api/v1/tenants/{tenant_id}"),
            None,
            Some(&member.access_token),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "suspended member must get 403"
    );
    assert_eq!(resp["error"]["code"], "FORBIDDEN");
}

#[tokio::test]
async fn disabled_user_is_unauthorized_across_tenants_immediately() {
    let Some(app) = spawn().await else { return };

    let owner = app.create_session().await;
    let member = app.create_session().await;

    // Create tenant & add member
    let (status, t) = app
        .request(
            Method::POST,
            "/api/v1/tenants",
            Some(json!({ "name": "Disable Test", "slug": unique("dis-test") })),
            Some(&owner.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let tenant_id: Uuid = t["id"].as_str().unwrap().parse().unwrap();

    let (status, _) = app
        .request(
            Method::POST,
            &format!("/api/v1/tenants/{tenant_id}/invitations"),
            Some(json!({ "email": member.email, "role": "member" })),
            Some(&owner.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let (status, _) = app
        .request(
            Method::POST,
            &format!("/api/v1/invitations/{tenant_id}/accept"),
            None,
            Some(&member.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Member can access
    let (status, _) = app
        .request(
            Method::GET,
            &format!("/api/v1/tenants/{tenant_id}"),
            None,
            Some(&member.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    // Disable the user in DB (via owner pool helper)
    app.set_user_status(member.user_id, "disabled").await;

    // Immediately rejected with 401 UNAUTHORIZED
    let (status, resp) = app
        .request(
            Method::GET,
            &format!("/api/v1/tenants/{tenant_id}"),
            None,
            Some(&member.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(resp["error"]["code"], "UNAUTHORIZED");

    // /auth/me also rejects disabled user
    let (status, resp) = app
        .request(
            Method::GET,
            "/api/v1/auth/me",
            None,
            Some(&member.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(resp["error"]["code"], "UNAUTHORIZED");
}
