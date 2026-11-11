//! Task 7: tenant resolution through the real `TenantCtx` extractor.
//!
//! The real tenant endpoints arrive in Task 8, so these tests mount two tiny
//! test-only routes that use the extractor exactly as a handler will.

use axum::{
    extract::State,
    http::{Method, StatusCode},
    routing::get,
    Json, Router,
};
use common_auth::{Permission, TenantRole};
use common_database::begin_tenant_tx;
use common_errors::AppError;
use serde_json::{json, Value};
use tenant_saas::{app_state::AppState, auth::extractors::TenantCtx};
use test_support::TestApp;
use uuid::Uuid;

/// Returns what the extractor resolved, plus the tenant setting the database sees
/// inside `begin_tenant_tx`.
async fn whoami(
    State(state): State<AppState>,
    TenantCtx(ctx): TenantCtx,
) -> Result<Json<Value>, AppError> {
    let mut tx = begin_tenant_tx(&state.db, &ctx).await?;
    let db_tenant: String =
        sqlx::query_scalar("SELECT current_setting('app.current_tenant_id', true)")
            .fetch_one(&mut *tx)
            .await?;

    Ok(Json(json!({
        "tenant_id": ctx.tenant_id(),
        "user_id": ctx.user_id(),
        "role": ctx.role().as_str(),
        "db_tenant": db_tenant,
    })))
}

/// Shows the handler pattern: resolve, then check a permission.
async fn manage_members(TenantCtx(ctx): TenantCtx) -> Result<Json<Value>, AppError> {
    ctx.require(Permission::MembersManage)?;
    Ok(Json(json!({ "allowed": true })))
}

async fn spawn() -> Option<TestApp> {
    let routes = Router::new()
        .route("/test/tenants/{tenant_id}/whoami", get(whoami))
        .route("/test/tenants/{tenant_id}/manage", get(manage_members));
    TestApp::spawn().await.map(|app| app.with_routes(routes))
}

async fn call(
    app: &TestApp,
    action: &str,
    tenant: &str,
    bearer: Option<&str>,
) -> (StatusCode, Value) {
    let path = format!("/test/tenants/{tenant}/{action}");
    app.request(Method::GET, &path, None, bearer).await
}

async fn whoami_as(app: &TestApp, tenant: impl ToString, bearer: &str) -> (StatusCode, Value) {
    call(app, "whoami", &tenant.to_string(), Some(bearer)).await
}

// ----------------------------------------------------------------- success

#[tokio::test]
async fn every_role_resolves_with_the_right_role_and_database_scope() {
    let Some(app) = spawn().await else { return };

    for role in TenantRole::ALL {
        let session = app.create_session().await;
        let tenant = app
            .seed_membership(session.user_id, role.as_str(), "active")
            .await;

        let (status, body) = whoami_as(&app, tenant, &session.access_token).await;

        assert_eq!(status, StatusCode::OK, "{role}: {body}");
        assert_eq!(body["role"], role.as_str());
        assert_eq!(body["tenant_id"], tenant.to_string());
        assert_eq!(body["user_id"], session.user_id.to_string());
        assert_eq!(
            body["db_tenant"],
            tenant.to_string(),
            "begin_tenant_tx must scope the database to the resolved tenant"
        );
    }
}

#[tokio::test]
async fn handlers_can_check_permissions_on_the_resolved_role() {
    let Some(app) = spawn().await else { return };

    for (role, expected) in [
        (TenantRole::Owner, StatusCode::OK),
        (TenantRole::Admin, StatusCode::OK),
        (TenantRole::Member, StatusCode::FORBIDDEN),
        (TenantRole::Viewer, StatusCode::FORBIDDEN),
    ] {
        let session = app.create_session().await;
        let tenant = app
            .seed_membership(session.user_id, role.as_str(), "active")
            .await;

        let (status, body) = call(
            &app,
            "manage",
            &tenant.to_string(),
            Some(&session.access_token),
        )
        .await;

        assert_eq!(status, expected, "{role}: {body}");
        if expected == StatusCode::FORBIDDEN {
            assert_eq!(body["error"]["code"], "FORBIDDEN");
        }
    }
}

#[tokio::test]
async fn a_user_in_two_tenants_gets_the_role_of_the_tenant_in_the_url() {
    let Some(app) = spawn().await else { return };
    let session = app.create_session().await;
    let owned = app
        .seed_membership(session.user_id, "owner", "active")
        .await;
    let viewed = app
        .seed_membership(session.user_id, "viewer", "active")
        .await;
    let strangers = app.create_tenant().await;

    let (_, body) = whoami_as(&app, owned, &session.access_token).await;
    assert_eq!(
        (body["role"].clone(), body["db_tenant"].clone()),
        ("owner".into(), owned.to_string().into())
    );

    let (_, body) = whoami_as(&app, viewed, &session.access_token).await;
    assert_eq!(
        (body["role"].clone(), body["db_tenant"].clone()),
        ("viewer".into(), viewed.to_string().into())
    );

    let (status, _) = whoami_as(&app, strangers, &session.access_token).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "membership elsewhere grants nothing here"
    );
}

// --------------------------------------------------------------- concealment

