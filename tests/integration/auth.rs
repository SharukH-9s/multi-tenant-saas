//! Task 6: `/api/v1/auth/*` against a real database, through `app_runtime`.

use axum::http::{Method, StatusCode};
use common_auth::{TokenService, TokenSettings};
use serde_json::{json, Value};
use test_support::{captured_logs, unique, TestApp, PASSWORD};

const REGISTER: &str = "/api/v1/auth/register";
const LOGIN: &str = "/api/v1/auth/login";
const REFRESH: &str = "/api/v1/auth/refresh";
const LOGOUT: &str = "/api/v1/auth/logout";
const ME: &str = "/api/v1/auth/me";

async fn spawn() -> Option<TestApp> {
    TestApp::spawn().await
}

async fn post(app: &TestApp, path: &str, body: Value, bearer: Option<&str>) -> (StatusCode, Value) {
    app.request(Method::POST, path, Some(body), bearer).await
}

async fn get_me(app: &TestApp, bearer: Option<&str>) -> (StatusCode, Value) {
    app.request(Method::GET, ME, None, bearer).await
}

async fn refresh(app: &TestApp, token: &str) -> (StatusCode, Value) {
    post(app, REFRESH, json!({ "refresh_token": token }), None).await
}

/// The parts of an error that must not differ between failure causes.
fn error_shape(status: StatusCode, body: &Value) -> (StatusCode, Value, Value) {
    (
        status,
        body["error"]["code"].clone(),
        body["error"]["message"].clone(),
    )
}

// ---------------------------------------------------------------- register

#[tokio::test]
async fn register_creates_a_user_without_exposing_secrets() {
    let Some(app) = spawn().await else { return };
    let email = format!("{}@Example.com", unique("Reg"));

    let (status, body) = post(
        &app,
        REGISTER,
        json!({ "email": email, "password": PASSWORD }),
        None,
    )
    .await;

    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["email"], email.to_lowercase());
    assert!(body["id"].as_str().is_some());
    assert!(body["created_at"].as_str().is_some());
    assert!(body.get("password_hash").is_none());
    assert!(body.get("password").is_none());
    assert!(
        body.get("access_token").is_none(),
        "register issues no tokens"
    );
}

#[tokio::test]
async fn register_rejects_a_duplicate_email_ignoring_case() {
    let Some(app) = spawn().await else { return };
    let session = app.create_session().await;

    let (status, body) = post(
        &app,
        REGISTER,
        json!({ "email": session.email.to_uppercase(), "password": PASSWORD }),
        None,
    )
    .await;

    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "EMAIL_ALREADY_REGISTERED");
}

#[tokio::test]
async fn register_validates_input() {
    let Some(app) = spawn().await else { return };
    let ok_email = format!("{}@example.com", unique("val"));

    let cases = [
        json!({ "email": "not-an-email", "password": PASSWORD }),
        json!({ "email": ok_email, "password": "short" }),
        json!({ "email": ok_email, "password": "x".repeat(129) }),
        json!({ "email": ok_email }),
    ];
    for body in cases {
        let (status, response) = post(&app, REGISTER, body.clone(), None).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{body} -> {response}"
        );
        assert_eq!(response["error"]["code"], "VALIDATION_ERROR");
    }
}

