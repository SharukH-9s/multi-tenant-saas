//! Integration test: multi-tenant membership and role enforcement.
//!
//! Verifies:
//! 1. A user can join multiple tenants and hold different roles in each.
//! 2. GET /api/v1/tenants returns the caller's active tenants with accurate roles.
//! 3. Role-based permission checks work on tenant endpoints (e.g., Member cannot
//!    rename a tenant, Owner can).
//! 4. Admin cannot promote to Owner, demote Owner, or manage critical tenant settings.

use axum::http::{Method, StatusCode};
use serde_json::json;
use test_support::{unique, TestApp};
use uuid::Uuid;

async fn spawn() -> Option<TestApp> {
    TestApp::spawn().await
}

#[tokio::test]
async fn user_can_belong_to_multiple_tenants_with_different_roles() {
    let Some(app) = spawn().await else { return };

    let owner = app.create_session().await;
    let member = app.create_session().await;

    // Owner creates two separate tenants
    let t1_name = "Tenant Alpha";
    let t1_slug = unique("alpha");
    let (status, t1) = app
        .request(
            Method::POST,
            "/api/v1/tenants",
            Some(json!({ "name": t1_name, "slug": t1_slug })),
            Some(&owner.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{t1}");
    let t1_id: Uuid = t1["id"].as_str().unwrap().parse().unwrap();

    let t2_name = "Tenant Beta";
    let t2_slug = unique("beta");
    let (status, t2) = app
        .request(
            Method::POST,
            "/api/v1/tenants",
            Some(json!({ "name": t2_name, "slug": t2_slug })),
            Some(&owner.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{t2}");
    let t2_id: Uuid = t2["id"].as_str().unwrap().parse().unwrap();

    // Owner invites member into Tenant 1 as 'member' and Tenant 2 as 'viewer'
    let (status, _) = app
        .request(
            Method::POST,
            &format!("/api/v1/tenants/{t1_id}/invitations"),
            Some(json!({ "email": member.email, "role": "member" })),
            Some(&owner.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let (status, _) = app
        .request(
            Method::POST,
            &format!("/api/v1/tenants/{t2_id}/invitations"),
            Some(json!({ "email": member.email, "role": "viewer" })),
            Some(&owner.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    // Member accepts both invitations
    let (status, _) = app
        .request(
            Method::POST,
            &format!("/api/v1/invitations/{t1_id}/accept"),
            None,
            Some(&member.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _) = app
        .request(
            Method::POST,
            &format!("/api/v1/invitations/{t2_id}/accept"),
            None,
            Some(&member.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Member lists their tenants
    let (status, member_tenants) = app
        .request(
            Method::GET,
            "/api/v1/tenants",
            None,
            Some(&member.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{member_tenants}");
    let arr = member_tenants.as_array().expect("array of tenants");
    assert_eq!(arr.len(), 2);

    let t1_entry = arr
        .iter()
        .find(|t| t["tenant_id"].as_str() == Some(&t1_id.to_string()))
        .expect("tenant 1 in list");
    assert_eq!(t1_entry["role"], "member");
    assert_eq!(t1_entry["status"], "active");

    let t2_entry = arr
        .iter()
        .find(|t| t["tenant_id"].as_str() == Some(&t2_id.to_string()))
        .expect("tenant 2 in list");
    assert_eq!(t2_entry["role"], "viewer");
    assert_eq!(t2_entry["status"], "active");

    // Owner also sees both as owner
    let (status, owner_tenants) = app
        .request(
            Method::GET,
            "/api/v1/tenants",
            None,
            Some(&owner.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{owner_tenants}");
    let owner_arr = owner_tenants.as_array().expect("array of tenants");
    assert_eq!(owner_arr.len(), 2);
    for t in owner_arr {
        assert_eq!(t["role"], "owner");
    }
}

#[tokio::test]
async fn member_cannot_update_tenant_settings_but_owner_can() {
    let Some(app) = spawn().await else { return };

    let owner = app.create_session().await;
    let member = app.create_session().await;

    // Create tenant
    let (status, t) = app
        .request(
            Method::POST,
            "/api/v1/tenants",
            Some(json!({ "name": "Initial Name", "slug": unique("set-test") })),
            Some(&owner.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let tenant_id: Uuid = t["id"].as_str().unwrap().parse().unwrap();

    // Invite & accept as member
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

    // Member tries to update tenant name -> 403 Forbidden
    let (status, resp) = app
        .request(
            Method::PATCH,
            &format!("/api/v1/tenants/{tenant_id}"),
            Some(json!({ "name": "Member Renamed" })),
            Some(&member.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{resp}");
    assert_eq!(resp["error"]["code"], "FORBIDDEN");

    // Owner updates tenant name -> 200 OK
    let (status, resp) = app
        .request(
            Method::PATCH,
            &format!("/api/v1/tenants/{tenant_id}"),
            Some(json!({ "name": "Owner Renamed" })),
            Some(&owner.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{resp}");
    assert_eq!(resp["name"], "Owner Renamed");

    // Member gets tenant detail -> 200 OK, sees updated name and role "member"
    let (status, detail) = app
        .request(
            Method::GET,
            &format!("/api/v1/tenants/{tenant_id}"),
            None,
            Some(&member.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["name"], "Owner Renamed");
    assert_eq!(detail["role"], "member");
}

#[tokio::test]
async fn admin_permissions_and_escalation_rules() {
    let Some(app) = spawn().await else { return };

    let owner = app.create_session().await;
    let admin = app.create_session().await;
    let other = app.create_session().await;

    // Create tenant
    let (status, t) = app
        .request(
            Method::POST,
            "/api/v1/tenants",
            Some(json!({ "name": "Admin Test", "slug": unique("admin-test") })),
            Some(&owner.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let tenant_id: Uuid = t["id"].as_str().unwrap().parse().unwrap();

    // Invite admin as admin & accept
    let (status, _) = app
        .request(
            Method::POST,
            &format!("/api/v1/tenants/{tenant_id}/invitations"),
            Some(json!({ "email": admin.email, "role": "admin" })),
            Some(&owner.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let (status, _) = app
        .request(
            Method::POST,
            &format!("/api/v1/invitations/{tenant_id}/accept"),
            None,
            Some(&admin.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // 1. Admin CAN update tenant name (TenantUpdateSettings)
    let (status, resp) = app
        .request(
            Method::PATCH,
            &format!("/api/v1/tenants/{tenant_id}"),
            Some(json!({ "name": "Admin Renamed Tenant" })),
            Some(&admin.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{resp}");
    assert_eq!(resp["name"], "Admin Renamed Tenant");

    // 2. Admin CANNOT update slug/plan/status (TenantManageCritical -> 403)
    let (status, resp) = app
        .request(
            Method::PATCH,
            &format!("/api/v1/tenants/{tenant_id}"),
            Some(json!({ "slug": unique("new-slug") })),
            Some(&admin.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{resp}");
    assert_eq!(resp["error"]["code"], "FORBIDDEN");

    // 3. Admin CANNOT invite someone as 'owner' -> 403
    let (status, resp) = app
        .request(
            Method::POST,
            &format!("/api/v1/tenants/{tenant_id}/invitations"),
            Some(json!({ "email": other.email, "role": "owner" })),
            Some(&admin.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{resp}");
    assert_eq!(resp["error"]["code"], "FORBIDDEN");

    // 4. Admin CAN invite someone as 'member'
    let (status, _) = app
        .request(
            Method::POST,
            &format!("/api/v1/tenants/{tenant_id}/invitations"),
            Some(json!({ "email": other.email, "role": "member" })),
            Some(&admin.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    // Other accepts
    let (status, _) = app
        .request(
            Method::POST,
            &format!("/api/v1/invitations/{tenant_id}/accept"),
            None,
            Some(&other.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // 5. Admin CANNOT promote member to 'owner' -> 403
    let (status, resp) = app
        .request(
            Method::PATCH,
            &format!("/api/v1/tenants/{tenant_id}/members/{}", other.user_id),
            Some(json!({ "role": "owner" })),
            Some(&admin.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{resp}");

    // 6. Admin CANNOT modify or demote Owner -> 403
    let (status, resp) = app
        .request(
            Method::PATCH,
            &format!("/api/v1/tenants/{tenant_id}/members/{}", owner.user_id),
            Some(json!({ "role": "member" })),
            Some(&admin.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{resp}");

    // 7. Admin CANNOT remove Owner -> 403
    let (status, resp) = app
        .request(
            Method::DELETE,
            &format!("/api/v1/tenants/{tenant_id}/members/{}", owner.user_id),
            None,
            Some(&admin.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{resp}");
}
