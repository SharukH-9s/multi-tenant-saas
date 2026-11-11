# Phase 1 — Foundation Implementation Plan (Revised)
**Project:** Multi-Tenant AI SaaS Platform (`portfolio-engine`)\
**Timeline:** Week 1 — ~15–20 focused hours (treat as aggressive, not guaranteed)\
**Rust stack:** Axum · SQLx · Tokio · PostgreSQL 16 · Docker Compose

---

> [!IMPORTANT]
> **Phase 1 exit criteria — all must pass before moving to Phase 2**
>
> | Requirement | Expected result |
> |---|---|
> | Fresh clone | Reproducible setup from documented steps only |
> | Docker Compose | All services healthy: `docker compose up -d` |
> | Database migration | Applies cleanly from empty database |
> | Database credentials | `app_runtime` role connects and runs queries |
> | Runtime privileges | `app_runtime` cannot execute migration-owner operations |
> | API startup | Starts without manual steps beyond documented setup |
> | Liveness | Returns `200` even when PostgreSQL is stopped |
> | Readiness | Returns `503` when PostgreSQL is stopped |
> | Request IDs | Present in both responses and log output |
> | Error handling | No internal DB details or stack traces leak to clients |
> | CI | Format, Clippy, unit tests, migration check, and release build all pass |
> | Clean restart | `docker compose down -v && docker compose up -d` → system works as documented |

---

## Priority tiers

> [!NOTE]
> Phase 1 should establish a **reliable foundation**, not demonstrate every technology in the architecture. Do not sacrifice correctness to finish the checklist.

**Must complete before Phase 2:**
- Workspace, toolchain pin, crate structure
- `common-errors`, `common-config`, `common-telemetry`, `common-database`
- Phase 1 migration (extensions + roles only)
- Axum app: `AppState`, request ID, trace middleware, timeout
- `/health/live` and `/health/ready`
- Docker Compose with automated MinIO init
- CI: format, Clippy, tests, migration validation, release build
- README with reproducible setup

**Complete if time permits:**
- Multi-stage Dockerfile with `cargo-chef` optimization
- `SQLX_OFFLINE` cache generation and commit
- ADR documents (ADR-001 through ADR-003)
- `docs/adr/` and `docs/architecture/` stubs

---

## Overview

```mermaid
flowchart LR
    A["1. Workspace\n& Toolchain"] --> B["2. Core\nShared Crates"]
    B --> C["3. PostgreSQL\nFoundation"]
    C --> D["4. Axum App\n& Middleware"]
    D --> E["5. Health\nEndpoints"]
    E --> F["6. Docker Compose\n(automated)"]
    F --> G["7. CI &\nDockerfile"]
    G --> H["8. Docs &\nVerification"]
```

---

## Task 1 — Cargo Workspace & Toolchain Pin

### 1.1 — Directory structure

```
portfolio-engine/
├── Cargo.toml              ← workspace manifest
├── Cargo.lock
├── rust-toolchain.toml
├── .gitignore
├── .env.example
├── README.md
├── crates/
│   ├── common-config/
│   ├── common-errors/
│   ├── common-auth/        ← stub only; Phase 2
│   ├── common-telemetry/
│   ├── common-database/
│   ├── common-storage/     ← stub only; Phase 3
│   └── common-messaging/   ← stub only; Phase 4
├── apps/
│   └── tenant-saas/
│       ├── Cargo.toml
│       ├── Dockerfile
│       ├── migrations/
│       └── src/
├── infra/
│   ├── docker-compose.yml
│   ├── postgres/
│   │   └── init/
│   │       └── 00_roles.sql
│   └── minio/
│       └── init.sh
├── docs/
│   └── adr/
└── tests/
    └── integration/
```

### 1.2 — Root `Cargo.toml`

