//! Shared test support: builds the real router against a real database.
//!
//! Tests run through the `app_runtime` pool (`DATABASE_URL`) so RLS applies
//! exactly as in production. The optional owner pool (`DATABASE_MIGRATION_URL`,
//! `db_owner`) is for fixture seeding and cleanup only.

use std::{
    io,
    sync::{Arc, Mutex, OnceLock},
};

use axum::{
    body::Body,
    http::{HeaderMap, Method, Request, StatusCode},
    Router,
};
use common_config::AppConfig;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::{postgres::PgPoolOptions, PgPool, Postgres, Transaction};
use tenant_saas::{app_state::AppState, routes::build_router};
use tower::ServiceExt;
use uuid::Uuid;

/// Everything the app logged during this test run (all tests share it).
#[derive(Clone, Default)]
struct LogCapture(Arc<Mutex<Vec<u8>>>);

impl io::Write for LogCapture {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

static LOGS: OnceLock<LogCapture> = OnceLock::new();

/// Installs one global capturing subscriber. It must be global, not per test:
/// tracing caches whether a callsite is enabled, and a thread-local subscriber
/// loses that race when other tests run in parallel without one.
fn capture_logs() {
    LOGS.get_or_init(|| {
        let capture = LogCapture::default();
        let writer = capture.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::set_global_default(subscriber)
            .expect("no other global tracing subscriber in tests");
        capture
    });
}

/// All logs captured so far. Search it for strings unique to your test.
pub fn captured_logs() -> String {
    let capture = LOGS.get().expect("call TestApp::spawn first");
    String::from_utf8_lossy(&capture.0.lock().unwrap()).into_owned()
}

#[derive(Default, Clone)]
pub struct TestAppOptions {
    pub per_ip: Option<u32>,
    pub per_email: Option<u32>,
    pub window_secs: Option<u64>,
    pub redis_url: Option<String>,
    pub trusted_proxy_hops: Option<u8>,
}

#[derive(Clone)]
pub struct TestApp {
    router: Router,
    pub state: AppState,
    /// `app_runtime` pool: the same role the production app uses.
    pub runtime_pool: PgPool,
    /// `db_owner` pool for seeding/cleanup; `None` when `DATABASE_MIGRATION_URL` is unset.
    pub owner_pool: Option<PgPool>,
    /// Unique random client IP assigned to this test app instance.
    pub client_ip: String,
}

impl TestApp {
    /// Builds the app with default test options, or returns `None` when `SKIP_DB_TESTS=1`.
    pub async fn spawn() -> Option<Self> {
        Self::spawn_with_options(TestAppOptions::default()).await
    }

    /// Spawns the test app with custom per-IP and per-email rate limits.
    pub async fn spawn_with_limits(per_ip: u32, per_email: u32) -> Option<Self> {
        Self::spawn_with_options(TestAppOptions {
            per_ip: Some(per_ip),
            per_email: Some(per_email),
            ..Default::default()
        })
        .await
    }

    /// Spawns the test app pointing Redis to a dead port (127.0.0.1:1).
    pub async fn spawn_with_dead_redis() -> Option<Self> {
        Self::spawn_with_options(TestAppOptions {
            redis_url: Some("redis://127.0.0.1:1".to_string()),
            ..Default::default()
        })
        .await
    }

    /// Builds the app with custom options, or returns `None` when `SKIP_DB_TESTS=1`.
    pub async fn spawn_with_options(options: TestAppOptions) -> Option<Self> {
        let _ = dotenvy::dotenv();
        capture_logs();

        let env = |key: &str| match key {
            "AUTH_RATE_LIMIT_TRUSTED_PROXY_HOPS" => {
                Some(options.trusted_proxy_hops.unwrap_or(1).to_string())
            }
            "AUTH_RATE_LIMIT_PER_IP" => Some(options.per_ip.unwrap_or(10_000).to_string()),
            "AUTH_RATE_LIMIT_PER_EMAIL" => Some(options.per_email.unwrap_or(10_000).to_string()),
            "AUTH_RATE_LIMIT_WINDOW_SECS" => options.window_secs.map(|w| w.to_string()),
            "REDIS_URL" => options
                .redis_url
                .clone()
                .or_else(|| std::env::var("REDIS_URL").ok()),
            _ => std::env::var(key).ok().or_else(|| match key {
                "OBJECT_STORAGE_ACCESS_KEY" | "OBJECT_STORAGE_SECRET_KEY" => Some("test".into()),
                "JWT_SECRET" => Some("test-secret-".repeat(4)),
                _ => None,
            }),
        };

        if std::env::var("SKIP_DB_TESTS").as_deref() == Ok("1") {
            eprintln!("SKIP_DB_TESTS=1 is set: skipping DB-backed test");
            return None;
        }

        if env("DATABASE_URL").is_none() {
            panic!("DATABASE_URL is required for DB-backed tests (set SKIP_DB_TESTS=1 to skip locally)");
        }

        let config = AppConfig::load_from_env_getter(env).expect("test config must load");
        let runtime_pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(&config.database.url)
            .await
            .expect("connect as app_runtime");
        let owner_pool = match config.database.migration_url.as_deref() {
            Some(url) => Some(
                PgPoolOptions::new()
                    .max_connections(2)
                    .connect(url)
                    .await
                    .expect("connect as db_owner"),
            ),
            None => None,
        };

        let state =
            AppState::new(runtime_pool.clone(), Arc::new(config)).expect("valid test config");

        // Unique client IP per test app instance so tests running concurrently don't share buckets
        let client_ip = format!(
            "198.51.100.{}",
            (Uuid::new_v4().as_bytes()[0] as u32 % 250) + 1
        );

        Some(Self {
            router: build_router(state.clone()),
            state,
            runtime_pool,
            owner_pool,
            client_ip,
        })
    }

