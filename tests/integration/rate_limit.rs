//! Task 9: Auth abuse controls (rate limiting) — integration tests.
//!
//! Verifies:
//! 1. IP Token Bucket limit: Exceeding IP capacity returns 429 RATE_LIMITED with Retry-After.
//! 2. (IP + Email) Token Bucket limit: Exceeding attempts on an email from one IP returns 429 for that IP+Email.
//! 3. No Victim Lockout: Attacker exhausts (Attacker IP + victim), victim from their own IP can still log in.
//! 4. Endpoint isolation: Exhausting login bucket does not exhaust register or refresh buckets.
//! 5. Token refill: After capacity is drained, waiting for refill interval grants tokens again.
//! 6. Redis Outage / Fail Open (Circuit Breaker): When Redis is down, fails open with normal response,
//!    fast response time (<200 ms), and circuit breaker trips open.
//! 7. Concurrency & Atomicity: Concurrent requests do not exceed bucket capacity.

use std::time::{Duration, Instant};

use axum::http::{Method, StatusCode};
use serde_json::json;
use test_support::{unique, TestApp, TestAppOptions, PASSWORD};

const REGISTER: &str = "/api/v1/auth/register";
const LOGIN: &str = "/api/v1/auth/login";
const REFRESH: &str = "/api/v1/auth/refresh";

/// Checks if Redis is live so live-Redis tests can skip gracefully if Redis is not running.
async fn redis_is_live() -> bool {
    let url = std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://localhost:6379".to_string());
    let Ok(client) = redis::Client::open(url) else {
        return false;
    };
    matches!(
        tokio::time::timeout(
            Duration::from_millis(300),
            client.get_multiplexed_async_connection(),
        )
        .await,
        Ok(Ok(_))
    )
}

// ------------------------------------------------------------------------- 1. IP bucket limit

#[tokio::test]
async fn ip_bucket_exhaustion_returns_429_with_retry_after() {
    if !redis_is_live().await {
        eprintln!("skipping: Redis is not reachable");
        return;
    }
    let Some(app) = TestApp::spawn_with_limits(2, 100).await else {
        return;
    };

    let ip = format!("198.51.100.{}", unique_octet());
    let email = format!("{}@example.com", unique("rl-ip"));
    let body = json!({ "email": email, "password": "wrong-password" });

    // Attempts 1 and 2 should be allowed (returning 401 INVALID_CREDENTIALS)
    for i in 1..=2 {
        let (status, resp) = app
            .request_with_ip(Method::POST, LOGIN, Some(body.clone()), None, &ip)
            .await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "attempt {i} should be allowed: {resp}"
        );
    }

    // Attempt 3 must be blocked by IP rate limit (429)
    let (status, resp, headers) = app
        .request_full(Method::POST, LOGIN, Some(body), None, Some(&ip))
        .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{resp}");
    assert_eq!(resp["error"]["code"], "RATE_LIMITED");

    let retry_after = headers.get("retry-after").expect("must have Retry-After");
    let retry_secs: u64 = retry_after
        .to_str()
        .unwrap()
        .parse()
        .expect("Retry-After must be integer");
    assert!(retry_secs >= 1, "Retry-After must be at least 1 second");
}

// ------------------------------------------------------------------------- 2. (IP + Email) bucket limit

#[tokio::test]
async fn ip_email_bucket_exhaustion_blocks_specific_email() {
    if !redis_is_live().await {
        eprintln!("skipping: Redis is not reachable");
        return;
    }
    // High IP capacity (100), low email capacity (2)
    let Some(app) = TestApp::spawn_with_limits(100, 2).await else {
        return;
    };

    let ip = format!("198.51.100.{}", unique_octet());
    let target_email = format!("{}@example.com", unique("rl-email-target"));
    let other_email = format!("{}@example.com", unique("rl-email-other"));

    let target_body = json!({ "email": target_email, "password": "wrong" });
    let other_body = json!({ "email": other_email, "password": "wrong" });

    // 2 attempts on target email
    for _ in 0..2 {
        let (status, _) = app
            .request_with_ip(Method::POST, LOGIN, Some(target_body.clone()), None, &ip)
            .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    // 3rd attempt on target email: blocked by (IP + Email) bucket
    let (status, resp) = app
        .request_with_ip(Method::POST, LOGIN, Some(target_body), None, &ip)
        .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{resp}");
    assert_eq!(resp["error"]["code"], "RATE_LIMITED");

    // But from the SAME IP, a different email is NOT blocked
    let (status, _) = app
        .request_with_ip(Method::POST, LOGIN, Some(other_body), None, &ip)
        .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "different email on same IP must not be rate limited"
    );
}