```toml
[workspace]
resolver = "2"
members = [
    "crates/common-config",
    "crates/common-errors",
    "crates/common-auth",
    "crates/common-telemetry",
    "crates/common-database",
    "crates/common-storage",
    "crates/common-messaging",
    "apps/tenant-saas",
]

[workspace.dependencies]
tokio             = { version = "1", features = ["full"] }
axum              = { version = "0.7", features = ["macros"] }
sqlx              = { version = "0.8", features = ["postgres", "uuid", "chrono", "runtime-tokio-native-tls"] }
serde             = { version = "1", features = ["derive"] }
serde_json        = "1"
uuid              = { version = "1", features = ["v4", "serde"] }
chrono            = { version = "0.4", features = ["serde"] }
tracing           = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "json"] }
thiserror         = "1"
anyhow            = "1"
config            = "0.14"
dotenvy           = "0.15"
tower             = "0.4"
tower-http        = { version = "0.5", features = ["trace", "request-id", "cors", "timeout"] }
```

### 1.3 — `rust-toolchain.toml`

```toml
[toolchain]
channel = "stable"
# Pin to a specific version once CI builds are verified clean:
# channel = "1.82.0"
components = ["rustfmt", "clippy"]
```

### 1.4 — `.gitignore`

```gitignore
/target
**/*.rs.bk
.env
.env.local
*.pem
*.key
.sqlx/   # generated; commit only after offline mode is confirmed working
```

> [!NOTE]
> Commit `.sqlx/` once `SQLX_OFFLINE=true` is confirmed working in CI. Until then, keep it out of the repo to avoid stale cache issues.

### 1.5 — `.env.example`

> [!IMPORTANT]
> **Fix #1 — Consistent database roles.** The application (`app_runtime`) and the migration runner (`db_owner`) are separate roles with different credentials. Never use the same account for both.

```dotenv
APP_ENV=development
APP_HOST=0.0.0.0
APP_PORT=8080
RUST_LOG=info,tenant_saas=debug

# Application runtime account (limited privileges — no DDL, no schema ownership)
DATABASE_URL=postgres://app_runtime:runtime_password@localhost:5432/tenant_saas
DATABASE_MAX_CONNECTIONS=10
DATABASE_ACQUIRE_TIMEOUT_SECS=5

# Migration account (used by sqlx-cli only, not by the running application)
DATABASE_MIGRATION_URL=postgres://db_owner:owner_password@localhost:5432/tenant_saas

REDIS_URL=redis://localhost:6379

OBJECT_STORAGE_ENDPOINT=http://localhost:9000
OBJECT_STORAGE_BUCKET=tenant-documents
OBJECT_STORAGE_REGION=us-east-1
OBJECT_STORAGE_ACCESS_KEY=minioadmin
OBJECT_STORAGE_SECRET_KEY=minioadmin

JWT_SECRET=changeme-dev-only-32-chars-minimum
JWT_EXPIRY_SECS=3600
```

**Deliverable checklist:**
- [x] `cargo build --workspace` succeeds
- [x] `cargo fmt --check --all` passes
- [x] `cargo clippy --all-targets --all-features -- -D warnings` passes

---

## Task 2 — Core Shared Crates

Build only what Phase 1 needs. Mark Phase 2+ crates as stubs.

### 2.1 — `common-errors` *(build first; everything else depends on it)*

**Responsibilities:**
- `AppError` enum: `Database`, `NotFound`, `Unauthorized`, `Forbidden`, `Conflict`, `Validation`, `Internal`, `ExternalService`
- Axum `IntoResponse` mapping to a safe JSON error envelope
- `From<sqlx::Error>` and `From<std::io::Error>` impls

```rust
// Error response shape (§13.1 of design doc)
// {"error":{"code":"...","message":"...","request_id":"..."}}
```

> [!CAUTION]
> Never pass `sqlx::Error` messages directly to clients. Map to safe variants with generic messages. SQL errors, stack traces, and table names must not appear in HTTP responses.

### 2.2 — `common-config`

**Responsibilities:**
- Typed configuration structs, loaded from environment variables at startup
- Fail fast on missing required values — panic, do not silently default
- Provides both `DATABASE_URL` (runtime) and `DATABASE_MIGRATION_URL` (migration-only) as separate typed fields

```rust
#[derive(Debug, Deserialize)]
pub struct AppConfig {
    pub app: AppSettings,
    pub database: DatabaseConfig,
    pub redis: RedisConfig,
    pub storage: StorageConfig,
    pub auth: AuthConfig,
}

#[derive(Debug, Deserialize)]
pub struct DatabaseConfig {
    pub url: String,               // app_runtime account
    pub migration_url: Option<String>, // db_owner account; only needed by migration tooling
    pub max_connections: u32,
    pub acquire_timeout_secs: u64,
}
```