#[tokio::test]
async fn non_member_unknown_tenant_and_bad_id_are_indistinguishable() {
    let Some(app) = spawn().await else { return };
    let owner = app.create_session().await;
    let outsider = app.create_session().await;
    let real_tenant = app.seed_membership(owner.user_id, "owner", "active").await;

    let attempts = [
        real_tenant.to_string(),          // exists, but the caller is not a member
        Uuid::new_v4().to_string(),       // does not exist
        "not-a-uuid".to_string(),         // not a UUID
        "%27%20OR%201%3D1--".to_string(), // injection attempt, URL-encoded
    ];

    let mut bodies = Vec::new();
    for tenant in &attempts {
        let (status, body) = whoami_as(&app, tenant, &outsider.access_token).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{tenant}: {body}");
        assert_eq!(body["error"]["code"], "NOT_FOUND");
        bodies.push(body);
    }
    assert!(
        bodies.windows(2).all(|pair| pair[0] == pair[1]),
        "responses differ, which would reveal which tenants exist: {bodies:?}"
    );
}

#[tokio::test]
async fn only_the_url_decides_the_tenant() {
    let Some(app) = spawn().await else { return };
    let session = app.create_session().await;
    let mine = app
        .seed_membership(session.user_id, "owner", "active")
        .await;
    let theirs = app.create_tenant().await;

    // Pointing a query string at my tenant does not help.
    let path = format!("/test/tenants/{theirs}/whoami?tenant_id={mine}");
    let (status, _) = app
        .request(Method::GET, &path, None, Some(&session.access_token))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ----------------------------------------------------------- authentication

#[tokio::test]
async fn unauthenticated_requests_get_401_before_any_tenant_lookup() {
    let Some(app) = spawn().await else { return };
    let session = app.create_session().await;
    let tenant = app
        .seed_membership(session.user_id, "owner", "active")
        .await;

    let mut bodies = Vec::new();
    for target in [
        tenant.to_string(),
        Uuid::new_v4().to_string(),
        "not-a-uuid".to_string(),
    ] {
        for bearer in [None, Some("garbage"), Some(session.refresh_token.as_str())] {
            let (status, body) = call(&app, "whoami", &target, bearer).await;
            assert_eq!(
                status,
                StatusCode::UNAUTHORIZED,
                "{target} {bearer:?}: {body}"
            );
            bodies.push(body);
        }
    }
    assert!(bodies.windows(2).all(|pair| pair[0] == pair[1]));
}

#[tokio::test]
async fn a_disabled_user_is_unauthorized_even_as_a_member() {
    let Some(app) = spawn().await else { return };
    let session = app.create_session().await;
    let tenant = app
        .seed_membership(session.user_id, "owner", "active")
        .await;
    assert_eq!(
        whoami_as(&app, tenant, &session.access_token).await.0,
        StatusCode::OK
    );

    app.set_user_status(session.user_id, "disabled").await;

    let (status, body) = whoami_as(&app, tenant, &session.access_token).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "UNAUTHORIZED");
}

// ------------------------------------------------------------ forbidden cases

#[tokio::test]
async fn invited_and_suspended_members_are_forbidden() {
    let Some(app) = spawn().await else { return };

    for status_name in ["invited", "suspended"] {
        let session = app.create_session().await;
        let tenant = app
            .seed_membership(session.user_id, "member", status_name)
            .await;

        let (status, body) = whoami_as(&app, tenant, &session.access_token).await;

        assert_eq!(status, StatusCode::FORBIDDEN, "{status_name}: {body}");
        assert_eq!(body["error"]["code"], "FORBIDDEN");
    }
}

#[tokio::test]
async fn a_suspended_tenant_is_forbidden_for_its_members() {
    let Some(app) = spawn().await else { return };
    let session = app.create_session().await;
    let tenant = app
        .seed_membership(session.user_id, "owner", "active")
        .await;

    app.set_tenant_status(tenant, "suspended").await;

    let (status, body) = whoami_as(&app, tenant, &session.access_token).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
}

// ------------------------------------------------- changes apply immediately

#[tokio::test]
async fn membership_changes_apply_to_the_same_still_valid_token() {
    let Some(app) = spawn().await else { return };
    let member = app.create_session().await;
    let tenant = app.create_tenant().await;
    app.add_member(tenant, member.user_id, "member", "active")
        .await;
    let token = &member.access_token;

    let (status, body) = whoami_as(&app, tenant, token).await;
    assert_eq!((status, &body["role"]), (StatusCode::OK, &json!("member")));

    // Role change shows on the very next request.
    app.update_member(tenant, member.user_id, "viewer", "active")
        .await;
    let (status, body) = whoami_as(&app, tenant, token).await;
    assert_eq!((status, &body["role"]), (StatusCode::OK, &json!("viewer")));

    // Suspension: 403 at once, and lifting it restores access at once.
    app.update_member(tenant, member.user_id, "viewer", "suspended")
        .await;
    assert_eq!(
        whoami_as(&app, tenant, token).await.0,
        StatusCode::FORBIDDEN
    );
    app.update_member(tenant, member.user_id, "viewer", "active")
        .await;
    assert_eq!(whoami_as(&app, tenant, token).await.0, StatusCode::OK);

    // Removal: the token is still valid, but the tenant now looks unknown.
    app.remove_member(tenant, member.user_id).await;
    let (status, body) = whoami_as(&app, tenant, token).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}