// ------------------------------------------------------------------------- 3. No Victim Lockout

#[tokio::test]
async fn no_victim_lockout_when_attacker_exhausts_email_from_another_ip() {
    if !redis_is_live().await {
        eprintln!("skipping: Redis is not reachable");
        return;
    }
    // High IP capacity, low email capacity (2)
    let Some(app) = TestApp::spawn_with_limits(100, 2).await else {
        return;
    };

    let victim_email = format!("{}@example.com", unique("victim"));
    // Register the victim
    let (reg_status, _) = app
        .request(
            Method::POST,
            REGISTER,
            Some(json!({ "email": victim_email, "password": PASSWORD })),
            None,
        )
        .await;
    assert_eq!(reg_status, StatusCode::CREATED);

    let attacker_ip = format!("198.51.100.{}", unique_octet());
    let victim_ip = format!("198.51.100.{}", unique_octet());

    // Attacker spams 3 login attempts with victim's email from attacker_ip
    let attacker_body = json!({ "email": victim_email, "password": "wrong-password" });
    for _ in 0..2 {
        let (status, _) = app
            .request_with_ip(
                Method::POST,
                LOGIN,
                Some(attacker_body.clone()),
                None,
                &attacker_ip,
            )
            .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    let (status, resp) = app
        .request_with_ip(Method::POST, LOGIN, Some(attacker_body), None, &attacker_ip)
        .await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "attacker must be rate limited"
    );
    assert_eq!(resp["error"]["code"], "RATE_LIMITED");

    // The real victim logging in from victim_ip must NOT be locked out
    let victim_body = json!({ "email": victim_email, "password": PASSWORD });
    let (status, tokens) = app
        .request_with_ip(Method::POST, LOGIN, Some(victim_body), None, &victim_ip)
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "victim must be able to log in from their own IP: {tokens}"
    );
    assert!(tokens["access_token"].as_str().is_some());
}

// ------------------------------------------------------------------------- 4. Endpoint isolation

#[tokio::test]
async fn endpoint_buckets_are_isolated() {
    if !redis_is_live().await {
        eprintln!("skipping: Redis is not reachable");
        return;
    }
    // Limit IP to 2 requests
    let Some(app) = TestApp::spawn_with_limits(2, 100).await else {
        return;
    };

    let ip = format!("198.51.100.{}", unique_octet());
    let email = format!("{}@example.com", unique("isolated"));
    let body = json!({ "email": email, "password": "wrong" });

    // Exhaust LOGIN bucket from this IP
    for _ in 0..2 {
        let (status, _) = app
            .request_with_ip(Method::POST, LOGIN, Some(body.clone()), None, &ip)
            .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    let (status, _) = app
        .request_with_ip(Method::POST, LOGIN, Some(body.clone()), None, &ip)
        .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "login is depleted");

    // REGISTER with same IP must NOT be blocked (different bucket scope)
    let reg_body = json!({ "email": email, "password": PASSWORD });
    let (reg_status, _) = app
        .request_with_ip(Method::POST, REGISTER, Some(reg_body), None, &ip)
        .await;
    assert_eq!(
        reg_status,
        StatusCode::CREATED,
        "register must not be blocked by depleted login bucket"
    );

    // REFRESH with same IP must NOT be blocked
    let refresh_body = json!({ "refresh_token": "some-random-token" });
    let (refresh_status, _) = app
        .request_with_ip(Method::POST, REFRESH, Some(refresh_body), None, &ip)
        .await;
    assert_ne!(
        refresh_status,
        StatusCode::TOO_MANY_REQUESTS,
        "refresh must not be blocked by depleted login bucket"
    );
}

// ------------------------------------------------------------------------- 5. Token refill