    /// Adds test-only routes that share this app's state (and so its database).
    pub fn with_routes(mut self, routes: Router<AppState>) -> Self {
        self.router = self.router.merge(routes.with_state(self.state.clone()));
        self
    }

    /// POSTs a raw string body with a JSON content type, for malformed-input tests.
    pub async fn post_raw(&self, path: &str, body: String) -> StatusCode {
        let request = Request::builder()
            .method(Method::POST)
            .uri(path)
            .header("content-type", "application/json")
            .header("x-forwarded-for", format!("{}, 10.0.0.1", self.client_ip))
            .body(Body::from(body))
            .expect("valid request");
        self.router
            .clone()
            .oneshot(request)
            .await
            .expect("router is infallible")
            .status()
    }

    /// Sends a request through the router using this app instance's client IP.
    pub async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        bearer: Option<&str>,
    ) -> (StatusCode, Value) {
        self.request_with_ip(method, path, body, bearer, &self.client_ip)
            .await
    }

    /// Sends a request with a custom client IP.
    pub async fn request_with_ip(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        bearer: Option<&str>,
        ip: &str,
    ) -> (StatusCode, Value) {
        let (status, body, _) = self
            .request_full(method, path, body, bearer, Some(ip))
            .await;
        (status, body)
    }

    /// Sends a request and returns status, JSON body, and all response headers.
    pub async fn request_full(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        bearer: Option<&str>,
        ip: Option<&str>,
    ) -> (StatusCode, Value, HeaderMap) {
        let mut builder = Request::builder().method(method).uri(path);
        if let Some(token) = bearer {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        if let Some(ip) = ip {
            builder = builder.header("x-forwarded-for", format!("{ip}, 10.0.0.1"));
        }
        let request = match body {
            Some(json) => builder
                .header("content-type", "application/json")
                .body(Body::from(json.to_string())),
            None => builder.body(Body::empty()),
        }
        .expect("valid request");

        let response = self
            .router
            .clone()
            .oneshot(request)
            .await
            .expect("router is infallible");
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("read body")
            .to_bytes();
        let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, json, headers)
    }
}

pub const PASSWORD: &str = "correct-horse-battery";

/// A registered and logged-in user.
pub struct Session {
    pub email: String,
    pub user_id: Uuid,
    pub access_token: String,
    pub refresh_token: String,
}

impl TestApp {
    /// The `db_owner` pool, for fixtures only. Fails the test if it is not configured.
    pub fn owner(&self) -> &PgPool {
        self.owner_pool
            .as_ref()
            .expect("DATABASE_MIGRATION_URL is required for this test")
    }

    /// Registers a new user through the API and logs them in.
    pub async fn create_session(&self) -> Session {
        let email = format!("{}@example.com", unique("user"));
        let credentials = json!({ "email": email, "password": PASSWORD });

        let (status, body) = self
            .request(
                Method::POST,
                "/api/v1/auth/register",
                Some(credentials.clone()),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "register failed: {body}");
        let user_id = body["id"].as_str().unwrap().parse().unwrap();

        let (status, tokens) = self
            .request(Method::POST, "/api/v1/auth/login", Some(credentials), None)
            .await;
        assert_eq!(status, StatusCode::OK, "login failed: {tokens}");

        Session {
            email,
            user_id,
            access_token: tokens["access_token"].as_str().unwrap().to_owned(),
            refresh_token: tokens["refresh_token"].as_str().unwrap().to_owned(),
        }
    }

