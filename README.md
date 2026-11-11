# Multi-Tenant AI SaaS Platform (`portfolio-engine`)

A production-oriented multi-tenant AI SaaS platform built in Rust.

## Prerequisites

- **Rust** (version pinned in `rust-toolchain.toml`; install via `rustup`)
- **Docker Desktop** (with Compose V2)
- **sqlx-cli**: `cargo install sqlx-cli --no-default-features --features postgres --locked`

## Workspace Layout

```text
portfolio-engine/
├── Cargo.toml              # Workspace manifest
├── rust-toolchain.toml     # Pinned toolchain channel
├── .env.example            # Environment template with separated DB roles
├── crates/
│   ├── common-config/      # Typed configuration loading
│   ├── common-errors/      # Error hierarchy and client-safe response mapping
│   ├── common-auth/        # Authentication, claims, Argon2id, JWT & TenantContext
│   ├── common-telemetry/   # Structured logging & tracing
│   ├── common-database/    # PostgreSQL connection pool and scoped transaction helpers
│   ├── common-storage/     # Object storage abstraction (Phase 3 stub)
│   └── common-messaging/   # Redis Streams producer/consumer helpers (Phase 4 stub)
├── apps/
│   └── tenant-saas/        # Axum API (migrations live in `migrations/`)
├── tests/                  # Integration and security test suites
├── infra/                  # Docker Compose and Postgres init scripts
└── docs/adr/               # Architecture Decision Records
```

## Quick Start

```sh
cp .env.example .env

# Start PostgreSQL, Redis and MinIO-compatible storage (bucket is created automatically)
docker compose -f infra/docker-compose.yml up -d --wait

# Start the API. With APP_ENV=development it applies migrations first,
# using DATABASE_MIGRATION_URL (the db_owner account).
cargo run --bin tenant-saas-api
```

In PowerShell, use `Copy-Item .env.example .env` for the first step.

To run migrations by hand instead (this is how production does it):

```sh
DATABASE_URL=postgres://db_owner:owner_password@localhost:5432/tenant_saas \
  sqlx migrate run --source apps/tenant-saas/migrations
```

### Verify

```sh
curl http://localhost:8080/health/live   # 200 {"status":"ok"}
curl http://localhost:8080/health/ready  # 200 {"status":"ready","db":"ok"}
```

`/health/ready` returns `503` while PostgreSQL is down; `/health/live` stays `200`.

### Auth & Multi-Tenancy Quick-Start

Run these commands to walk through registration, login, tenant creation, and invitations:

```sh
# 1. Register a user
curl -X POST http://localhost:8080/api/v1/auth/register \
  -H "Content-Type: application/json" \
  -d '{"email": "alice@example.com", "password": "correct-horse-battery"}'

# 2. Login to get an access token
TOKENS=$(curl -s -X POST http://localhost:8080/api/v1/auth/login \
  -H "Content-Type: application/json" \
  -d '{"email": "alice@example.com", "password": "correct-horse-battery"}')
ACCESS_TOKEN=$(echo $TOKENS | grep -o '"access_token":"[^"]*' | cut -d'"' -f4)

# 3. Create a tenant (Alice automatically becomes Owner)
TENANT=$(curl -s -X POST http://localhost:8080/api/v1/tenants \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer $ACCESS_TOKEN" \
  -d '{"name": "Acme Corp", "slug": "acme-corp"}')
TENANT_ID=$(echo $TENANT | grep -o '"id":"[^"]*' | cut -d'"' -f4)

# 4. Register a second user (Bob)
curl -X POST http://localhost:8080/api/v1/auth/register \
  -H "Content-Type: application/json" \
  -d '{"email": "bob@example.com", "password": "correct-horse-battery"}'

# 5. Alice invites Bob as Member
curl -X POST http://localhost:8080/api/v1/tenants/$TENANT_ID/invitations \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer $ACCESS_TOKEN" \
  -d '{"email": "bob@example.com", "role": "member"}'

# 6. Bob logs in and accepts the invitation
BOB_TOKENS=$(curl -s -X POST http://localhost:8080/api/v1/auth/login \
  -H "Content-Type: application/json" \
  -d '{"email": "bob@example.com", "password": "correct-horse-battery"}')
BOB_TOKEN=$(echo $BOB_TOKENS | grep -o '"access_token":"[^"]*' | cut -d'"' -f4)

curl -X POST http://localhost:8080/api/v1/invitations/$TENANT_ID/accept \
  -H "Authorization: Bearer $BOB_TOKEN"

# 7. Bob reads tenant details
curl http://localhost:8080/api/v1/tenants/$TENANT_ID \
  -H "Authorization: Bearer $BOB_TOKEN"
```

### Operational Runbook (Identity & Sessions)

- **Disable a user:**
  ```sql
  -- Run as db_owner
  UPDATE users SET status = 'disabled' WHERE email = 'target@example.com';
  ```
  A disabled user is immediately denied access on their next request across all endpoints (`/auth/me`, tenant resolution, and token refresh).

- **Revoke all sessions for a user:**
  ```sql
  -- Run as db_owner
  UPDATE refresh_tokens SET revoked_at = now() WHERE user_id = '<user-id>' AND revoked_at IS NULL;
  ```

### Clean restart

```sh
docker compose -f infra/docker-compose.yml down -v
docker compose -f infra/docker-compose.yml up -d --wait
```

The API re-applies migrations on its next start in development.

### Port 5432 already in use

If a local PostgreSQL already listens on 5432, publish the container on another
port and update both URLs in `.env`:

```sh
POSTGRES_HOST_PORT=5433 docker compose -f infra/docker-compose.yml up -d --wait
# .env: DATABASE_URL and DATABASE_MIGRATION_URL -> localhost:5433
```

## Checks (same as CI)

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
```

## Docker image

```sh
docker build -f apps/tenant-saas/Dockerfile -t tenant-saas-api .
```

## Notes

- Object storage uses [`pgsty/silo`](https://github.com/pgsty/silo), a maintained
  MinIO fork, because upstream MinIO images are no longer published. It is
  S3-compatible and uses the same `MINIO_*` variables.