### 2.3 — `common-telemetry`

**Responsibilities:**
- Initialize `tracing-subscriber` with structured JSON output
- Respect `RUST_LOG` for filter control
- Expose a single `init_tracing(rust_log: &str)` function called once at binary startup
- Phase 1: logging only. OpenTelemetry exporter is wired in Phase 7.

```rust
pub fn init_tracing(rust_log: &str) {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(EnvFilter::new(rust_log))
        .with_current_span(true)
        .init();
}
```

### 2.4 — `common-database`

> [!IMPORTANT]
> **Fix #3 — `begin_tenant_tx` boundary.** Phase 1 has no authentication, no membership verification, and no RLS policies. `begin_tenant_tx` requires a **verified** `TenantContext` to be meaningful. Shipping it now creates a helper with no enforcement backing it.
>
> **Phase 1:** create the pool only.\
> **Phase 2:** implement `begin_tenant_tx` alongside RLS policies and authenticated tenant context.

**Phase 1 responsibilities:**
- Build a `PgPool` from `DatabaseConfig` using the `app_runtime` connection string
- Enforce `max_connections` and `acquire_timeout`
- Provide a `ping` function used by the readiness check

```rust
pub async fn create_pool(config: &DatabaseConfig) -> Result<PgPool, AppError> {
    PgPoolOptions::new()
        .max_connections(config.max_connections)
        .acquire_timeout(Duration::from_secs(config.acquire_timeout_secs))
        .connect(&config.url)  // app_runtime URL
        .await
        .map_err(|e| AppError::Internal(format!("DB pool creation failed: {e}")))
}

pub async fn ping(pool: &PgPool) -> Result<(), AppError> {
    sqlx::query("SELECT 1")
        .execute(pool)
        .await
        .map(|_| ())
        .map_err(|_| AppError::ExternalService("database".into()))
}
```

**Deferred to Phase 2:**
```rust
// NOT in Phase 1 — added in Phase 2 alongside RLS and TenantContext
pub async fn begin_tenant_tx(pool: &PgPool, tenant_id: Uuid) -> Result<Transaction<...>, AppError>
```

### 2.5 — Stub crates

Create a minimal `Cargo.toml` and `src/lib.rs` with a dated `// TODO: Phase N` comment for:
- `common-auth` → Phase 2
- `common-storage` → Phase 3
- `common-messaging` → Phase 4

---

## Task 3 — PostgreSQL Foundation

> [!IMPORTANT]
> **Fix #2 — Phase-aligned schema.** Phase 1 creates only the database roles and extensions. Domain tables are added in the phase where they are actually used. This avoids early schema commitments and keeps migrations coherent with feature delivery.

### Schema migration schedule

| Migration | Phase | Tables / Objects |
|---|---|---|
| `001_foundation.sql` | **Phase 1** | Extensions, GUC namespace, DB roles |
| `002_identity.sql` | Phase 2 | `users`, `tenants`, `memberships` + RLS |
| `003_documents.sql` | Phase 3 | `documents` + RLS |
| `004_ingestion.sql` | Phase 4 | `ingestion_jobs`, `outbox_events` + RLS |
| `005_search.sql` | Phase 5 | `document_chunks`, pgvector, FTS indexes |
| `006_platform.sql` | Phase 7+ | `api_keys`, `usage_records`, `audit_logs` + RLS |

### 3.1 — Migration 001: `foundation`

