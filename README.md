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
│   ├── common-storage/     # Object storage abstraction (S3/MinIO, local FS, in-memory)
│   └── common-messaging/   # Ingestion event schema and Redis Streams client
├── apps/
│   └── tenant-saas/        # Axum API + ingestion worker binaries (migrations live in `migrations/`)
├── tests/                  # Integration and security test suites
├── infra/                  # Docker Compose and Postgres init scripts
└── docs/
    ├── adr/                # Architecture Decision Records (ADR-001 – ADR-007)
    ├── api/                # HTTP API reference (documents.md)
    ├── architecture/       # Design notes (ingestion.md)
    └── runbooks/           # Operational runbooks (orphan-cleanup, ingestion-recovery)
```

## Quick Start

```sh
cp .env.example .env

# Start PostgreSQL, Redis and MinIO-compatible storage (bucket is created automatically)
docker compose -f infra/docker-compose.yml up -d --wait

# Start the API. With APP_ENV=development it applies migrations first,
# using DATABASE_MIGRATION_URL (the db_owner account).
cargo run --bin tenant-saas-api

# In a second terminal, start the ingestion worker (it never runs migrations,
# so start the API first on a fresh database).
cargo run --bin tenant-saas-worker
```

In PowerShell, use `Copy-Item .env.example .env` for the first step.

**Existing Postgres volume from before Phase 4?** The pgvector extension is
installed by `infra/postgres/init/00_roles.sql`, which only runs when the data
volume is first created. Install it once by hand (or do a clean restart, below):

```sh
docker compose -f infra/docker-compose.yml exec postgres \
  psql -U postgres -d tenant_saas -c "CREATE EXTENSION IF NOT EXISTS vector;"
```

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

### Upload a Document

After logging in and creating a tenant, upload files via multipart form:

```sh
# Log in and capture the access token
TOKENS=$(curl -s -X POST http://localhost:8080/api/v1/auth/login \
  -H 'Content-Type: application/json' \
  -d '{"email":"alice@example.com","password":"correct-horse-battery"}')
ACCESS_TOKEN=$(echo $TOKENS | grep -o '"access_token":"[^"]*' | cut -d'"' -f4)

TENANT_ID=<your-tenant-uuid>

# Upload a document (multipart/form-data)
curl -X POST http://localhost:8080/api/v1/tenants/$TENANT_ID/documents \
  -H "Authorization: Bearer $ACCESS_TOKEN" \
  -F 'title=Q4 Report' \
  -F 'file=@report.pdf'

# List uploaded documents
curl http://localhost:8080/api/v1/tenants/$TENANT_ID/documents \
  -H "Authorization: Bearer $ACCESS_TOKEN"
```

Re-uploading the same file bytes returns `200 OK` with the existing document record (content-addressed deduplication via SHA-256).

### Watch a Document Become Searchable

With `tenant-saas-worker` running, a new upload moves from `pending` through
`processing` to `ready` on its own:

```sh
DOC_ID=<id from the upload response>

# Poll until status is "ready" (or "failed", with a failure_code)
curl http://localhost:8080/api/v1/tenants/$TENANT_ID/documents/$DOC_ID/status \
  -H "Authorization: Bearer $ACCESS_TOKEN"
# {"id":"...","status":"ready","chunk_count":12,"failure_code":null,...}

# Re-queue a failed document (Owner/Admin: any document, Member: own uploads)
curl -X POST http://localhost:8080/api/v1/tenants/$TENANT_ID/documents/$DOC_ID/retry \
  -H "Authorization: Bearer $ACCESS_TOKEN"
```

Check the stored chunks as `db_owner`:

```sql
SELECT count(*), min(vector_dims(embedding)) AS dims, min(embedding_model) AS model
  FROM document_chunks WHERE document_id = '<doc-id>';
```

The default `EMBEDDING_PROVIDER=fake` produces deterministic vectors offline,
with no API key. They carry **no meaning**, so use `EMBEDDING_PROVIDER=openai`
with `EMBEDDING_API_KEY` for anything that measures search quality.

### Ingestion Worker

`tenant-saas-worker` runs five supervised tasks in one process:

| Task | Job |
|---|---|
| Outbox publisher | Moves `outbox_events` rows to the Redis stream |
| Ingestion consumer | Claims jobs, then parses, chunks, embeds and stores chunks |
| Sweeper | Requeues crashed or due-for-retry jobs, re-announces lost events, cancels jobs of deleted documents |
| Storage reconciler | The Phase 3 orphan cleanup, shallow passes every 10 minutes |
| Heartbeat | One `worker heartbeat` log line every 30 s with backlog fields |

Uploads never depend on the worker or on Redis: they return `202` and wait in
the outbox. Killing the worker at any point loses no work. Its jobs are
picked up again once their lease (`JOB_LEASE_SECS`) expires. Run several
workers to scale out. Stop one with Ctrl+C or SIGTERM, and it gives in-flight
jobs 30 s to finish.

See [`docs/architecture/ingestion.md`](docs/architecture/ingestion.md) for the
design and [`docs/runbooks/ingestion-recovery.md`](docs/runbooks/ingestion-recovery.md)
for troubleshooting.

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

### Reconcile Orphaned Objects

The reconciler sweeps for objects in S3 that have no corresponding database row (e.g., after a failed upload) and cleans up documents stuck in a `deleting` state.

```sh
# Run one reconciliation pass (dry-run: false — will delete confirmed orphans)
./tenant-saas-api reconcile-orphans