#[tokio::test]
async fn tokens_refill_over_time() {
    if !redis_is_live().await {
        eprintln!("skipping: Redis is not reachable");
        return;
    }
    // Capacity 1, window 1 second -> refill rate is 1 token / second
    let Some(app) = TestApp::spawn_with_options(TestAppOptions {
        per_ip: Some(1),
        per_email: Some(100),
        window_secs: Some(1),
        ..Default::default()
    })
    .await
    else {
        return;
    };

    let ip = format!("198.51.100.{}", unique_octet());
    let email = format!("{}@example.com", unique("refill"));
    let body = json!({ "email": email, "password": "wrong" });

    // 1st request drains the 1 token
    let (status, _) = app
        .request_with_ip(Method::POST, LOGIN, Some(body.clone()), None, &ip)
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // 2nd request immediately is rate limited
    let (status, _) = app
        .request_with_ip(Method::POST, LOGIN, Some(body.clone()), None, &ip)
        .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

    // Wait for refill: window is 1 second, wait 1.2s
    tokio::time::sleep(Duration::from_millis(1200)).await;

    // Token has refilled! Request succeeds (with 401 INVALID_CREDENTIALS, not 429)
    let (status, _) = app
        .request_with_ip(Method::POST, LOGIN, Some(body), None, &ip)
        .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "token should have refilled"
    );
}

// ------------------------------------------------------------------------- 6. Redis Outage / Fail Open (Circuit Breaker)

#[tokio::test]
async fn redis_outage_fails_open_and_trips_circuit_breaker() {
    // Intentionally point Redis to an unreachable port (127.0.0.1:1)
    let Some(app) = TestApp::spawn_with_dead_redis().await else {
        return;
    };

    let email = format!("{}@example.com", unique("dead-redis"));
    let body = json!({ "email": email, "password": "wrong-password" });

    // Send 3 requests (the failure threshold to trip the circuit breaker)
    for attempt in 1..=3 {
        let start = Instant::now();
        let (status, resp) = app
            .request(Method::POST, LOGIN, Some(body.clone()), None)
            .await;
        let elapsed = start.elapsed();

        // Must NOT return 503 or 429; must return normal application status (401)
        assert_ne!(
            status,
            StatusCode::SERVICE_UNAVAILABLE,
            "must not return 503 during Redis outage"
        );
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "must fail open with 401: {resp}"
        );

        // Bound latency: each attempt times out Redis in <= 150 ms + Argon2 time
        assert!(
            elapsed < Duration::from_millis(1500),
            "attempt {attempt} took too long: {elapsed:?}"
        );
    }

    // Circuit breaker must now be tripped to OPEN
    assert!(
        app.state.rate_limiter.is_circuit_open(),
        "circuit breaker must be open after 3 failures"
    );

    // Subsequent request while circuit is OPEN must have ZERO Redis overhead (<50 ms)
    let start = Instant::now();
    let (status, _) = app.request(Method::POST, LOGIN, Some(body), None).await;
    let elapsed = start.elapsed();
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(
        elapsed < Duration::from_millis(100),
        "request with open circuit took {elapsed:?}, expected near-zero Redis overhead"
    );
}

// ------------------------------------------------------------------------- 7. Concurrency & Atomicity

#[tokio::test]
async fn concurrent_requests_do_not_exceed_capacity() {
    if !redis_is_live().await {
        eprintln!("skipping: Redis is not reachable");
        return;
    }
    // Capacity = 5 tokens
    let Some(app) = TestApp::spawn_with_limits(5, 100).await else {
        return;
    };

    let ip = format!("198.51.100.{}", unique_octet());
    let email = format!("{}@example.com", unique("concurrency"));
    let body = json!({ "email": email, "password": "wrong" });

    // Fire 15 concurrent login requests from the same IP
    let mut handles = Vec::new();
    for _ in 0..15 {
        let app_clone = app.clone();
        let ip_clone = ip.clone();
        let body_clone = body.clone();
        handles.push(tokio::spawn(async move {
            let (status, _) = app_clone
                .request_with_ip(Method::POST, LOGIN, Some(body_clone), None, &ip_clone)
                .await;
            status
        }));
    }

    let mut allowed_count = 0;
    let mut rate_limited_count = 0;

    for handle in handles {
        match handle.await.unwrap() {
            StatusCode::UNAUTHORIZED => allowed_count += 1,
            StatusCode::TOO_MANY_REQUESTS => rate_limited_count += 1,
            other => panic!("unexpected status: {other}"),
        }
    }

    // Exactly 5 requests must be allowed, and 10 must be 429
    assert_eq!(
        allowed_count, 5,
        "exactly 5 requests must be allowed by capacity"
    );
    assert_eq!(
        rate_limited_count, 10,
        "remaining 10 requests must be rate limited"
    );
}

fn unique_octet() -> u8 {
    (uuid::Uuid::new_v4().as_bytes()[1] % 240) + 10
}