```sql
-- Migration: 001_foundation.sql
-- Purpose: Extensions, database roles, and GUC namespace only.
--          No domain tables. These are added per phase.

-- =============================================
-- Extensions
-- =============================================
CREATE EXTENSION IF NOT EXISTS "uuid-ossp";
CREATE EXTENSION IF NOT EXISTS "citext";
-- NOTE: pgvector extension added in Phase 5 migration (requires pgvector:pg16-v0.8.0 image)

-- =============================================
-- Database Roles (§7.5 of design doc)
--
-- db_owner  : runs migrations, owns schema objects.
--             Used ONLY by sqlx-cli/migration tooling.
--             Never used by the running application.
--
-- app_runtime: application runtime role.
--             Non-superuser. No schema ownership.
--             No BYPASSRLS. No CREATEROLE.
--             Used by DATABASE_URL in .env.
-- =============================================
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'db_owner') THEN
        CREATE ROLE db_owner LOGIN PASSWORD 'owner_password' CREATEDB;
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'app_runtime') THEN
        CREATE ROLE app_runtime LOGIN PASSWORD 'runtime_password';
    END IF;
END
$$;

-- app_runtime must NOT be a superuser, table owner, or have BYPASSRLS.
-- Grants for specific tables are added in the migration that creates them.

-- =============================================
-- Custom GUC namespace (§7.3 of design doc)
-- Declares the custom parameter family so PostgreSQL
-- accepts SET/SHOW for app.current_tenant_id.
-- The actual setting is used in Phase 2 with RLS.
-- =============================================
-- Custom GUC settings in PostgreSQL are dynamic;
-- no DDL is needed. Document here for reference:
--   SET LOCAL app.current_tenant_id = '<uuid>';  (transaction-scoped)
--   SHOW app.current_tenant_id;

-- =============================================
-- Verification (run manually after migration)
-- =============================================
-- SELECT rolname, rolsuper, rolinherit, rolbypassrls
-- FROM pg_roles WHERE rolname IN ('db_owner', 'app_runtime');
```

### 3.2 — Migration tooling

```bash
# Install sqlx-cli (migration owner account)
cargo install sqlx-cli --no-default-features --features postgres

# Create migration file
sqlx migrate add --source apps/tenant-saas/migrations foundation

# Run migrations using the db_owner account
DATABASE_URL=$DATABASE_MIGRATION_URL sqlx migrate run \
  --source apps/tenant-saas/migrations
```

> [!WARNING]
> Always run migrations with `DATABASE_MIGRATION_URL` (db_owner), never with `DATABASE_URL` (app_runtime). The runtime role does not have DDL privileges.

### 3.3 — Verify runtime role privileges

After running the migration, confirm `app_runtime` cannot perform DDL:

```sql
-- Connect as app_runtime and attempt DDL — must fail
SET ROLE app_runtime;
CREATE TABLE test_forbidden (); -- should produce: ERROR: permission denied
DROP TABLE IF EXISTS test_forbidden;  -- should produce: ERROR: permission denied
```

This check belongs in the Phase 1 exit criteria.

---

## Task 4 — Axum Application Skeleton

### 4.1 — `app_state.rs`

```rust
/// Shared state handed to every request handler.
#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    pub config: Arc<AppConfig>,
}
```

`config` is not read by any handler until Phase 2 (JWT settings), so it carries a narrow `#[allow(dead_code)]` with a comment until then.

### 4.2 — `main.rs`

> [!IMPORTANT]
> **Fix #5 — Migration environment separation.** In development, `sqlx::migrate!()` runs at API startup for convenience. In production, a separate migration job runs before API deployment — never raced across replicas (§20.3 of design doc).
>
> This distinction is enforced at startup via the `APP_ENV` config value, not silently: the API logs which path it took.

> [!IMPORTANT]
> **In-process migrations use `db_owner`, not `app_runtime`.** The runtime role has no DDL rights, so running migrations on its pool would fail. The development path opens a short-lived pool from `DATABASE_MIGRATION_URL`, runs the migrations, and closes it. This happens *before* the runtime pool is created, so it also works against a brand-new database.

