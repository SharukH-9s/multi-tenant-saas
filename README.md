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
│   ├── common-auth/        # Authentication, claims, and principal (Phase 2 stub)
│   ├── common-telemetry/   # Structured logging & tracing
│   ├── common-database/    # PostgreSQL connection pool and health check
│   ├── common-storage/     # Object storage abstraction (Phase 3 stub)
│   └── common-messaging/   # Redis Streams producer/consumer helpers (Phase 4 stub)
├── apps/
│   └── tenant-saas/        # Axum API (migrations live in `migrations/`)
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
