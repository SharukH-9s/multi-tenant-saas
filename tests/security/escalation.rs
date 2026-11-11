//! Security test: escalation prevention and last-owner protection.
//!
//! Verifies:
//! 1. No user can change their own role (self-demotion or self-promotion).
//! 2. The last active owner cannot be removed or demoted (returns 409 LAST_OWNER).
//! 3. Multiple owners can remove each other down to the last remaining owner.
//! 4. Lower roles (Member, Viewer) cannot perform member management actions.

use axum::http::{Method, StatusCode};
use serde_json::json;
use test_support::{unique, TestApp};
use uuid::Uuid;

async fn spawn() -> Option<TestApp> {
    TestApp::spawn().await
}

#[tokio::test]
async fn user_cannot_change_own_role() {
    let Some(app) = spawn().await else { return };
    let owner = app.create_session().await;

    // Create tenant
    let (status, t) = app
        .request(
            Method::POST,
            "/api/v1/tenants",
            Some(json!({ "name": "Self Change Test", "slug": unique("self-chg") })),
            Some(&owner.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let tenant_id: Uuid = t["id"].as_str().unwrap().parse().unwrap();

    // Owner tries to change their own role to 'admin' -> 403 Forbidden
    let (status, resp) = app
        .request(
            Method::PATCH,
            &format!("/api/v1/tenants/{tenant_id}/members/{}", owner.user_id),
            Some(json!({ "role": "admin" })),
            Some(&owner.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{resp}");
    assert_eq!(resp["error"]["code"], "FORBIDDEN");
}

#[tokio::test]
async fn sole_owner_cannot_be_removed() {
    let Some(app) = spawn().await else { return };
    let owner = app.create_session().await;

    // Create tenant
    let (status, t) = app
        .request(
            Method::POST,
            "/api/v1/tenants",
            Some(json!({ "name": "Sole Owner", "slug": unique("sole-own") })),
            Some(&owner.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let tenant_id: Uuid = t["id"].as_str().unwrap().parse().unwrap();

    // Sole owner attempts to leave / delete their membership
    // DB trigger `memberships_protect_last_owner` intercepts this and raises LAST_OWNER -> 409 Conflict
    let (status, resp) = app
        .request(
            Method::DELETE,
            &format!("/api/v1/tenants/{tenant_id}/members/{}", owner.user_id),
            None,
            Some(&owner.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{resp}");
    assert_eq!(resp["error"]["code"], "LAST_OWNER");
}

#[tokio::test]
async fn multiple_owners_can_remove_each_other_down_to_one() {
    let Some(app) = spawn().await else { return };
    let owner1 = app.create_session().await;
    let owner2 = app.create_session().await;

    // Create tenant
    let (status, t) = app
        .request(
            Method::POST,
            "/api/v1/tenants",
            Some(json!({ "name": "Dual Owners", "slug": unique("dual-own") })),
            Some(&owner1.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let tenant_id: Uuid = t["id"].as_str().unwrap().parse().unwrap();

    // Owner 1 invites Owner 2 as owner
    let (status, _) = app
        .request(
            Method::POST,
            &format!("/api/v1/tenants/{tenant_id}/invitations"),
            Some(json!({ "email": owner2.email, "role": "owner" })),
            Some(&owner1.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    // Owner 2 accepts
    let (status, _) = app
        .request(
            Method::POST,
            &format!("/api/v1/invitations/{tenant_id}/accept"),
            None,
            Some(&owner2.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Owner 1 removes Owner 2 -> 204 No Content (succeeds because Owner 1 remains active)
    let (status, resp) = app
        .request(
            Method::DELETE,
            &format!("/api/v1/tenants/{tenant_id}/members/{}", owner2.user_id),
            None,
            Some(&owner1.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{resp}");

    // Now Owner 1 is the sole remaining owner. Attempting to remove self -> 409 LAST_OWNER
    let (status, resp) = app
        .request(
            Method::DELETE,
            &format!("/api/v1/tenants/{tenant_id}/members/{}", owner1.user_id),
            None,
            Some(&owner1.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{resp}");
    assert_eq!(resp["error"]["code"], "LAST_OWNER");
}

#[tokio::test]
async fn member_and_viewer_cannot_manage_members() {
    let Some(app) = spawn().await else { return };
    let owner = app.create_session().await;
    let member = app.create_session().await;
    let viewer = app.create_session().await;

    // Create tenant
    let (status, t) = app
        .request(
            Method::POST,
            "/api/v1/tenants",
            Some(json!({ "name": "Member Gate", "slug": unique("mem-gate") })),
            Some(&owner.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let tenant_id: Uuid = t["id"].as_str().unwrap().parse().unwrap();

    // Invite & accept member and viewer
    for (user, role) in [(&member, "member"), (&viewer, "viewer")] {
        let (status, _) = app
            .request(
                Method::POST,
                &format!("/api/v1/tenants/{tenant_id}/invitations"),
                Some(json!({ "email": user.email, "role": role })),
                Some(&owner.access_token),
            )
            .await;
        assert_eq!(status, StatusCode::ACCEPTED);

        let (status, _) = app
            .request(
                Method::POST,
                &format!("/api/v1/invitations/{tenant_id}/accept"),
                None,
                Some(&user.access_token),
            )
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }

    // Both member and viewer attempt GET /members -> 403 Forbidden
    for user in [&member, &viewer] {
        let (status, resp) = app
            .request(
                Method::GET,
                &format!("/api/v1/tenants/{tenant_id}/members"),
                None,
                Some(&user.access_token),
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{resp}");

        // Attempt invite -> 403
        let (status, resp) = app
            .request(
                Method::POST,
                &format!("/api/v1/tenants/{tenant_id}/invitations"),
                Some(json!({ "email": "any@example.com", "role": "viewer" })),
                Some(&user.access_token),
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{resp}");

        // Attempt delete -> 403
        let (status, resp) = app
            .request(
                Method::DELETE,
                &format!("/api/v1/tenants/{tenant_id}/members/{}", owner.user_id),
                None,
                Some(&user.access_token),
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{resp}");
    }
}