```rust
mod app_state;
mod handlers;
mod routes;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Loads .env (if present) and validates required values; fails fast otherwise.
    let config = AppConfig::load()?;
    common_telemetry::init_tracing(&config.app.rust_log);

    // Migrations run in-process only in development. In production a separate
    // job runs them first, so replicas never race on the schema.
    if config.app.env == "development" {
        run_migrations(&config).await?;
    } else {
        tracing::info!("Skipping in-process migrations; run the migration job separately");
    }

    let db = common_database::create_pool(&config.database).await?;

    let addr = format!("{}:{}", config.app.host, config.app.port);
    let state = AppState {
        db,
        config: Arc::new(config),
    };

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(address = %addr, "API listening");
    axum::serve(listener, routes::build_router(state)).await?;

    Ok(())
}

/// Migrations need DDL rights, so they use the `db_owner` account, never `app_runtime`.
async fn run_migrations(config: &AppConfig) -> anyhow::Result<()> {
    let url = config
        .database
        .migration_url
        .as_deref()
        .context("DATABASE_MIGRATION_URL is required to run migrations in development")?;

    tracing::info!("Running migrations (development mode)");
    let pool = PgPool::connect(url).await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    pool.close().await;

    Ok(())
}
```

> [!NOTE]
> `APP_ENV=development` is the only env where in-process migration runs. For production (`APP_ENV=production`), you must run migrations via a separate job (`sqlx migrate run`) before deploying API replicas.
>
> The server binds `APP_HOST:APP_PORT` from config. Graceful shutdown is deferred to Phase 7 (§16.5 of design doc).

### 4.3 — `routes.rs`

```rust
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

// `/api/v1` routes are added in Phase 2.
pub fn build_router(state: AppState) -> Router {
    let middleware = ServiceBuilder::new()
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
        .layer(
            TraceLayer::new_for_http().make_span_with(|req: &Request<_>| {
                let request_id = req
                    .headers()
                    .get("x-request-id")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("unknown");
                tracing::info_span!(
                    "http_request",
                    method = %req.method(),
                    uri = %req.uri(),
                    request_id,
                )
            }),
        )
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(TimeoutLayer::new(REQUEST_TIMEOUT));

    Router::new()
        .route("/health/live", get(handlers::health::liveness))
        .route("/health/ready", get(handlers::health::readiness))
        .layer(middleware)
        .with_state(state)
}
```

The request ID is set first so the trace span can read it, and propagated back on the response.

**Tests** (in `routes.rs`, no running PostgreSQL needed): build the router with a lazy `PgPool` pointing at an unreachable address, then use `tower::ServiceExt::oneshot` (dev-dependency `tower` with the `util` feature) to assert `/health/live` → `200` and `/health/ready` → `503`.

---

## Task 5 — Health Endpoints

Implements §20.4 of the design doc.

### 5.1 — `/health/live`

Returns `200` immediately if the process is alive. **No dependency checks.** Must work even when PostgreSQL is stopped — that is an orchestrator restart signal, not a readiness signal.

```rust
pub async fn liveness() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}
```

### 5.2 — `/health/ready`

Checks only PostgreSQL (the only dependency needed for the API to serve traffic in Phase 1).

```rust
pub async fn readiness(State(state): State<AppState>) -> (StatusCode, Json<Value>) {
    match common_database::ping(&state.db).await {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({ "status": "ready", "db": "ok" })),
        ),
        Err(_) => {
            tracing::warn!("Readiness check failed: database unavailable");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "status": "not_ready", "db": "unavailable" })),
            )
        }
    }
}
```

**Readiness test — stop PostgreSQL and verify 503:**
```powershell
# While API is running:
docker compose stop postgres
curl http://localhost:8080/health/live   # must return 200
curl http://localhost:8080/health/ready  # must return 503
docker compose start postgres
curl http://localhost:8080/health/ready  # must return 200 again
```

This test is a Phase 1 exit criterion.

---

## Task 6 — Docker Compose (Fully Automated)

> [!IMPORTANT]
> **Fix #4 — MinIO bucket initialization is now automatic.** A `minio-init` service creates the private bucket on first `docker compose up`. No `mc` tool is needed on the developer's machine. `docker compose up -d` is the complete setup command.

### 6.1 — `infra/docker-compose.yml`