# Schedule nightly via cron (append output to a log file)
0 2 * * * /path/to/tenant-saas-api reconcile-orphans >> /var/log/tenant-saas/reconcile.log 2>&1
```

The command exits `0` on success and prints a JSON `ReconcileReport`. See [`docs/runbooks/orphan-cleanup.md`](docs/runbooks/orphan-cleanup.md) for diagnostics and escalation guidance.

## Environment Variables

| Variable | Type | Default | Description |
|---|---|---|---|
| `DATABASE_URL` | URL | — | PostgreSQL connection string for `app_runtime` |
| `DATABASE_MIGRATION_URL` | URL | — | PostgreSQL connection string for `db_owner` (migrations & fixtures) |
| `REDIS_URL` | URL | — | Redis connection string |
| `OBJECT_STORAGE_BACKEND` | string | `memory` | Storage backend: `s3`, `local`, or `memory` |
| `OBJECT_STORAGE_ENDPOINT` | URL | — | S3/MinIO endpoint (required when backend is `s3`) |
| `OBJECT_STORAGE_BUCKET` | string | — | Bucket name (required when backend is `s3` or `local`) |
| `OBJECT_STORAGE_ACCESS_KEY` | string | — | S3 access key |
| `OBJECT_STORAGE_SECRET_KEY` | string | — | S3 secret key |
| `OBJECT_STORAGE_REGION` | string | `us-east-1` | S3 region |
| `MAX_UPLOAD_BYTES` | integer | `20971520` | Maximum upload size in bytes (default 20 MiB) |
| `STORAGE_ORPHAN_RETENTION_SECS` | integer | `86400` | Minimum age (seconds) before an untracked object is deleted by the reconciler |
| `INGEST_STREAM` / `INGEST_GROUP` | string | `ingestion:events` / `ingestion-workers` | Redis stream and consumer group |
| `WORKER_CONCURRENCY` | integer | `4` | Jobs processed in parallel per worker. Keep `DATABASE_MAX_CONNECTIONS ≥ WORKER_CONCURRENCY + 3` |
| `JOB_LEASE_SECS` | integer | `300` | How long a claimed job is owned before others may take it over |
| `JOB_MAX_ATTEMPTS` | integer | `5` | Attempts per job before it is marked `failed` |
| `RETRY_BASE_SECS` / `RETRY_MAX_SECS` | integer | `5` / `600` | Exponential backoff with jitter between attempts |
| `SWEEP_INTERVAL_SECS` | integer | `15` | How often the sweeper runs |
| `OUTBOX_POLL_MS` / `OUTBOX_BATCH` / `OUTBOX_CLAIM_SECS` | integer | `200` / `50` / `30` | Outbox publisher polling, batch size and claim lifetime |
| `PARSE_TIMEOUT_SECS` / `MAX_PDF_PAGES` / `MAX_EXTRACTED_CHARS` | integer | `60` / `500` / `5000000` | Parser limits |
| `CHUNK_TARGET_TOKENS` / `CHUNK_OVERLAP_TOKENS` / `MAX_CHUNKS_PER_DOC` | integer | `500` / `75` / `2000` | Chunking |
| `EMBEDDING_PROVIDER` | string | `fake` | `fake` (offline, no meaning) or `openai` (any OpenAI-compatible endpoint) |
| `EMBEDDING_MODEL` | string | `text-embedding-3-small` | Also selects the tokenizer |
| `EMBEDDING_DIM` | integer | `1536` | Must be 1536 (fixed by the `vector(1536)` column) |
| `EMBEDDING_BATCH` / `EMBEDDING_TIMEOUT_SECS` | integer | `32` / `30` | Inputs per provider request, request timeout |
| `EMBEDDING_API_KEY` | string | — | Required when `EMBEDDING_PROVIDER=openai` |
| `EMBEDDING_BASE_URL` | URL | `https://api.openai.com/v1` | For compatible providers |

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
docker build -f apps/tenant-saas/Dockerfile -t tenant-saas .

docker run --env-file .env tenant-saas                                   # API (default entrypoint)
docker run --env-file .env --entrypoint ./tenant-saas-worker tenant-saas # worker
```

One image ships both binaries.

## Notes

- Object storage uses [`pgsty/silo`](https://github.com/pgsty/silo), a maintained
  MinIO fork, because upstream MinIO images are no longer published. It is
  S3-compatible and uses the same `MINIO_*` variables.