#[tokio::test]
async fn register_rejects_malformed_and_oversized_bodies() {
    let Some(app) = spawn().await else { return };

    // Not JSON at all.
    let status = app.post_raw(REGISTER, "{nope".into()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Larger than the 16 KiB limit.
    let (status, _) = post(
        &app,
        REGISTER,
        json!({ "email": "a@example.com", "password": "x".repeat(20_000) }),
        None,
    )
    .await;
    assert!(status.is_client_error(), "oversized body gave {status}");
}

// ------------------------------------------------------------------- login

#[tokio::test]
async fn login_returns_tokens() {
    let Some(app) = spawn().await else { return };
    let session = app.create_session().await;

    let (status, body) = post(
        &app,
        LOGIN,
        json!({ "email": session.email.to_uppercase(), "password": PASSWORD }),
        None,
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["token_type"], "Bearer");
    assert!(body["expires_in"].as_u64().unwrap() > 0);
    assert!(!body["access_token"].as_str().unwrap().is_empty());
    assert!(!body["refresh_token"].as_str().unwrap().is_empty());
}

#[tokio::test]
async fn every_login_failure_looks_the_same() {
    let Some(app) = spawn().await else { return };
    let session = app.create_session().await;
    let disabled = app.create_session().await;
    app.set_user_status(disabled.user_id, "disabled").await;

    let attempts = [
        // Unknown email.
        json!({ "email": format!("{}@example.com", unique("ghost")), "password": PASSWORD }),
        // Wrong password for a real user.
        json!({ "email": session.email, "password": "definitely-wrong-password" }),
        // Correct password, but the account is disabled.
        json!({ "email": disabled.email, "password": PASSWORD }),
        // Not even an email.
        json!({ "email": "nope", "password": PASSWORD }),
    ];

    let mut shapes = Vec::new();
    for body in attempts {
        let (status, response) = post(&app, LOGIN, body, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{response}");
        assert_eq!(response["error"]["code"], "INVALID_CREDENTIALS");
        shapes.push(error_shape(status, &response));
    }
    assert!(shapes.windows(2).all(|pair| pair[0] == pair[1]));
}

// ---------------------------------------------------------------------- me

#[tokio::test]
async fn me_returns_the_user_and_only_their_active_memberships() {
    let Some(app) = spawn().await else { return };
    let alice = app.create_session().await;
    let bob = app.create_session().await;

    let alice_tenant = app.seed_membership(alice.user_id, "owner", "active").await;
    let _invited_only = app
        .seed_membership(alice.user_id, "member", "invited")
        .await;
    let bob_tenant = app.seed_membership(bob.user_id, "owner", "active").await;

    let (status, body) = get_me(&app, Some(&alice.access_token)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["id"], alice.user_id.to_string());
    assert_eq!(body["email"], alice.email);
    assert!(body.get("password_hash").is_none());

    let memberships = body["memberships"].as_array().unwrap();
    assert_eq!(
        memberships.len(),
        1,
        "invited membership must not be listed"
    );
    assert_eq!(memberships[0]["tenant_id"], alice_tenant.to_string());
    assert_eq!(memberships[0]["role"], "owner");
    assert!(memberships[0]["slug"].as_str().is_some());
    assert!(memberships[0]["name"].as_str().is_some());

    // Bob sees his own tenant and never Alice's.
    let (_, body) = get_me(&app, Some(&bob.access_token)).await;
    let ids: Vec<&str> = body["memberships"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["tenant_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [bob_tenant.to_string()]);
}

#[tokio::test]
async fn me_rejects_bad_credentials_with_one_generic_401() {
    let Some(app) = spawn().await else { return };
    let session = app.create_session().await;

    let wrong_secret = TokenService::new(TokenSettings {
        secret: "another-secret-another-secret-another".into(),
        issuer: "tenant-saas".into(),
        audience: "tenant-saas-api".into(),
        expiry_secs: 900,
    })
    .issue_access(session.user_id)
    .unwrap()
    .token;

    let candidates: [Option<&str>; 5] = [
        None,
        Some(""),
        Some("garbage"),
        Some(&wrong_secret),
        // A refresh token is not an access token.
        Some(&session.refresh_token),
    ];

    let mut shapes = Vec::new();
    for bearer in candidates {
        let (status, body) = get_me(&app, bearer).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{bearer:?} -> {body}");
        assert_eq!(body["error"]["code"], "UNAUTHORIZED");
        shapes.push(error_shape(status, &body));
    }
    assert!(shapes.windows(2).all(|pair| pair[0] == pair[1]));
}

#[tokio::test]
async fn me_rejects_a_disabled_user_even_with_a_valid_token() {
    let Some(app) = spawn().await else { return };
    let session = app.create_session().await;
    app.set_user_status(session.user_id, "disabled").await;

    let (status, _) = get_me(&app, Some(&session.access_token)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ----------------------------------------------------------------- refresh

#[tokio::test]
async fn refresh_rotates_the_token() {
    let Some(app) = spawn().await else { return };
    let session = app.create_session().await;

    let (status, body) = refresh(&app, &session.refresh_token).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["token_type"], "Bearer");
    let new_refresh = body["refresh_token"].as_str().unwrap();
    assert_ne!(new_refresh, session.refresh_token);

    // The new access token works, and so does a second rotation.
    let (status, _) = get_me(&app, body["access_token"].as_str()).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = refresh(&app, new_refresh).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn reusing_a_rotated_token_revokes_the_whole_family() {
    let Some(app) = spawn().await else { return };
    let session = app.create_session().await;

    let (_, rotated) = refresh(&app, &session.refresh_token).await;
    let newest = rotated["refresh_token"].as_str().unwrap();

    // Replaying the old token is rejected...
    let (status, body) = refresh(&app, &session.refresh_token).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "UNAUTHORIZED");

    // ...and the legitimate newer token is revoked along with it.
    let (status, _) = refresh(&app, newest).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn reuse_in_one_session_does_not_touch_another_session() {
    let Some(app) = spawn().await else { return };
    let first = app.create_session().await;

    let (_, second) = post(
        &app,
        LOGIN,
        json!({ "email": first.email, "password": PASSWORD }),
        None,
    )
    .await;
    let second_refresh = second["refresh_token"].as_str().unwrap();

    refresh(&app, &first.refresh_token).await;
    let (status, _) = refresh(&app, &first.refresh_token).await; // reuse
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _) = refresh(&app, second_refresh).await;
    assert_eq!(status, StatusCode::OK, "other logins keep working");
}

#[tokio::test]
async fn refresh_rejects_unknown_and_expired_tokens() {
    let Some(app) = spawn().await else { return };
    let session = app.create_session().await;

    let (status, _) = refresh(&app, "this-token-was-never-issued").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    app.expire_refresh_tokens(session.user_id).await;
    let (status, _) = refresh(&app, &session.refresh_token).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn refresh_for_a_disabled_user_fails_and_stays_revoked() {
    let Some(app) = spawn().await else { return };
    let session = app.create_session().await;
    app.set_user_status(session.user_id, "disabled").await;

    let (status, _) = refresh(&app, &session.refresh_token).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Re-enabling the user must not bring the old session back.
    app.set_user_status(session.user_id, "active").await;
    let (status, _) = refresh(&app, &session.refresh_token).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ------------------------------------------------------------------ logout

#[tokio::test]
async fn logout_revokes_the_session_and_is_idempotent() {
    let Some(app) = spawn().await else { return };
    let session = app.create_session().await;
    let body = json!({ "refresh_token": session.refresh_token });

    let (status, _) = post(&app, LOGOUT, body.clone(), Some(&session.access_token)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _) = refresh(&app, &session.refresh_token).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _) = post(&app, LOGOUT, body, Some(&session.access_token)).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "second logout is still 204");
}

#[tokio::test]
async fn logout_requires_authentication() {
    let Some(app) = spawn().await else { return };
    let session = app.create_session().await;

    let (status, _) = post(
        &app,
        LOGOUT,
        json!({ "refresh_token": session.refresh_token }),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _) = refresh(&app, &session.refresh_token).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "unauthenticated logout revoked nothing"
    );
}

#[tokio::test]
async fn logout_cannot_revoke_another_users_session() {
    let Some(app) = spawn().await else { return };
    let victim = app.create_session().await;
    let attacker = app.create_session().await;

    let (status, _) = post(
        &app,
        LOGOUT,
        json!({ "refresh_token": victim.refresh_token }),
        Some(&attacker.access_token),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "no hint that the token was real"
    );

    let (status, _) = refresh(&app, &victim.refresh_token).await;
    assert_eq!(status, StatusCode::OK, "victim's session is untouched");
}

// -------------------------------------------------------------------- logs

#[tokio::test]
async fn secrets_never_appear_in_logs() {
    let Some(app) = spawn().await else { return };

    let email = format!("{}@example.com", unique("logs"));
    let password = "a-very-unlikely-password-xyz";
    let credentials = json!({ "email": email, "password": password });

    let (status, registered) = post(&app, REGISTER, credentials.clone(), None).await;
    assert_eq!(status, StatusCode::CREATED);
    let user_id = registered["id"].as_str().unwrap().to_owned();
    let (_, tokens) = post(&app, LOGIN, credentials, None).await;
    let access = tokens["access_token"].as_str().unwrap().to_owned();
    let refresh_token = tokens["refresh_token"].as_str().unwrap().to_owned();

    let (status, rotated) = refresh(&app, &refresh_token).await;
    assert_eq!(status, StatusCode::OK);
    let rotated_refresh = rotated["refresh_token"].as_str().unwrap().to_owned();
    get_me(&app, Some(&access)).await;
    post(
        &app,
        LOGIN,
        json!({ "email": email, "password": "wrong-wrong-wrong" }),
        None,
    )
    .await;
    refresh(&app, &refresh_token).await; // reuse, logs a warning
    post(
        &app,
        LOGOUT,
        json!({ "refresh_token": rotated_refresh }),
        Some(&access),
    )
    .await;

    // The log buffer is shared by every test; the strings below are unique to this one.
    let logs = captured_logs();

    // Prove capturing works, so the checks below mean something: this user's
    // audit events must be there (they carry the user id, nothing else).
    let ours: Vec<&str> = logs.lines().filter(|l| l.contains(&user_id)).collect();
    for event in [
        "user.registered",
        "auth.login",
        "auth.login_failed",
        "auth.refresh_reuse_detected",
        "auth.logout",
    ] {
        assert!(
            ours.iter().any(|l| l.contains(event)),
            "missing audit event {event}"
        );
    }

    for (name, secret) in [
        ("password", password),
        ("wrong password", "wrong-wrong-wrong"),
        ("access token", access.as_str()),
        ("refresh token", refresh_token.as_str()),
        ("rotated refresh token", rotated_refresh.as_str()),
        ("email", email.as_str()),
        ("password hash", "$argon2"),
    ] {
        assert!(!logs.contains(secret), "{name} leaked into logs");
    }
}