```yaml
version: "3.9"

services:
  postgres:
    image: pgvector/pgvector:pg16-v0.8.0  # Pinned — required for Phase 5 iterative scans (§9.2)
    environment:
      POSTGRES_DB: tenant_saas
      POSTGRES_USER: db_owner             # Migration owner — not the app runtime account
      POSTGRES_PASSWORD: owner_password
    ports:
      - "5432:5432"
    volumes:
      - postgres_data:/var/lib/postgresql/data
      - ./postgres/init:/docker-entrypoint-initdb.d:ro
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U db_owner -d tenant_saas"]
      interval: 5s
      timeout: 5s
      retries: 10

  redis:
    image: redis:7-alpine
    ports:
      - "6379:6379"
    command: redis-server --appendonly yes
    volumes:
      - redis_data:/data
    healthcheck:
      test: ["CMD", "redis-cli", "ping"]
      interval: 5s
      timeout: 3s
      retries: 5

  minio:
    image: minio/minio:RELEASE.2024-11-07T00-52-20Z  # pin a specific release
    command: server /data --console-address ":9001"
    environment:
      MINIO_ROOT_USER: minioadmin
      MINIO_ROOT_PASSWORD: minioadmin
    ports:
      - "9000:9000"
      - "9001:9001"
    volumes:
      - minio_data:/data
    healthcheck:
      test: ["CMD", "mc", "ready", "local"]
      interval: 10s
      timeout: 5s
      retries: 5

  # Automatic bucket initialization — no mc install needed on developer machine
  minio-init:
    image: minio/mc:latest
    depends_on:
      minio:
        condition: service_healthy
    entrypoint: >
      /bin/sh -c "
        mc alias set local http://minio:9000 minioadmin minioadmin &&
        mc mb --ignore-existing local/tenant-documents &&
        mc anonymous set none local/tenant-documents &&
        echo 'MinIO bucket initialized: tenant-documents (private)'
      "
    restart: "no"   # Run once, do not restart

volumes:
  postgres_data:
  redis_data:
  minio_data:
```

### 6.2 — `infra/postgres/init/00_roles.sql`

Runs automatically before the first container start (via `docker-entrypoint-initdb.d`). Creates `app_runtime` before migrations run.

```sql
-- Creates app_runtime before migration 001 runs.
-- POSTGRES_USER (db_owner) owns the schema; app_runtime is the runtime account.
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'app_runtime') THEN
        CREATE ROLE app_runtime LOGIN PASSWORD 'runtime_password';
    END IF;
END
$$;

-- app_runtime is NOT a superuser, schema owner, or BYPASSRLS holder.
-- Table-level GRANT statements are added in each migration that creates a table.
```

> [!WARNING]
> `POSTGRES_USER=db_owner` in the Compose file must exactly match the role name in migration SQL and `DATABASE_MIGRATION_URL`. `POSTGRES_PASSWORD=owner_password` must match `.env.example`. Keeping these consistent eliminates the credential mismatch that caused the original issue.

### 6.3 — Credential consistency map

| Location | Username | Password | Purpose |
|---|---|---|---|
| `docker-compose.yml` → `POSTGRES_USER` | `db_owner` | `owner_password` | Compose DB superuser |
| `postgres/init/00_roles.sql` | `app_runtime` | `runtime_password` | App runtime role |
| `.env.example` → `DATABASE_URL` | `app_runtime` | `runtime_password` | Application connection |
| `.env.example` → `DATABASE_MIGRATION_URL` | `db_owner` | `owner_password` | sqlx-cli only |
| `migration 001` → `CREATE ROLE` | both | as above | Schema creation |

---

## Task 7 — CI Pipeline & Dockerfile

### 7.1 — `.github/workflows/ci.yml`