    /// Owner-pool transaction with the same settings the app would set. RLS still
    /// applies to `db_owner` on `tenants` and `memberships` (FORCE), so fixtures
    /// must set them too.
    async fn owner_scoped_tx(
        &self,
        tenant_id: Uuid,
        user_id: Uuid,
    ) -> Transaction<'static, Postgres> {
        let mut tx = self.owner().begin().await.expect("begin");
        sqlx::query(
            "SELECT set_config('app.current_tenant_id', $1, true),
                    set_config('app.current_user_id', $2, true)",
        )
        .bind(tenant_id.to_string())
        .bind(user_id.to_string())
        .execute(&mut *tx)
        .await
        .expect("set scope");
        tx
    }

    /// Creates an empty tenant (no members) and returns its id.
    pub async fn create_tenant(&self) -> Uuid {
        let tenant_id = Uuid::new_v4();
        let mut tx = self.owner_scoped_tx(tenant_id, Uuid::new_v4()).await;
        sqlx::query("INSERT INTO tenants (id, name, slug) VALUES ($1, $2, $3)")
            .bind(tenant_id)
            .bind(format!("Tenant {}", &tenant_id.simple().to_string()[..8]))
            .bind(unique("t"))
            .execute(&mut *tx)
            .await
            .expect("insert tenant");
        tx.commit().await.expect("commit");
        tenant_id
    }

    /// Ensures a user row exists in the `users` table for `user_id`.
    pub async fn ensure_user_exists(&self, user_id: Uuid) {
        let email = format!("user-{}@example.com", user_id);
        sqlx::query(
            "INSERT INTO users (id, email, password_hash)
             VALUES ($1, $2, 'dummy_hash')
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(user_id)
        .bind(email)
        .execute(self.owner())
        .await
        .expect("ensure user exists");
    }

    /// Adds `user_id` to an existing tenant.
    pub async fn add_member(&self, tenant_id: Uuid, user_id: Uuid, role: &str, status: &str) {
        self.ensure_user_exists(user_id).await;
        let mut tx = self.owner_scoped_tx(tenant_id, user_id).await;
        sqlx::query(
            "INSERT INTO memberships (tenant_id, user_id, role, status) VALUES ($1, $2, $3, $4)",
        )
        .bind(tenant_id)
        .bind(user_id)
        .bind(role)
        .bind(status)
        .execute(&mut *tx)
        .await
        .expect("insert membership");
        tx.commit().await.expect("commit");
    }

    /// Creates a new tenant with one membership for `user_id`.
    pub async fn seed_membership(&self, user_id: Uuid, role: &str, status: &str) -> Uuid {
        let tenant_id = self.create_tenant().await;
        self.add_member(tenant_id, user_id, role, status).await;
        tenant_id
    }

    pub async fn set_tenant_status(&self, tenant_id: Uuid, status: &str) {
        let mut tx = self.owner_scoped_tx(tenant_id, Uuid::new_v4()).await;
        let result = sqlx::query("UPDATE tenants SET status = $2 WHERE id = $1")
            .bind(tenant_id)
            .bind(status)
            .execute(&mut *tx)
            .await
            .expect("update tenant status");
        assert_eq!(result.rows_affected(), 1);
        tx.commit().await.expect("commit");
    }

    /// Changes a membership's role and status. Do not use it to demote the last owner.
    pub async fn update_member(&self, tenant_id: Uuid, user_id: Uuid, role: &str, status: &str) {
        let mut tx = self.owner_scoped_tx(tenant_id, user_id).await;
        let result = sqlx::query(
            "UPDATE memberships SET role = $3, status = $4 WHERE tenant_id = $1 AND user_id = $2",
        )
        .bind(tenant_id)
        .bind(user_id)
        .bind(role)
        .bind(status)
        .execute(&mut *tx)
        .await
        .expect("update membership");
        assert_eq!(result.rows_affected(), 1);
        tx.commit().await.expect("commit");
    }

    pub async fn remove_member(&self, tenant_id: Uuid, user_id: Uuid) {
        let mut tx = self.owner_scoped_tx(tenant_id, user_id).await;
        let result = sqlx::query("DELETE FROM memberships WHERE tenant_id = $1 AND user_id = $2")
            .bind(tenant_id)
            .bind(user_id)
            .execute(&mut *tx)
            .await
            .expect("delete membership");
        assert_eq!(result.rows_affected(), 1);
        tx.commit().await.expect("commit");
    }

    pub async fn set_user_status(&self, user_id: Uuid, status: &str) {
        sqlx::query("UPDATE users SET status = $2 WHERE id = $1")
            .bind(user_id)
            .bind(status)
            .execute(self.owner())
            .await
            .expect("update user status");
    }

    pub async fn expire_refresh_tokens(&self, user_id: Uuid) {
        sqlx::query(
            "UPDATE refresh_tokens SET expires_at = now() - interval '1 minute' WHERE user_id = $1",
        )
        .bind(user_id)
        .execute(self.owner())
        .await
        .expect("expire tokens");
    }
}

/// Unique suffix so parallel tests sharing one database never collide on emails/slugs.
pub fn unique(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4().simple())
}
