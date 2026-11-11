//! Integration test: invitation flows and email enumeration protection.
//!
//! Verifies:
//! 1. Inviting unregistered email returns 202 Accepted (no enumeration).
//! 2. Inviting an already-active member returns 202 Accepted (idempotent, unchanged).
//! 3. Registered user receives invitation, sees it in GET /invitations, accepts it.
//! 4. Accepting a nonexistent or already-accepted invitation returns 404 Not Found.

use axum::http::{Method, StatusCode};
use serde_json::json;
use test_support::{unique, TestApp};
use uuid::Uuid;

async fn spawn() -> Option<TestApp> {
    TestApp::spawn().await
}

#[tokio::test]
async fn invite_unregistered_email_returns_202_without_enumeration() {
    let Some(app) = spawn().await else { return };

    let owner = app.create_session().await;

    // Create tenant
    let (status, t) = app
        .request(
            Method::POST,
            "/api/v1/tenants",
            Some(json!({ "name": "No Enum Tenant", "slug": unique("no-enum") })),
            Some(&owner.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let tenant_id: Uuid = t["id"].as_str().unwrap().parse().unwrap();

    // Invite completely unregistered email
    let fake_email = format!("{}@example.com", unique("fake-user"));
    let (status, resp) = app
        .request(
            Method::POST,
            &format!("/api/v1/tenants/{tenant_id}/invitations"),
            Some(json!({ "email": fake_email, "role": "member" })),
            Some(&owner.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{resp}");
}

#[tokio::test]
async fn invite_already_active_member_returns_202_and_leaves_membership_intact() {
    let Some(app) = spawn().await else { return };

    let owner = app.create_session().await;
    let member = app.create_session().await;

    // Create tenant
    let (status, t) = app
        .request(
            Method::POST,
            "/api/v1/tenants",
            Some(json!({ "name": "Active Member Invite", "slug": unique("act-mem") })),
            Some(&owner.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let tenant_id: Uuid = t["id"].as_str().unwrap().parse().unwrap();

    // 1st invite as admin
    let (status, _) = app
        .request(
            Method::POST,
            &format!("/api/v1/tenants/{tenant_id}/invitations"),
            Some(json!({ "email": member.email, "role": "admin" })),
            Some(&owner.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    // Member accepts
    let (status, _) = app
        .request(
            Method::POST,
            &format!("/api/v1/invitations/{tenant_id}/accept"),
            None,
            Some(&member.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // 2nd invite with a different role 'viewer' -> still returns 202
    let (status, resp) = app
        .request(
            Method::POST,
            &format!("/api/v1/tenants/{tenant_id}/invitations"),
            Some(json!({ "email": member.email, "role": "viewer" })),
            Some(&owner.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{resp}");

    // Verify member role remains 'admin', not overwritten to 'viewer'
    let (status, detail) = app
        .request(
            Method::GET,
            &format!("/api/v1/tenants/{tenant_id}"),
            None,
            Some(&member.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["role"], "admin");
}

#[tokio::test]
async fn invitation_lifecycle_full_flow() {
    let Some(app) = spawn().await else { return };

    let owner = app.create_session().await;
    let invitee = app.create_session().await;

    // Create tenant
    let (status, t) = app
        .request(
            Method::POST,
            "/api/v1/tenants",
            Some(json!({ "name": "Invite Flow", "slug": unique("inv-flow") })),
            Some(&owner.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let tenant_id: Uuid = t["id"].as_str().unwrap().parse().unwrap();

    // Prior to invitation: invitee's GET /invitations is empty
    let (status, invs) = app
        .request(
            Method::GET,
            "/api/v1/invitations",
            None,
            Some(&invitee.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(invs.as_array().unwrap().len(), 0);

    // Owner sends invite
    let (status, _) = app
        .request(
            Method::POST,
            &format!("/api/v1/tenants/{tenant_id}/invitations"),
            Some(json!({ "email": invitee.email, "role": "member" })),
            Some(&owner.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    // Invitee lists pending invitations -> sees this tenant
    let (status, invs) = app
        .request(
            Method::GET,
            "/api/v1/invitations",
            None,
            Some(&invitee.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{invs}");
    let arr = invs.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["tenant_id"], tenant_id.to_string());
    assert_eq!(arr[0]["role"], "member");

    // Invitee accepts
    let (status, _) = app
        .request(
            Method::POST,
            &format!("/api/v1/invitations/{tenant_id}/accept"),
            None,
            Some(&invitee.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // After accept: GET /invitations is empty again
    let (status, invs) = app
        .request(
            Method::GET,
            "/api/v1/invitations",
            None,
            Some(&invitee.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(invs.as_array().unwrap().len(), 0);

    // Invitee can now access tenant
    let (status, detail) = app
        .request(
            Method::GET,
            &format!("/api/v1/tenants/{tenant_id}"),
            None,
            Some(&invitee.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["role"], "member");

    // Re-accepting already active invitation -> 404 Not Found
    let (status, resp) = app
        .request(
            Method::POST,
            &format!("/api/v1/invitations/{tenant_id}/accept"),
            None,
            Some(&invitee.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{resp}");
}

#[tokio::test]
async fn accept_nonexistent_invitation_returns_404() {
    let Some(app) = spawn().await else { return };
    let user = app.create_session().await;

    let (status, resp) = app
        .request(
            Method::POST,
            &format!("/api/v1/invitations/{}/accept", Uuid::new_v4()),
            None,
            Some(&user.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{resp}");
    assert_eq!(resp["error"]["code"], "NOT_FOUND");
}