```yaml
name: CI

on:
  push:
    branches: [main, develop]
  pull_request:
    branches: [main]

env:
  CARGO_TERM_COLOR: always
  SQLX_OFFLINE: "true"

jobs:
  check:
    name: Format & Lint
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt, clippy
      - uses: Swatinem/rust-cache@v2
      - run: cargo fmt --all --check
      - run: cargo clippy --all-targets --all-features -- -D warnings

  test:
    name: Tests
    runs-on: ubuntu-latest
    services:
      postgres:
        image: pgvector/pgvector:pg16-v0.8.0
        env:
          POSTGRES_DB: tenant_saas_test
          POSTGRES_USER: db_owner
          POSTGRES_PASSWORD: owner_password
        ports: ["5432:5432"]
        options: >-
          --health-cmd "pg_isready -U db_owner -d tenant_saas_test"
          --health-interval 5s --health-timeout 5s --health-retries 10
      redis:
        image: redis:7-alpine
        ports: ["6379:6379"]
        options: >-
          --health-cmd "redis-cli ping"
          --health-interval 5s --health-timeout 3s --health-retries 5
    env:
      # Migration account for sqlx-cli step
      DATABASE_URL: postgres://db_owner:owner_password@localhost:5432/tenant_saas_test
      # Runtime account for application code
      APP_DATABASE_URL: postgres://app_runtime:runtime_password@localhost:5432/tenant_saas_test
      REDIS_URL: redis://localhost:6379
      APP_ENV: test
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - name: Install sqlx-cli
        run: cargo install sqlx-cli --no-default-features --features postgres --locked
      - name: Create app_runtime role
        run: |
          psql $DATABASE_URL -c "
            DO \$\$ BEGIN
              IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'app_runtime') THEN
                CREATE ROLE app_runtime LOGIN PASSWORD 'runtime_password';
              END IF;
            END \$\$;"
      - name: Run migrations
        run: sqlx migrate run --source apps/tenant-saas/migrations
      - name: Verify app_runtime cannot perform DDL
        run: |
          psql postgres://app_runtime:runtime_password@localhost:5432/tenant_saas_test \
            -c "CREATE TABLE forbidden_test ();" 2>&1 | grep -q "permission denied"
          echo "Runtime role DDL restriction: PASS"
      - name: Run tests
        run: cargo test --all
        env:
          DATABASE_URL: postgres://app_runtime:runtime_password@localhost:5432/tenant_saas_test

  build:
    name: Build Release Binary
    runs-on: ubuntu-latest
    needs: [check, test]
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - run: cargo build --release --bin tenant-saas-api
        env:
          SQLX_OFFLINE: "true"
```

### 7.2 — `apps/tenant-saas/Dockerfile`

*(Complete if time permits — `cargo-chef` is an optimization, not a correctness requirement)*

```dockerfile
# ── Stage 1: Planner ──────────────────────────────────────────────────────
FROM rust:1-slim-bookworm AS chef
RUN cargo install cargo-chef --locked
WORKDIR /app

# ── Stage 2: Recipe ────────────────────────────────────────────────────────
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ── Stage 3: Builder ───────────────────────────────────────────────────────
FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
# Cache dependency compilation (invalidated only when dependencies change)
RUN cargo chef cook --release --recipe-path recipe.json
# Build application code
COPY . .
ENV SQLX_OFFLINE=true
RUN cargo build --release --bin tenant-saas-api

# ── Stage 4: Runtime ───────────────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /app/target/release/tenant-saas-api .
RUN useradd -r -s /bin/false appuser
USER appuser
EXPOSE 8080
ENTRYPOINT ["./tenant-saas-api"]
```

---

## Task 8 — Documentation & Final Verification

### 8.1 — `README.md` (local setup)

```markdown
## Prerequisites

- Rust stable (see `rust-toolchain.toml`)
- Docker Desktop with Compose V2

## Quick start

git clone https://github.com/you/portfolio-engine.git
cd portfolio-engine
cp .env.example .env

# Start all infrastructure (PostgreSQL, Redis, MinIO with auto-created bucket)
docker compose -f infra/docker-compose.yml up -d

# Install sqlx-cli
cargo install sqlx-cli --no-default-features --features postgres

# Run database migrations (uses DATABASE_MIGRATION_URL from .env)
DATABASE_URL=$DATABASE_MIGRATION_URL \
  sqlx migrate run --source apps/tenant-saas/migrations

# Start the API (uses DATABASE_URL — app_runtime account)
cargo run --bin tenant-saas-api

## Verify

curl http://localhost:8080/health/live   # → {"status":"ok"}
curl http://localhost:8080/health/ready  # → {"status":"ready","db":"ok"}

## Clean restart

docker compose -f infra/docker-compose.yml down -v
docker compose -f infra/docker-compose.yml up -d
# Then re-run migrations
```

### 8.2 — ADRs *(complete if time permits)*

Create stub files for:
- `docs/adr/ADR-001-modular-monolith.md`
- `docs/adr/ADR-002-rust-axum-tokio.md`
- `docs/adr/ADR-003-shared-postgres-schema.md`

Each ADR should follow the format defined in §22 of the design doc: Context, Decision, Alternatives, Consequences, Revisit conditions.

### 8.3 — Phase 1 clean-start verification

Run this sequence before declaring Phase 1 complete:

```powershell
# 1. Full clean slate
docker compose -f infra/docker-compose.yml down -v

# 2. Bring up infrastructure
docker compose -f infra/docker-compose.yml up -d

# 3. Run migrations with owner account
DATABASE_URL=postgres://db_owner:owner_password@localhost:5432/tenant_saas `
  sqlx migrate run --source apps/tenant-saas/migrations

# 4. Start API
cargo run --bin tenant-saas-api

# 5. Health checks
curl http://localhost:8080/health/live   # 200 {"status":"ok"}
curl http://localhost:8080/health/ready  # 200 {"status":"ready","db":"ok"}

# 6. Stop DB — verify correct behavior
docker compose -f infra/docker-compose.yml stop postgres
curl http://localhost:8080/health/live   # still 200
curl http://localhost:8080/health/ready  # 503 {"status":"not_ready"}

# 7. Restart DB
docker compose -f infra/docker-compose.yml start postgres
curl http://localhost:8080/health/ready  # 200 again

# 8. CI
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all
```

---

## Phase 1 Deliverable Checklist

### Must complete
- [x] `Cargo.toml` workspace with `resolver = "2"`
- [x] `rust-toolchain.toml` pinned to stable
- [x] All 7 crates compiling (`cargo build --workspace`)
- [x] `.env.example` with **consistent** `DATABASE_URL` (app_runtime) and `DATABASE_MIGRATION_URL` (db_owner)
- [x] `common-errors`: `AppError` with safe `IntoResponse`; no SQL details to clients
- [x] `common-config`: typed config, fails fast on missing required values
- [x] `common-telemetry`: JSON structured logging
- [x] `common-database`: pool creation using `app_runtime` URL; `ping` function; **no `begin_tenant_tx`**
- [x] `common-auth`, `common-storage`, `common-messaging`: compiling stubs
- [x] Migration `001_foundation.sql`: extensions + two roles only; no domain tables
- [x] `app_runtime` role created before migrations run
- [x] `app_runtime` verified to lack DDL privileges
- [x] `AppState` with `PgPool` and `Arc<AppConfig>`
- [x] `main.rs`: migrations gated to `APP_ENV=development` only
- [x] Request ID middleware: UUID v4 per request, in headers and log spans
- [x] TraceLayer: logs method, path, status, duration
- [x] Timeout layer (30s)
- [x] `GET /health/live` → `200` even when PostgreSQL is stopped
- [x] `GET /health/ready` → `200` (DB up) / `503` (DB stopped)
- [x] Docker Compose: Postgres (`pgvector:pg16-v0.8.0`), Redis, MinIO — all healthy
- [x] `minio-init` service creates `tenant-documents` bucket automatically (private)
- [x] CI: format, Clippy, migration step, DDL privilege check, unit tests, release build
- [x] README: reproducible from documented steps only; no undocumented manual steps

### Complete if time permits
- [x] Multi-stage Dockerfile with `cargo-chef`
- [x] `SQLX_OFFLINE=true` cache generated and committed
- [x] ADR-001, ADR-002, ADR-003 stubs in `docs/adr/`

---

## Phase 1 → Phase 2 Handoff

| Concern | Phase 1 State | Phase 2 Starting Point |
|---|---|---|
| DB schema | Extensions + two roles only | Add `users`, `tenants`, `memberships` with RLS |
| Auth | None | JWT + Argon2id login, claims, `AuthenticatedPrincipal` |
| Tenant context | No concept yet | Implement `TenantContext` after membership verification |
| `begin_tenant_tx` | Deliberately absent | Implement alongside RLS policies and verified tenant context |
| `common-auth` | Stub | Token verification, principal types, permission structs |
| API routes | `/health/*` only | Add `/auth/*`, `/tenants/*`, `/tenants/{id}/members/*` |
| Security tests | None | Cross-tenant isolation tests (§19.3 of design doc) |
