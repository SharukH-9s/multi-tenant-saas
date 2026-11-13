# Phase 3: Document Management Implementation Plan

> Source of truth: `multi_tenant_ai_saas_system_design.md` §2.1, §7.3.1, §7.6, §8.5, §9.1, §10.1 to §10.3, §10.2, §13.1, §13.4, §13.7, §14.5, §16.3, §19.3, §23 (Phase 3).
> Builds on: `phase1_implementation_plan.md` and `phase2_implementation_plan.md` (both complete).
> Budget: Week 3, ~15 to 20 focused hours.

---

## 0. Goal and exit criteria

Deliver a secure streaming upload endpoint, a validated object storage adapter, tenant scoped document metadata with content hash deduplication, read and status endpoints, soft delete with an ownership check, and a working orphan object cleanup path.

Phase 3 deliberately stops before asynchronous processing. No `ingestion_jobs` table, no outbox, no Redis Streams, no worker process, no parsing or chunking. A new document lands in status `pending` and stays there until Phase 4 picks it up.

Exit criteria from the roadmap and how this plan proves each one:

| Exit criterion | Proof |
|---|---|
| Valid supported files upload | `tests/integration/documents.rs`: one test per format (PDF, TXT, MD) asserting 202, a `pending` status, a stored object, and a row readable through `GET /documents/{id}` |
| Invalid or oversized files are rejected | Table driven tests covering 413, 415, 422 for oversize, wrong extension, extension that disagrees with the magic bytes, invalid UTF-8 text, zero bytes, and a missing `file` part. Every rejection also asserts that no object was left in storage |
| Concurrent duplicate uploads resolve deterministically | `tests/integration/documents.rs`: 10 parallel uploads of identical bytes produce exactly one 202 and nine 200 responses, all carrying the same `document_id`, and exactly one row in `documents` |
| Original file remains private | Three independent checks: no route serves document bytes, no response body or log line ever contains `object_key` (asserted by scanning the captured logs and the JSON), and a CI step requests the raw object key over anonymous HTTP against MinIO and asserts 403 |

Two further properties are carried forward from Phase 2 and must not regress:

| Carried property | Proof |
|---|---|
| Tenant A cannot see or touch Tenant B documents | `tests/security/cross_tenant_documents.rs`: HTTP level 404 on read, status and delete, plus direct SQL SELECT, INSERT, UPDATE and DELETE attempts through the `app_runtime` pool |
| Pool reuse leaks no tenant context | The existing `pool_reuse.rs` test gains a `documents` case, so the probe runs against a real tenant owned table instead of only `memberships` |

---

## 1. Baseline after Phase 2 (what exists, do not redo)

Verified in the repository at the start of Phase 3:

| Area | State now | How Phase 3 uses it |
|---|---|---|
| Toolchain | `rust-toolchain.toml` pins **1.99.1**. CI uses `dtolnay/rust-toolchain@1.99.1` in all three jobs. Dockerfile takes `ARG RUST_VERSION=1.99.1` | Every new crate must build on 1.99.1. Do not bump the toolchain in Phase 3 |
| Workspace | `apps/tenant-saas` plus seven `common-*` crates, and a `tests` member named `integration-tests` | `common-storage` stops being a stub. `common-messaging` stays a stub until Phase 4 |
| `common-storage` | Empty crate with a doc comment and a `TODO: Phase 3` marker. `[dependencies]` is empty | This is the main new crate work (Task 3) |
| DB roles | `db_owner` owns the schema. `app_runtime` is `NOSUPERUSER NOBYPASSRLS NOCREATEROLE` and is not a table owner | Reuse. Migration `003` adds grants for `documents` in the same style as `002` |
| Migrations | `001_foundation.sql` (extensions), `002_identity.sql` (identity plus RLS). Embedded with `sqlx::migrate!("./migrations")`, run in process only when `APP_ENV=development` using `DATABASE_MIGRATION_URL`. CI runs `sqlx migrate run` as `db_owner` | **Never edit `001` or `002`.** `003_documents.sql` is purely additive and keeps the three digit naming |
| RLS template | `app_tenant_id()` and `app_user_id()` helpers exist. `memberships` and `tenants` use ENABLE plus FORCE with four explicit policies each, all written as `tenant_id = (SELECT app_tenant_id())` | Copy this template verbatim for `documents` (Task 2) |
| Composite FK pattern | `memberships` has `UNIQUE (tenant_id, id)` so tenant owned children can use a composite FK | `documents` gets `UNIQUE (tenant_id, id)` for the Phase 4 `document_chunks` and `ingestion_jobs` tables |
| Tenant context | `TenantContext` is immutable, built only inside `auth::tenant_resolver`, and a CI grep guard enforces that. `begin_tenant_tx(&pool, &ctx)` and `begin_user_tx(&pool, user_id)` set the two GUCs in one round trip | Upload and read handlers take `TenantCtx(ctx)` and pass `&ctx` straight into `begin_tenant_tx` |
| Permissions | `Permission::{DocumentsUpload, DocumentsSearch, DocumentsDeleteAny, DocumentsDeleteOwn}` already exist and are covered by the full matrix test in `common-auth` | Handlers call `ctx.require(...)`. Phase 3 adds the row level ownership check that `DocumentsDeleteOwn` needs |
| Concealment | Phase 2 decision D7: not a member, unknown tenant or malformed tenant id all return an identical 404. Lacking a permission returns 403. Suspended tenant or membership returns 403 | Document endpoints inherit all of this from `TenantCtx` with no extra code |
| `AppError` | Variants: `NotFound`, `Unauthorized`, `Forbidden`, `BadRequest`, `InvalidCredentials`, `Conflict { code, message }`, `TooManyRequests`, `Validation` (422), `Database` (500), `ExternalService` (503), `Internal`. `code()` returns a fixed `&'static str` per variant. `ApiJson` is the body extractor that produces the standard envelope | Needs two new variants for 413 and 415 (Task 1) |
| Request id | `inject_request_id` rewrites bodies tagged with `AppErrorMarker` and adds `error.request_id`. Known gap already documented: `TimeoutLayer` returns a bare 408 with no envelope | A rejection raised by axum itself (a body limit hit, for example) has no marker, so Phase 3 must convert those into `AppError` rather than letting axum answer directly |
| `common-config` | Hand rolled `load_from_env_getter` with `required` / `text` / `number` helpers, then `validate()`. `StorageConfig { endpoint, bucket, region, access_key, secret_key }` already exists and both keys are **required** | Extend `StorageConfig` in the same style (Task 1). Note that `routes.rs` unit tests and `tests/common/mod.rs` both inject fake storage keys, so any new required key breaks them |
| `AppState` | `AppState::new(db, Arc<AppConfig>)` is the single construction point and builds `token_service`, `password_service` and `rate_limiter`. `with_rate_limiter` exists for tests | Add `storage: Arc<dyn FileStorage>` inside `new`, plus a `with_storage` override for tests. Because `new` is the only constructor, no test needs editing to add the field |
| App layering | `handlers` to `services` to `repositories`, with `domain` free of Axum and SQLx types. `repositories::db_error::map_db_error` maps unique violations by constraint name and the `P0001` plus `LAST_OWNER` trigger | Add `domain/document.rs`, `services/document_service.rs`, `repositories/document_repo.rs`, `handlers/documents.rs` |
| Routes | `/health/*` unversioned, `/api/v1/auth`, `/api/v1/tenants`, `/api/v1/tenants/{tenant_id}/...`, `/api/v1/invitations`. Layer order: SetRequestId, Trace, PropagateRequestId, Timeout 30s, then `inject_request_id` innermost | Documents nest under the existing `/api/v1/tenants/{tenant_id}` subrouter so `TenantCtx` keeps finding its path parameter |
| Request timeout | A global `TimeoutLayer` of 30 seconds | A 20 MiB upload over a slow link can exceed this. Task 6 raises the limit for the upload route only |
| Tests | `integration-tests` crate with `TestApp` (real router, `app_runtime` pool, optional `db_owner` pool, per instance client IP, global log capture), `create_session`, `create_tenant`, `add_member`, `seed_membership`, `update_member`, and `unique()` | Add `upload_file` and `create_document` helpers plus a `documents` cleanup path |
| CI | Jobs `check`, `test`, `build`. Services are Postgres (`pgvector/pgvector:0.8.0-pg16`) and Redis only. Env names are `BOOTSTRAP_URL`, `MIGRATION_URL`, `RUNTIME_URL`. Steps assert role privileges, the DDL denial and the `password_hash` denial | **CI has no object storage service yet.** Task 9 adds MinIO plus bucket creation, and new assertions for `documents` |
| Compose | Postgres, Redis, MinIO (`pgsty/silo`, a maintained fork since the upstream images were pulled), and a `minio-init` job that creates the private `tenant-documents` bucket with `mcli anonymous set none` | Reuse as is. The private bucket assertion in Task 9 depends on `anonymous set none` already being there |
| ADRs | `docs/adr/ADR-001` through `ADR-004` exist | Add `ADR-005` and `ADR-006`. The repository sequence is its own, so it no longer matches the numbering in design doc §22. Record that mismatch once in `ADR-005` |

### Design doc gaps this plan has to close

Three places in the design doc cannot be implemented as written. Each gets an explicit decision below.

1. §10.2.1 proposes `async fn upload(&self, key: &str, data: Bytes)`. That signature forces the whole file into memory, which contradicts §10.1 and §14.5. See D4.
2. §10.1 shows the duplicate check happening before the object is stored, but the content hash is only known once the last byte has been read. See D5.
3. §16.3 says PostgreSQL being unavailable should return a controlled 503, while `AppError::Database` currently maps to 500. See D12.

---

## 2. Key design decisions

### D1: Upload transport is multipart form data

`POST /api/v1/tenants/{tenant_id}/documents` accepts `multipart/form-data` with:

| Part | Required | Handling |
|---|---|---|
| `file` | yes, exactly one | Streamed. Filename is read from the part headers |
| `title` | no | Read into memory, trimmed, 1 to 200 characters |

Any other part name is ignored. A second `file` part is a 422 and the first object is removed. Fields may arrive in any order, because the database row is only written after the whole body has been consumed.

Reasons for multipart over a raw body: real clients and HTML forms send it, the filename arrives in band so no extra header is needed, and axum's `Multipart` extractor hands over chunks rather than a buffered body. The cost is one new axum feature flag and the rule that each part must be drained before the next one is requested.

Rejected alternative: a raw `application/octet-stream` body with `X-Filename`. Fewer moving parts, but it pushes filename encoding onto a custom header and makes browser uploads awkward.

### D2: Two body limits, because Content-Length cannot be trusted

- A route local `DefaultBodyLimit::max(MAX_UPLOAD_BYTES + 1 MiB)` on the upload route only. The 1 MiB of slack covers multipart boundaries and the `title` part. This is a cheap early rejection based on the declared length.
- A running byte counter inside the streaming loop that aborts at exactly `MAX_UPLOAD_BYTES`. A client can understate Content-Length or use chunked encoding, so the counter is the real limit.

When axum's own limit fires it produces its own 413 with no `AppErrorMarker`, so the body would not carry a request id. The handler therefore uses `Multipart` through a wrapper that converts `MultipartError` into `AppError`, and the counter path raises `AppError::PayloadTooLarge` directly. Both responses then look identical to the client.

### D3: Object keys are generated server side and never leave the process

Key layout:

```text
tenants/{tenant_id}/documents/{upload_id}
```

- `upload_id` is a fresh v4 UUID, not the document id. The row is inserted after the upload, and a deduplicated upload reuses the existing row rather than creating one, so the document id is not available at key generation time.
- No filename and no extension in the key. Filenames are attacker controlled and belong in a column, not a path.
- The key is random, so a listing leak does not make other keys guessable.
- The tenant prefix is an organizational convention for reconciliation and lifecycle rules, not an authorization boundary. Design §5.3 already says this, and `ADR-005` repeats it.

`object_key` is never serialized. The repository row type keeps it, every response DTO leaves it out, and a test asserts it never appears in any response body or captured log line.

### D4: `FileStorage` is a writer handle trait, not a `Bytes` trait

This is the deliberate departure from §10.2.1. The trait exposes an incremental writer so the handler can hash, count and validate while bytes flow through, and so no file is ever fully resident in memory.

```rust
#[async_trait]
pub trait FileStorage: Send + Sync {
    /// Opens a writer for `key`. Nothing is visible at `key` until `finish` returns Ok.
    async fn begin_write(&self, key: &str) -> Result<Box<dyn ObjectWriter>, StorageError>;

    /// Small writes in one call. Used by tests and by future small payloads.
    async fn put(&self, key: &str, bytes: Bytes) -> Result<(), StorageError>;

    /// Whole object read. The worker uses this in Phase 4; bounded by MAX_UPLOAD_BYTES.
    async fn get(&self, key: &str) -> Result<Bytes, StorageError>;

    async fn delete(&self, key: &str) -> Result<(), StorageError>;

    /// Keys under `prefix` with size and last modified time. Used by the reconciler.
    async fn list(&self, prefix: &str) -> Result<Vec<StoredObject>, StorageError>;

    /// Cheap reachability probe for /health/dependencies.
    async fn health(&self) -> Result<(), StorageError>;
}

#[async_trait]
pub trait ObjectWriter: Send {
    async fn write(&mut self, chunk: Bytes) -> Result<(), StorageError>;
    /// Commits the object. Must be called exactly once.
    async fn finish(&mut self) -> Result<(), StorageError>;
    /// Discards everything written so far. Safe to call after a failed write.
    async fn abort(&mut self) -> Result<(), StorageError>;
}
```

`finish` and `abort` take `&mut self` rather than `Box<Self>` because that mirrors `object_store::MultipartUpload` exactly and avoids any question about async trait object self types. The implementation keeps a `done: bool` and turns a second call into an error.

### D5: One `object_store` backed adapter covers all three environments

`object_store` 0.14.2 already abstracts local filesystem, S3 and in memory behind its own `ObjectStore` trait. Writing three adapters would duplicate that. Instead there is a single adapter over `Arc<dyn ObjectStore>` and a factory that picks the concrete store from config:

| `OBJECT_STORAGE_BACKEND` | Store | Use |
|---|---|---|
| `s3` (default) | `AmazonS3Builder` | MinIO locally and in CI, real S3 or R2 in production |
| `local` | `LocalFileSystem::new_with_prefix` | Offline development with no Docker |
| `memory` | `InMemory` | Unit tests in `common-storage`, and any app test that does not care about durability |

Facts about the pinned version that the implementation must respect:

- `object_store` default features enable `fs` only. The `aws` feature is **not** default and must be requested explicitly. It pulls in `reqwest` with rustls.
- Since 0.13.0 the convenience methods (`put`, `get`, `delete`, `head`, `put_multipart`) live on `ObjectStoreExt`, which has a blanket impl for every `ObjectStore`. The `ObjectStore` trait itself only has the `*_opts` forms. So the adapter must `use object_store::ObjectStoreExt;` or the calls will not resolve.
- `MultipartUpload::put_part(&mut self, data: PutPayload) -> UploadPart`. `complete` and `abort` both take `&mut self` and return boxed futures.
- Most stores require every part except the last to be at least 5 MiB. The adapter therefore buffers into fixed size parts.
- S3 cannot clean up an abandoned multipart upload on drop, so `abort` must be called explicitly on every failure path. A bucket lifecycle rule is the backstop, and that goes in the runbook rather than in code.

MinIO builder settings: `with_endpoint`, `with_bucket_name`, `with_region`, `with_access_key_id`, `with_secret_access_key`, `with_allow_http(true)` for the plain HTTP local endpoint, and path style requests, which are the default since `with_virtual_hosted_style_request` defaults to false.

### D6: Part buffering, so small files cost one request

`ObjectStoreWriter` holds a `BytesMut` buffer and starts with no multipart session at all:

1. `write` appends to the buffer.
2. When the buffer reaches `OBJECT_STORAGE_PART_SIZE_BYTES` (default 8 MiB, validated at startup to be at least 5 MiB), the writer lazily opens a multipart upload on first flush and sends one part.
3. `finish` has two paths. If no multipart session was ever opened, the whole buffer goes out as a single `put`, which avoids the 5 MiB floor and a wasted session. Otherwise the remaining buffer is sent as the final part and `complete` is called.
4. `abort` either drops the buffer, if nothing was opened, or calls `MultipartUpload::abort`.

A 4 KiB text file is therefore one PUT. A 20 MiB PDF is three parts plus a complete. The in memory store makes both paths cheap to test.

### D7: Hash first, then let the unique index arbitrate

The content hash is only known after the last byte, so the §10.1 ordering cannot be followed literally. The real sequence is:

```text
authorize (TenantCtx + DocumentsUpload)
        |
        v
open ObjectWriter at tenants/{tenant}/documents/{upload_id}
        |
        v
for each chunk: validate, hash (SHA-256), count bytes, write
        |
        v
finish the object            <- no DB connection has been touched yet
        |
        v
begin_tenant_tx
        |
        +--> INSERT ... ON CONFLICT (tenant_id, content_hash)
        |        WHERE deleted_at IS NULL DO NOTHING RETURNING *
        |
        +--> if no row came back: SELECT the existing document
        |
        v
COMMIT                       <- transaction stays open for two statements
        |
        v
if it was a duplicate: delete the object we just wrote, return 200
otherwise: return 202
```

`ON CONFLICT DO NOTHING RETURNING` plus a fallback `SELECT` is race free by construction. Two concurrent requests with the same bytes both reach the insert, exactly one wins, and the loser reads the winner's row. The pre check that §10.3 calls useful is folded into the same statement, so there is no extra round trip and no window between checking and inserting. The unique index is the only arbiter, which is exactly what §10.3 asks for.

### D8: A duplicate upload returns 200 with the existing document

Design §10.3 leaves the choice open and asks for it to be documented. The decision:

| Outcome | Status | Body |
|---|---|---|
| New content stored | **202 Accepted** | The new document, status `pending` |
| Content already in this tenant | **200 OK** | The existing document, whatever its current status |

The status code alone tells the client which happened, the response shape is identical either way, and a client retrying after a dropped connection gets a usable answer instead of an error. It also keeps the existing `AppError::Conflict` shape untouched, which cannot carry a resource id.

The cost is that a client who genuinely wants to know about the collision has to read the status code rather than catch an error. That is recorded in `ADR-006` along with the rejected option, which was a 409 carrying `DOCUMENT_DUPLICATE` and no id.

A document that was soft deleted does not block a re uploaded copy, because the unique index is partial on `deleted_at IS NULL`.

### D9: Validation order, cheapest check first

| Order | Check | Failure |
|---|---|---|
| 1 | Content type of the request is `multipart/form-data` | 415 |
| 2 | Declared body length within `MAX_UPLOAD_BYTES + 1 MiB` | 413 |
| 3 | `file` part present, exactly one | 422 |
| 4 | Filename sanitizes to a non empty name, at most 255 characters, no path separators, no control characters, not `.` or `..` | 422 |
| 5 | Extension is `pdf`, `txt`, `md` or `markdown` | 415 |
| 6 | First bytes agree with the extension | 415 |
| 7 | Running byte count stays at or under `MAX_UPLOAD_BYTES` | 413 |
| 8 | For text formats, bytes stay valid UTF-8 and contain no NUL | 415 |
| 9 | Total size is greater than zero | 422 |

Two details that are easy to get wrong and both get their own tests:

- **Sniffing needs the first bytes but the body is a stream.** The validator holds incoming chunks until at least 8 bytes have arrived or the stream ends, sniffs once, then releases them. A PDF must start with `%PDF-`. Text formats have no signature, so their check is the UTF-8 and NUL rule instead.
- **A chunk can split a multi byte character.** Incremental UTF-8 validation keeps a carry buffer of at most 3 bytes. `std::str::from_utf8` returning an error whose `error_len()` is `None` means the tail is incomplete rather than invalid, so those bytes move to the carry and are prepended to the next chunk. At end of stream a non empty carry is invalid UTF-8.

No `infer` dependency. Three formats need one signature and one encoding rule, and hand rolled code here is auditable and matches how `common-config` was written. When Phase 4 widens the format list, `infer` 0.22 is the drop in replacement, and that is noted in `ADR-006`.

`mime_type` is stored as the type the server concluded, never the one the client declared. The client's `Content-Type` is read for logging only.

Malware scanning from §10.7 is out of scope. The validator exposes one place where a scan hook would go, and the comment says so.

### D10: Micro scoped transactions and compensating cleanup

§7.3.1 forbids holding a connection across external I/O. In this phase that means:

- The object is fully written before `begin_tenant_tx` is called. The transaction then runs two statements and commits.
- The compensating delete for a duplicate happens after the commit, never inside the transaction.
- If the transaction itself fails, the object is deleted immediately. If that delete also fails, a row goes into `storage_orphans` and the reconciler takes over.

```text
object written, DB transaction fails
        |
        +--> storage.delete(key)  -> Ok: nothing leaked, return the DB error
        |
        +--> storage.delete(key)  -> Err: INSERT INTO storage_orphans, log at target="audit",
                                          return the DB error
```

The orphan insert runs on its own short transaction, so it cannot be rolled back together with the failure it is recording.

### D11: Soft delete, with the ownership check Phase 2 deferred

`DELETE /api/v1/tenants/{tenant_id}/documents/{document_id}` returns 204 and does this:

1. Load the document inside a tenant transaction. Not visible means 404, which RLS gives for free.
2. Authorize. `DocumentsDeleteAny` allows any document in the tenant. Otherwise `DocumentsDeleteOwn` plus `uploaded_by == ctx.user_id()`. Anything else is 403. A Viewer has neither permission and is 403 before the row is even read.
3. Set `status = 'deleting'`, `deleted_at = now()`. Commit.
4. Best effort `storage.delete(object_key)`. On success a second short transaction sets `status = 'deleted'`. On failure the row stays in `deleting`, a `storage_orphans` row is written, and the reconciler finishes the job.

`app_runtime` gets no DELETE grant on `documents`. Deletes are soft by design, and withholding the grant means a coding mistake cannot hard delete a row. Phase 4 adds the grant when chunk cleanup needs it, or keeps the hard delete in a definer function.

A document in `deleting` or `deleted` is hidden from list and read endpoints. Only the reconciler sees it.

### D12: Status codes, including the 503 correction

Two new `AppError` variants:

| Variant | Status | `code()` |
|---|---|---|
| `PayloadTooLarge` | 413 | `PAYLOAD_TOO_LARGE` |
| `UnsupportedMediaType { .. }` | 415 | `UNSUPPORTED_MEDIA_TYPE` |

`UnsupportedMediaType` carries a safe message naming the accepted formats, because telling a caller which extensions work leaks nothing.

Separately, §16.3 requires a controlled 503 when PostgreSQL is unavailable, but `AppError::Database` maps to 500 today. Phase 3 fixes that by classifying connection level failures (`sqlx::Error::PoolTimedOut`, `sqlx::Error::Io`, and a `DatabaseError` whose SQLSTATE class is `08`) as `ExternalService("database")`, which is already a 503. Query level errors keep mapping to 500. This is a small change to Phase 2 code and is marked P1, not P0, so it can be dropped if the week runs short.

### D13: Scope cuts, stated plainly

Out of Phase 3, with the phase that owns each one:

- `ingestion_jobs`, `outbox_events`, the outbox publisher, Redis Streams and the worker binary: Phase 4.
- Parsing, normalization, token aware chunking, embedding: Phase 4.
- `POST /documents/{id}/retry`: Phase 4, because retry needs a job row.
- `document_chunks` and any vector column: Phase 4 and 5.
- A download endpoint or signed URLs. The worker reads bytes through `FileStorage` directly, so no HTTP route needs to serve document content. Leaving it out makes "the original file remains private" trivially true.
- Per tenant storage and document count quotas: Phase 7. Only `MAX_UPLOAD_BYTES` is enforced now.
- An `Idempotency-Key` header on upload. The content hash already gives idempotent behavior for the case that matters.
- Malware scanning. Hook point only.
- An `active_ingestion_version` column. Adding an unused column now would be speculative, and Phase 4 can add a defaulted column in a backward compatible migration. §25 says not to add things before a requirement justifies them.
- OpenAPI generation through utoipa. Endpoints are documented in `docs/api/documents.md` instead.

---

## 3. Priority tiers

| Tier | Meaning |
|---|---|
| **P0** | Required for the exit criteria |
| **P1** | Required by a design doc security or reliability rule. Do it unless blocked |
| **P2** | Nice to have if time remains |

---

## 4. Tasks

Suggested order and time: Task 1 (1.5h) then 2 (2.5h) then 3 (3h) then 4 (4h) then 5 (3h) then 6 (3h) then 7 (2h) then 9 (1.5h). Task 8 is written alongside every other task, not saved for the end.

---

### Task 1: Carry over groundwork (P0, ~1.5h)

Small changes that everything else depends on. Doing them first keeps the later tasks from each editing the same four files.

**1. Workspace dependencies.** Add to `[workspace.dependencies]` in the root `Cargo.toml`, using versions current as of November 2026:

```toml
object_store = { version = "0.14", default-features = false, features = ["fs", "aws"] }
bytes        = "1.12"
async-trait  = "0.1"
futures-util = "0.3"
```

Add `"multipart"` to the existing axum feature list. `sha2` 0.11 is already there from Phase 2 and needs no change. Verify the whole set builds on 1.99.1 before writing any code, because `object_store` with `aws` drags in `reqwest` and rustls and that is the only real version risk in this phase.

`default-features = false` plus an explicit `fs` keeps the dependency set honest about what is used. Confirm with `cargo tree -p object_store -e features` that `azure`, `gcp` and `http` are absent.

**2. `common-storage/Cargo.toml`** gets `object_store`, `bytes`, `async-trait`, `futures-util`, `thiserror`, `tokio` (dev), plus `common-config` so the factory can read `StorageConfig`.

**3. `common-errors`**: add the two variants from D12.

- `PayloadTooLarge` with no payload, since the limit is already in the message.
- `UnsupportedMediaType { accepted: &'static str }`.
- Extend `code()`, `status()` and `safe_message()`. All three are exhaustive matches, so the compiler finds every site.
- The existing error tests enumerate variants. Extend them rather than replacing them.

**4. `common-config`**: extend `StorageConfig` in the existing `required` / `text` / `number` style.

| Field | Env var | Default | Validation |
|---|---|---|---|
| `backend` | `OBJECT_STORAGE_BACKEND` | `s3` | one of `s3`, `local`, `memory` |
| `local_path` | `OBJECT_STORAGE_LOCAL_PATH` | `./.local-storage` | required when backend is `local` |
| `allow_http` | `OBJECT_STORAGE_ALLOW_HTTP` | `true` | must be `false` when `APP_ENV` is `production` and the endpoint is not `https` |
| `part_size_bytes` | `OBJECT_STORAGE_PART_SIZE_BYTES` | `8388608` | at least 5 MiB, at most 64 MiB |
| `max_upload_bytes` | `MAX_UPLOAD_BYTES` | `20971520` | greater than zero, at most 104857600 |
| `orphan_retention_secs` | `STORAGE_ORPHAN_RETENTION_SECS` | `86400` | greater than zero |

`access_key` and `secret_key` are currently `required`. That is wrong once `memory` and `local` backends exist, so they become required only when `backend` is `s3`. This also removes the fake keys that `routes.rs` tests and `tests/common/mod.rs` currently have to inject, so simplify both while touching them.

Mirror every key into `.env.example` under the existing `OBJECT STORAGE` block, and move `MAX_UPLOAD_BYTES` out of the commented `INGESTION SETTINGS (Phase 4+)` section into a live setting.

**5. `AppState`**: add `pub storage: Arc<dyn FileStorage>`, built inside `AppState::new` through the factory, plus `with_storage(self, Arc<dyn FileStorage>) -> Self` next to the existing `with_rate_limiter`. Because `new` is the only constructor, no existing test needs editing.

**6. P1, from D12**: add `classify_sqlx_error` to `repositories::db_error` and route connection level failures to `ExternalService("database")`. Unit test it with the three error shapes.

**Done when** `cargo build --workspace` and `cargo test --workspace` both pass with no behavior change, and `cargo tree` shows no cloud backend beyond `aws`.

---

### Task 2: Migration `003_documents.sql` (P0, ~2.5h)

Runs as `db_owner`. Forward only. Follows the `002` house style: helper functions first, tables, triggers, grants, then RLS.

**`documents`**, from §8.5:

```sql
CREATE TABLE documents (
    id                uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id         uuid        NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    uploaded_by       uuid        REFERENCES users(id) ON DELETE SET NULL,
    title             text        NOT NULL CHECK (char_length(title) BETWEEN 1 AND 200),
    original_filename text        NOT NULL CHECK (char_length(original_filename) BETWEEN 1 AND 255),
    content_hash      char(64)    NOT NULL CHECK (content_hash ~ '^[0-9a-f]{64}$'),
    object_key        text        NOT NULL CHECK (char_length(object_key) BETWEEN 1 AND 1024),
    mime_type         text        NOT NULL CHECK (mime_type IN (
                          'application/pdf', 'text/plain', 'text/markdown')),
    size_bytes        bigint      NOT NULL CHECK (size_bytes > 0),
    status            text        NOT NULL DEFAULT 'pending' CHECK (status IN (
                          'pending', 'processing', 'ready',
                          'retry_scheduled', 'failed', 'deleting', 'deleted')),
    failure_code      text        CHECK (failure_code IS NULL
                          OR failure_code ~ '^[A-Z][A-Z0-9_]{2,63}$'),
    failure_detail    text        CHECK (char_length(failure_detail) <= 2000),
    chunk_count       integer     NOT NULL DEFAULT 0 CHECK (chunk_count >= 0),
    created_at        timestamptz NOT NULL DEFAULT now(),
    updated_at        timestamptz NOT NULL DEFAULT now(),
    deleted_at        timestamptz,
    CONSTRAINT documents_tenant_id_id_unique UNIQUE (tenant_id, id),
    CONSTRAINT documents_deleted_consistency CHECK (
        (status IN ('deleting', 'deleted')) = (deleted_at IS NOT NULL))
);
```

Points worth defending in review:

- **The `status` CHECK already lists every lifecycle value from §10.4**, even though Phase 3 code only ever writes `pending`, `deleting` and `deleted`. Widening a CHECK later means dropping and re adding it, and that is a lock on a growing table for no benefit. A comment in the migration says which values this phase uses.
- **`documents_deleted_consistency`** makes the soft delete invariant a database fact. A row cannot sit in `deleting` without a timestamp, and a live row cannot carry one.
- **`uploaded_by` references `users(id)`, not a composite key into `memberships`.** A composite FK would guarantee that the uploader was a member of this tenant, but `memberships` rows get deleted when someone leaves, and then the choice is cascading (which would destroy that person's documents) or setting null (which loses the attribution the FK was supposed to protect). `users(id)` with `ON DELETE SET NULL` keeps the document and loses only the name. Tenant consistency of `uploaded_by` is enforced in the application, where the value always comes from the verified `ctx.user_id()`, never from a request body. §7.6 wants composite FKs for references to tenant owned parents, and `users` is a global table, so this is inside the rule rather than an exception to it.
- **No `ON DELETE CASCADE` worries for `tenants`**, because deleting a tenant is not something any endpoint can do in this phase.

**Indexes**, taken from §9.1 and from the queries this phase actually runs:

```sql
CREATE UNIQUE INDEX documents_tenant_content_hash_uniq
    ON documents (tenant_id, content_hash) WHERE deleted_at IS NULL;

CREATE INDEX documents_tenant_created_idx
    ON documents (tenant_id, created_at DESC, id DESC) WHERE deleted_at IS NULL;

CREATE INDEX documents_tenant_status_idx
    ON documents (tenant_id, status) WHERE deleted_at IS NULL;

CREATE INDEX documents_uploader_idx
    ON documents (tenant_id, uploaded_by) WHERE deleted_at IS NULL;

CREATE INDEX documents_cleanup_idx
    ON documents (status, deleted_at) WHERE deleted_at IS NOT NULL;
```

The deduplication index is **partial** on `deleted_at IS NULL`, which is what lets a deleted file be uploaded again. `ON CONFLICT` has to name the same predicate or inference fails, so the insert in Task 5 reads `ON CONFLICT (tenant_id, content_hash) WHERE deleted_at IS NULL DO NOTHING`. Get this wrong and the error is a confusing runtime 42P10, so it is worth a comment in both files.

`documents_tenant_created_idx` includes `id DESC` so keyset pagination has a total order and no tie breaking ambiguity.

**`storage_orphans`**, for D10 and Task 7:

```sql
CREATE TABLE storage_orphans (
    object_key   text        PRIMARY KEY,
    tenant_id    uuid,
    reason       text        NOT NULL CHECK (reason IN (
                     'insert_failed', 'duplicate_cleanup_failed', 'delete_failed')),
    attempts     integer     NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    last_error   text        CHECK (char_length(last_error) <= 500),
    next_attempt_at timestamptz NOT NULL DEFAULT now(),
    created_at   timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX storage_orphans_due_idx ON storage_orphans (next_attempt_at);
```

No RLS, and the ADR says why. The reconciler runs with no tenant context and has to see every row, so a tenant policy would make it blind. The table holds opaque keys and timestamps, never document content, and no endpoint exposes it. This follows the precedent `refresh_tokens` set in Phase 2, where identity data that is not tenant data was documented as an explicit exception rather than forced into the template.

`object_key` is the primary key, so recording the same orphan twice is idempotent and the retry path can use `ON CONFLICT (object_key) DO UPDATE`.

**Trigger**: `documents_set_updated_at BEFORE UPDATE` reusing the existing `set_updated_at()` function.

**Grants**:

```sql
REVOKE ALL ON documents, storage_orphans FROM PUBLIC;
GRANT SELECT, INSERT, UPDATE ON documents TO app_runtime;
GRANT SELECT, INSERT, UPDATE, DELETE ON storage_orphans TO app_runtime;
```

No DELETE on `documents`, per D11. CI asserts the denial.

**RLS**, the Phase 2 template copied without change:

```sql
ALTER TABLE documents ENABLE ROW LEVEL SECURITY;
ALTER TABLE documents FORCE ROW LEVEL SECURITY;

CREATE POLICY documents_select ON documents FOR SELECT
    USING (tenant_id = (SELECT app_tenant_id()));
CREATE POLICY documents_insert ON documents FOR INSERT
    WITH CHECK (tenant_id = (SELECT app_tenant_id()));
CREATE POLICY documents_update ON documents FOR UPDATE
    USING (tenant_id = (SELECT app_tenant_id()))
    WITH CHECK (tenant_id = (SELECT app_tenant_id()));
CREATE POLICY documents_delete ON documents FOR DELETE
    USING (tenant_id = (SELECT app_tenant_id()));
```

The SELECT policy has no `OR user_id = app_user_id()` escape, unlike `memberships`. Documents belong to a tenant, not to a person, so there is no "see your own across tenants" case. The DELETE policy exists even though no DELETE grant does, so that a future grant does not silently arrive without a policy.

**Done when** `sqlx migrate run` applies cleanly on an empty database, `relrowsecurity` and `relforcerowsecurity` are both true for `documents`, and a direct `DELETE FROM documents` as `app_runtime` fails with permission denied.

---

### Task 3: `common-storage`: the trait and the adapter (P0, ~3h)

Module layout:

```text
crates/common-storage/src/
  lib.rs        // re-exports, FileStorage, ObjectWriter, StoredObject
  error.rs      // StorageError
  object_store_backend.rs  // ObjectStoreBackend + ObjectStoreWriter
  factory.rs    // build_storage(&StorageConfig) -> Arc<dyn FileStorage>
```

**`StorageError`**, kept small on purpose so the mapping into `AppError` is unambiguous:

```rust
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("object not found")]
    NotFound,
    #[error("storage unavailable: {0}")]
    Unavailable(String),
    #[error("invalid object key: {0}")]
    InvalidKey(String),
    #[error("writer already finished")]
    WriterClosed,
    #[error("storage backend error: {0}")]
    Backend(String),
}
```

`common-errors` cannot depend on `common-storage` without creating a cycle, so the conversion lives in the app layer as `services::document_service::map_storage_error`. `NotFound` becomes `AppError::NotFound`, `Unavailable` becomes `ExternalService("object-storage")` which is a 503, and everything else becomes `Internal` with the backend text logged but not returned. `object_store::Error::NotFound` maps to `StorageError::NotFound`, and its `Generic` and `Reqwest` variants map to `Unavailable`.

**`ObjectStoreBackend`** holds `Arc<dyn ObjectStore>` and `part_size: usize`. Remember `use object_store::ObjectStoreExt;` for `put`, `get`, `delete` and `put_multipart`, per D5. Keys convert through `object_store::path::Path::parse`, and a parse failure is `InvalidKey` rather than a panic.

`list` uses the streaming `ObjectStore::list(Some(&prefix))` and collects with `futures_util::TryStreamExt::try_collect`, mapping each `ObjectMeta` into `StoredObject { key, size_bytes, last_modified }`.

**`ObjectStoreWriter`** implements D6:

```rust
pub struct ObjectStoreWriter {
    store: Arc<dyn ObjectStore>,
    path: Path,
    buffer: BytesMut,
    part_size: usize,
    upload: Option<Box<dyn MultipartUpload>>,
    done: bool,
}
```

- `write` extends the buffer, then flushes whole parts while `buffer.len() >= part_size`, opening the multipart session on the first flush.
- `finish` returns `WriterClosed` if `done`. With no session it does a single `put` of the buffer. With a session it sends the tail as the last part and calls `complete`. Sets `done`.
- `abort` is idempotent. It clears the buffer and calls `MultipartUpload::abort` when a session exists.
- Rustdoc states the rule that one of `finish` or `abort` must always be called, since S3 cannot clean up on drop.

**`build_storage`** matches on `config.backend`:

- `s3`: `AmazonS3Builder::new()` with endpoint, bucket, region, both keys, and `with_allow_http(config.allow_http)`. Path style is the default, so there is no call to `with_virtual_hosted_style_request`.
- `local`: `LocalFileSystem::new_with_prefix(&config.local_path)`, creating the directory if missing.
- `memory`: `InMemory::new()`.

`health` does a `list` of a fixed probe prefix with a short timeout and treats an empty result as healthy. It never writes, so a readiness probe cannot fill the bucket.

**Tests in this crate**, all against `InMemory` so they need no Docker:

- Round trip through `put` and `get`.
- `begin_write` with one small chunk, then `finish`, then `get` returns the same bytes. Asserts the single `put` path by keeping the payload under the part size.
- `begin_write` with enough chunks to cross two part boundaries, then `finish`, then `get` returns the concatenation. Asserts the multipart path.
- Chunks that do not align to the part size still reassemble byte for byte.
- `abort` after partial writes leaves `get` returning `NotFound`.
- A second `finish` returns `WriterClosed`.
- Zero byte `finish` creates an empty object. The handler rejects empty uploads at a higher level, so the storage layer stays neutral about it.
- `delete` of a missing key, with the chosen behavior pinned by assertion so a backend change cannot quietly flip it.
- `list` returns only keys under the prefix and reports the right sizes.
- A key with `..` in it is rejected as `InvalidKey`.

**Done when** `cargo test -p common-storage` passes with no database and no network.

---

### Task 4: Upload validation, hashing and the streaming loop (P0, ~4h)

The part of the phase with the most edge cases, so it is built as pure logic with no I/O and tested on its own.

**`apps/tenant-saas/src/domain/document.rs`**, following the `domain/tenant.rs` pattern of parse constructors that return `AppError::Validation`:

```rust
pub enum SupportedFormat { Pdf, Text, Markdown }   // mime_type(), from_extension()
pub enum DocumentStatus { Pending, Processing, Ready, RetryScheduled, Failed, Deleting, Deleted }
pub struct ContentHash(String);        // 64 lowercase hex characters
pub struct OriginalFilename(String);   // sanitized, 1 to 255
pub struct DocumentTitle(String);      // trimmed, 1 to 200
```

`DocumentStatus` round trips through `as_str` and `FromStr`, with `FromStr` rejecting anything the CHECK constraint would reject. A unit test asserts the Rust list and the SQL list match, so the two cannot drift apart.

`OriginalFilename::parse` takes the raw multipart filename and keeps only the final path component, so both `/` and `\` separators are handled, which matters because a Windows client sends a full path. It then strips control characters, rejects empty, `.` and `..`, and caps at 255.

`DocumentTitle` falls back to the filename with its extension removed when no `title` part was sent.

**`apps/tenant-saas/src/services/upload_validator.rs`**, the streaming state machine:

```rust
pub struct UploadValidator {
    format: SupportedFormat,
    max_bytes: u64,
    sniff_buffer: BytesMut,   // holds back the first bytes until the sniff
    sniffed: bool,
    hasher: Sha256,
    utf8_carry: [u8; 3],      // partial multi-byte character across a chunk edge
    utf8_carry_len: usize,
    total: u64,
}

impl UploadValidator {
    pub fn new(format: SupportedFormat, max_bytes: u64) -> Self;
    /// Accepts a chunk and returns the bytes that are safe to write, which may
    /// be empty while the sniff buffer is still filling.
    pub fn push(&mut self, chunk: Bytes) -> Result<Bytes, AppError>;
    /// Releases any held bytes and returns the hash and the byte count.
    pub fn finish(self) -> Result<(ContentHash, u64), AppError>;
}
```

The validator owns the hash so the bytes are hashed exactly once, in the same pass that validates and writes them.

Unit tests, all synchronous and fast:

| Case | Expectation |
|---|---|
| Valid minimal PDF in one chunk | `push` returns the bytes, `finish` gives the known SHA-256 |
| Valid PDF split into 1 byte chunks | Same hash. Proves the sniff hold back releases correctly |
| PDF where the 5 byte header spans two chunks | Accepted |
| File claiming `.pdf` whose body is plain text | 415 |
| File claiming `.txt` whose body is a PDF | 415 |
| Truncated 3 byte body claiming `.pdf` | 415, and the sniff still runs at end of stream |
| UTF-8 text with a 3 byte character split across a chunk edge | Accepted, hash unchanged |
| Same text with the character split at every possible offset | Accepted in all cases, driven by a loop |
| Invalid UTF-8 (a lone `0xFF`) in a `.txt` | 415 |
| Trailing incomplete multi byte sequence at end of stream | 415 |
| NUL byte in a `.md` | 415 |
| Body one byte over `max_bytes` | 413, raised on the chunk that crosses the line rather than at the end |
| Zero bytes | 422 from `finish` |
| Hash of a known fixture | Matches the value from `sha256sum`, so the hash is verifiable outside Rust |

**Done when** `cargo test -p tenant-saas upload_validator` passes and the table above is covered line for line.

---

### Task 5: Repository and service (P0, ~3h)

**`repositories/document_repo.rs`**, same shape as `tenant_repo.rs`. Every function takes `&mut Tx` and relies on RLS for the tenant filter rather than adding a `WHERE tenant_id` of its own.

```rust
pub struct DocumentRow { /* every column, object_key included */ }

pub enum InsertOutcome { Created(DocumentRow), Duplicate(DocumentRow) }

/// Inserts a pending document, or returns the existing one with the same
/// content hash. Race free: the partial unique index is the only arbiter.
pub async fn insert_pending(tx: &mut Tx, new: NewDocument) -> Result<InsertOutcome, AppError>;

pub async fn find_by_id(tx: &mut Tx, id: Uuid) -> Result<DocumentRow, AppError>;
pub async fn list_page(tx: &mut Tx, cursor: Option<Cursor>, limit: i64, status: Option<&str>)
    -> Result<Vec<DocumentRow>, AppError>;
pub async fn mark_deleting(tx: &mut Tx, id: Uuid) -> Result<DocumentRow, AppError>;
pub async fn mark_deleted(tx: &mut Tx, id: Uuid) -> Result<(), AppError>;

// storage_orphans, no tenant scope needed
pub async fn record_orphan(pool: &PgPool, key: &str, tenant_id: Uuid, reason: &str, err: &str)
    -> Result<(), AppError>;
pub async fn due_orphans(pool: &PgPool, limit: i64) -> Result<Vec<OrphanRow>, AppError>;
pub async fn clear_orphan(pool: &PgPool, key: &str) -> Result<(), AppError>;
pub async fn defer_orphan(pool: &PgPool, key: &str, err: &str) -> Result<(), AppError>;
```

`insert_pending` in full, because the `ON CONFLICT` predicate is the one place a typo costs an afternoon:

```sql
INSERT INTO documents (tenant_id, uploaded_by, title, original_filename,
                       content_hash, object_key, mime_type, size_bytes)
VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
ON CONFLICT (tenant_id, content_hash) WHERE deleted_at IS NULL DO NOTHING
RETURNING <columns>
```

No row back means a live document already holds that hash, so a second statement in the same transaction reads it:

```sql
SELECT <columns> FROM documents
 WHERE content_hash = $1 AND deleted_at IS NULL
```

RLS supplies the tenant restriction. In the rare case where the conflicting row was soft deleted between the two statements, the SELECT finds nothing and the function retries the insert once before giving up with `AppError::Conflict`. A comment explains that window instead of pretending it does not exist.

`list_page` uses keyset pagination on `(created_at DESC, id DESC)`, matching `documents_tenant_created_idx`, and never `OFFSET`:

```sql
WHERE deleted_at IS NULL
  AND ($1::timestamptz IS NULL OR (created_at, id) < ($1, $2))
  AND ($3::text IS NULL OR status = $3)
ORDER BY created_at DESC, id DESC
LIMIT $4
```

Every function routes errors through `map_db_error`. `find_by_id` and `mark_deleting` use `fetch_one`, so an invisible row arrives as `RowNotFound`, which the existing `From<sqlx::Error>` already turns into `AppError::NotFound`. That is how cross tenant access becomes a 404 with no handler code at all.

**`services/document_service.rs`**, the orchestration layer. Permission checks happen before any transaction opens, matching `tenant_service`.

```rust
pub struct UploadResult { pub document: DocumentRow, pub deduplicated: bool }

pub async fn upload(state: &AppState, ctx: &TenantContext, upload: IncomingUpload)
    -> Result<UploadResult, AppError>;
pub async fn get(state: &AppState, ctx: &TenantContext, id: Uuid) -> Result<DocumentRow, AppError>;
pub async fn list(state: &AppState, ctx: &TenantContext, query: ListQuery)
    -> Result<Vec<DocumentRow>, AppError>;
pub async fn delete(state: &AppState, ctx: &TenantContext, id: Uuid) -> Result<(), AppError>;
```

`upload` is the D7 sequence in code. Points the implementation must honor:

- `ctx.require(Permission::DocumentsUpload)?` runs before a single byte is read, so a Viewer gets a 403 without uploading 20 MiB first.
- The object key comes from `object_key::for_document(ctx.tenant_id(), Uuid::new_v4())`, a tiny module with a unit test, so key construction is not scattered through the service.
- Every exit path between `begin_write` and `finish` calls `writer.abort()`. The cleanest way to guarantee that is a small `async` helper that owns the writer and converts any `Err` into an abort before returning, rather than repeating the call at each `?`.
- The transaction covers only `insert_pending`. No storage call, no hashing and no Redis touch happens while it is open, and the rustdoc says so with a pointer to §7.3.1.
- After a `Duplicate` outcome the just written object is deleted. A failure there becomes a `storage_orphans` row with reason `duplicate_cleanup_failed`, and the request still returns 200, because the client's view of the world is correct either way.
- After a transaction failure the object is deleted, and a failure there becomes `insert_failed`. The original database error is what the client sees.
- One `tracing` event at `target="audit"` per outcome, carrying `tenant_id`, `document_id`, `actor`, `size_bytes`, `mime_type` and `deduplicated`. Never the object key, never the filename, since a filename can carry personal data and §14.4 wants an allowlist.

`delete` implements D11, including the ownership branch:

```rust
if !ctx.has(Permission::DocumentsDeleteAny) {
    ctx.require(Permission::DocumentsDeleteOwn)?;          // Viewer stops here with 403
    if document.uploaded_by != Some(ctx.user_id()) {
        return Err(AppError::Forbidden);                    // 403, not 404: the row is in
    }                                                       // the caller's own tenant
}
```

Returning 403 rather than 404 here is deliberate. Concealment protects other tenants, and this document is inside the caller's tenant, so hiding it would only confuse a legitimate member.

**Done when** `insert_pending` has a database backed test for the created path, the duplicate path and the soft deleted re upload path, and the service compiles with no storage call inside a transaction, verified by reading the code and by a clippy run.

---

### Task 6: HTTP endpoints (P0, ~3h)

**`handlers/documents.rs`**, following `handlers/tenants.rs`:

```rust
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/documents", post(upload_document).get(list_documents))
        .route("/documents/{document_id}", get(get_document).delete(delete_document))
        .route("/documents/{document_id}/status", get(get_status))
}
```

Merged into the existing `tenant_sub` router in `routes.rs`, beside `members` and `invitations`, so the `{tenant_id}` path parameter that `TenantCtx` needs is already in place.

Route local layers, applied to the upload route only so the rest of the API keeps Phase 1 behavior:

- `DefaultBodyLimit::max(max_upload_bytes + 1 MiB)`, read from config rather than hard coded.
- `TimeoutLayer` of 120 seconds, overriding the global 30. A 20 MiB upload on a slow connection legitimately takes longer than 30 seconds, and the global timeout would cut it off with a bare 408 that carries no envelope. The comment records that the global layer is a known envelope gap.

Endpoints and status codes:

| Method and path | Success | Notes |
|---|---|---|
| `POST /documents` | **202** new, **200** duplicate | `DocumentsUpload`. multipart body |
| `GET /documents` | 200 | `DocumentsSearch`. Keyset paged, `?after_created_at=&after_id=&limit=&status=` |
| `GET /documents/{id}` | 200 | `DocumentsSearch` |
| `GET /documents/{id}/status` | 200 | `DocumentsSearch`. Small body: `{ id, status, chunk_count, failure_code, updated_at }` |
| `DELETE /documents/{id}` | 204 | `DocumentsDeleteAny`, or `DocumentsDeleteOwn` plus ownership |

The full error table, which goes into `docs/api/documents.md` as the contract:

| Case | Status | `code` |
|---|---|---|
| Request is not `multipart/form-data` | 415 | `UNSUPPORTED_MEDIA_TYPE` |
| Declared body over the route limit | 413 | `PAYLOAD_TOO_LARGE` |
| Streamed bytes over `MAX_UPLOAD_BYTES` | 413 | `PAYLOAD_TOO_LARGE` |
| Extension not accepted | 415 | `UNSUPPORTED_MEDIA_TYPE` |
| Body disagrees with the extension | 415 | `UNSUPPORTED_MEDIA_TYPE` |
| Text file not valid UTF-8, or contains NUL | 415 | `UNSUPPORTED_MEDIA_TYPE` |
| No `file` part, two `file` parts, or a bad filename | 422 | `VALIDATION_ERROR` |
| Zero byte file | 422 | `VALIDATION_ERROR` |
| `title` longer than 200 characters | 422 | `VALIDATION_ERROR` |
| Caller lacks the permission | 403 | `FORBIDDEN` |
| Caller is not an active member, tenant unknown, or tenant id malformed | 404 | `NOT_FOUND` |
| Membership or tenant suspended | 403 | `FORBIDDEN` |
| Document id belongs to another tenant, or does not exist, or is deleted | 404 | `NOT_FOUND` |
| Member deleting another member's document | 403 | `FORBIDDEN` |
| Object storage unreachable | 503 | `SERVICE_UNAVAILABLE` |
| PostgreSQL unreachable | 503 | `SERVICE_UNAVAILABLE` (P1, D12) |

Response DTOs are hand written `Serialize` structs, as in `tenants.rs`. `DocumentResponse` carries `id`, `title`, `original_filename`, `content_hash`, `mime_type`, `size_bytes`, `status`, `chunk_count`, `uploaded_by`, `created_at` and `updated_at`. It has **no** `object_key`, `failure_detail`, or `deleted_at`. `content_hash` is included on purpose, so a client can detect a duplicate locally before uploading.

The multipart loop in the handler:

1. `while let Some(field) = multipart.next_field().await?`, mapping `MultipartError` to `AppError` so the body keeps the envelope.
2. `title` goes through `field.text()`, which is bounded by the route body limit.
3. `file` records the filename, parses the format, builds an `UploadValidator`, opens the writer, then loops on `field.chunk()`, feeding the validator and writing what it releases.
4. An unknown field name is skipped without reading it.
5. A second `file` field aborts the writer and returns 422.

Add to `routes.rs` tests, matching the three Task 8 route tests already there: an unauthenticated `POST /api/v1/tenants/{uuid}/documents` returns 401 rather than 404, and so does `GET .../documents`. These run without a database.

**Done when** every row of the error table has a passing test and the route tests in `routes.rs` pass with no database.

---

### Task 7: Orphan reconciliation path (P1, ~2h)

§10.2.2 asks for a periodic job. There is no worker process until Phase 4, so Phase 3 ships the logic and one way to run it, and Phase 4 moves the call site into the worker's supervised tasks without touching the logic.

**`services/storage_reconciler.rs`**:

```rust
pub struct ReconcileReport {
    pub retried: usize, pub deleted: usize, pub still_failing: usize,
    pub swept: usize, pub finalized: usize,
}

pub async fn reconcile(state: &AppState) -> Result<ReconcileReport, AppError>;
```

Three passes, cheapest first:

1. **Retry recorded orphans.** Read `due_orphans`, delete each key, then `clear_orphan` on success or `defer_orphan` with exponential backoff and a bounded attempt count on failure. This is the common case and costs no listing.
2. **Finish interrupted deletes.** Find documents in `deleting` whose `deleted_at` is older than one minute, delete the object, and `mark_deleted`. This closes the D11 gap where the storage delete failed after the commit.
3. **Sweep for unreferenced objects.** For each tenant prefix, `list` the keys, and delete any whose `last_modified` is older than `STORAGE_ORPHAN_RETENTION_SECS` and which no live `documents.object_key` references. The age guard is what keeps the sweep from deleting an object belonging to an upload that is still in flight, and it is the only thing standing between this job and data loss, so it gets a comment saying as much.

Pass 3 is the expensive one, so it is opt in through a `deep: bool` argument and off by default.

**How it runs in Phase 3.** `main.rs` reads `std::env::args().nth(1)`. The value `reconcile-orphans` runs one `reconcile` pass, logs the report and exits. Anything else starts the API as now. No argument parsing crate, since one string comparison does not justify a dependency. The README documents the command, and the operations runbook for §26 item 9 points at it.

**Tests** in `tests/integration/orphans.rs`:

- An object with no document row, aged past the retention window, is removed by a deep pass.
- The same object inside the window survives.
- An object referenced by a live document survives a deep pass.
- A `storage_orphans` row whose key no longer exists is cleared rather than retried forever.
- A document stuck in `deleting` reaches `deleted` and its object is gone.
- A document in `pending` whose object exists survives, which is the regression guard for the in flight upload case.

**Done when** all six pass and the report counts match the fixtures exactly.

---

### Task 8: Test suite (P0, written alongside Tasks 2 through 7)

**Harness additions in `tests/common/mod.rs`:**

```rust
impl TestApp {
    /// Builds a multipart body by hand and posts it as this session's user.
    pub async fn upload_file(&self, tenant_id: Uuid, token: &str,
                             filename: &str, bytes: &[u8], title: Option<&str>)
        -> (StatusCode, Value);

    /// Uploads a small valid text file and returns its document id.
    pub async fn create_document(&self, tenant_id: Uuid, token: &str) -> Uuid;

    /// Reads documents straight from the owner pool, bypassing the API.
    pub async fn document_row(&self, tenant_id: Uuid, id: Uuid) -> Option<DocumentFixture>;

    /// True when the key exists in the app's storage backend.
    pub async fn object_exists(&self, key: &str) -> bool;
}
```

`upload_file` writes the multipart body with a fixed boundary rather than pulling in a client crate, since the existing harness already builds requests by hand with `Request::builder`.

Test apps use the `memory` storage backend by default through `with_storage`, so most tests need no MinIO. A small set of tests opts into the `s3` backend to cover the real adapter, and those skip themselves when `OBJECT_STORAGE_ENDPOINT` is unreachable, in the same style as the existing `SKIP_DB_TESTS` guard.

**`tests/integration/documents.rs`** (P0):

- Happy path per format: PDF, TXT, MD. 202, status `pending`, `size_bytes` and `content_hash` match what was sent, object present in storage.
- `title` given, and `title` omitted so the filename becomes the title.
- Every rejection in the Task 6 error table. Each one also asserts that storage is empty afterwards, which is the real test of the abort paths.
- Duplicate upload returns 200 with the same `id`, and `documents` still holds one row.
- **Concurrency**: 10 `tokio::spawn` tasks uploading identical bytes. Exactly one 202, nine 200, one row, and one surviving object. This is the exit criterion, so it also asserts that the nine losers left no orphan objects.
- Soft deleted document can be uploaded again and gets a new id.
- List returns newest first, respects `limit`, pages correctly through a keyset cursor, and hides deleted documents.
- Status endpoint reflects `pending`, and the body carries no object key.
- Delete as Owner, as Admin, as a Member deleting their own upload, as a Member deleting another member's upload (403), as a Viewer (403), and twice in a row (404 the second time).
- After a delete the object is gone and the row reads `deleted`.

**`tests/security/cross_tenant_documents.rs`** (P0), extending the §19.3 matrix to documents:

- User A uploads into Tenant A. User B, with no membership there, gets 404 on read, status and delete, and the body is byte for byte identical to the 404 for a random UUID.
- User B who is a member of Tenant B passes `tenant_a_id` in the path and gets 404.
- Direct SQL through the `app_runtime` pool with Tenant B's context set: SELECT returns zero rows, INSERT with `tenant_id = tenant_a` is refused by the policy, UPDATE affects zero rows, and DELETE is refused by the missing grant.
- Missing tenant context, so no GUC at all, returns zero rows and refuses writes. Fails closed.
- A forged context set by raw `set_config` on a pool transaction, which is how Phase 2 tests this, cannot reach another tenant's rows through the composite unique constraint and the policies.
- `object_key` appears in no response body and in no captured log line, checked by scanning `captured_logs()` for the known key.
- `pool_reuse.rs` gains a `documents` case alongside the existing one, run with `max_connections = 1`.

**Role matrix** in `tests/security/escalation.rs`: Viewer cannot upload or delete, Member can upload, Member cannot delete another member's document, Admin can delete any, Owner can delete any. One assertion per cell.

**Done when** `cargo test --workspace` is green locally and in CI, and every row of §0 names a test that exists.

---

### Task 9: CI, documentation and ADRs (P0 and P1, ~1.5h)

**CI, in `.github/workflows/ci.yml`:**

1. Add a MinIO service to the `test` job, using the same `pgsty/silo` image and tag as `infra/docker-compose.yml` so local and CI cannot drift.
2. A bucket creation step using `mcli`, mirroring the `minio-init` service, including `anonymous set none`.
3. Export `OBJECT_STORAGE_ENDPOINT`, `OBJECT_STORAGE_BUCKET`, `OBJECT_STORAGE_ACCESS_KEY`, `OBJECT_STORAGE_SECRET_KEY` and `OBJECT_STORAGE_BACKEND=s3` on the test step.
4. Extend the existing RLS sanity step to include `documents` in the `relname IN (...)` list.
5. New assertion, matching the style of the DDL and `password_hash` checks: `app_runtime` cannot delete from `documents`.
   ```bash
   output=$(psql "$RUNTIME_URL" -c "DELETE FROM documents;" 2>&1 || true)
   echo "$output" | grep -q "permission denied"
   ```
6. New assertion for the private bucket exit criterion: upload a known object with `mcli`, then request it with plain `curl` and no credentials, and require 403.
   ```bash
   code=$(curl -s -o /dev/null -w '%{http_code}' \
     "http://localhost:9000/tenant-documents/ci-private-probe")
   [ "$code" = "403" ] || { echo "bucket is not private: $code"; exit 1; }
   ```
7. P1, a grep guard in the `check` job in the same style as the `from_verified_membership` guard: `object_key` must not appear in `handlers/`, since no response DTO should carry it.

**Documentation:**

- `README.md`: a curl upload example, the new environment variables, and the `reconcile-orphans` command.
- `docs/api/documents.md`: the endpoint table, the multipart contract, the full error table, and the pagination contract.
- `docs/runbooks/orphan-cleanup.md`: symptoms, the diagnostic query against `storage_orphans`, the command, and the recommended S3 lifecycle rule for incomplete multipart uploads. This is §26 item 9.

**ADRs:**

- `ADR-005: object storage abstraction over object_store.` Why one adapter over the crate's own trait instead of three hand written ones, why the trait is a writer handle rather than §10.2.1's `Bytes` signature, and the note that the repository ADR sequence no longer matches the numbering in design doc §22.
- `ADR-006: upload ordering, deduplication and orphan cleanup.` The storage first ordering and why the hash forces it, the partial unique index, the 200 versus 409 choice with the rejected option recorded, the `storage_orphans` table including why it carries no RLS, and the age guard on the sweep.

**Done when** CI is green on a branch and a fresh clone can follow the README to upload a file with no extra steps.

---

## 5. Dependency graph

```text
Task 1 (deps, config, AppError, AppState)
  |
  +--> Task 2 (migration 003)  ------------------+
  |                                              |
  +--> Task 3 (common-storage) --+               |
  |                              |               |
  +--> Task 4 (validator) -------+               |
                                 |               |
                                 v               v
                           Task 5 (repo + service)
                                 |
                                 v
                           Task 6 (HTTP endpoints)
                                 |
                    +------------+------------+
                    v                         v
             Task 7 (reconciler)       Task 8 (tests, ongoing)
                    |                         |
                    +------------+------------+
                                 v
                           Task 9 (CI, docs, ADRs)
```

Tasks 2, 3 and 4 are independent of each other once Task 1 lands, so they are the natural place to stop and restart across sessions. Task 3 needs no database and Task 4 needs neither database nor storage, which makes both good work for a session without Docker running.

---

## 6. Risks and mitigations

| Risk | Why it matters | Mitigation |
|---|---|---|
| `object_store` with the `aws` feature does not build on Rust 1.99.1 | It pulls in `reqwest` and rustls, the largest new dependency tree in the phase | Task 1 builds the dependency set before any code is written. If it fails, fall back to `default-features = false` with `fs` only, run on the `local` backend, and open an issue for the S3 path |
| Convenience methods moved to `ObjectStoreExt` in 0.13 | `store.put(...)` simply will not resolve, and the error points at the trait rather than the missing import | Noted in D5 and in the module rustdoc. The first unit test in Task 3 catches it immediately |
| The 5 MiB minimum part size | A naive writer that sends every chunk as a part fails on S3 for any multi chunk upload, while passing against `InMemory` | D6's buffering, plus a Task 3 test that crosses two part boundaries, plus one test in Task 8 that runs the real `s3` backend |
| Abandoned multipart uploads accumulate and cost money | S3 cannot clean up on drop | `abort` on every failure path, an explicit rustdoc rule, and an S3 lifecycle rule in the runbook |
| `ON CONFLICT` predicate does not match the partial index | Runtime 42P10, and the message is not obvious | The predicate is written out in both Task 2 and Task 5, and the duplicate test would fail loudly |
| Incremental UTF-8 validation is wrong at chunk edges | Valid files rejected, or invalid bytes accepted and then failing in the Phase 4 parser | The Task 4 test loop splits a multi byte character at every offset |
| The global 30 second timeout cuts off large uploads | A bare 408 with no envelope, and a leaked multipart session | A route local 120 second timeout, and a Task 8 test on a body near the limit |
| A readiness probe starts failing because MinIO is down | Phase 1's acceptance behavior would change, and the API can still serve reads with storage down | Storage health goes on a new `/health/dependencies` endpoint, not on `/health/ready` |
| The deep sweep deletes an in flight upload | Real data loss | The age guard, off by default, plus the Task 7 test asserting a `pending` document's object survives |
| Required storage keys break existing tests | `routes.rs` and `tests/common/mod.rs` both inject fake keys today | Task 1 makes the keys conditional on the `s3` backend and removes the fakes |
| `storage_orphans` has no RLS | Looks like an inconsistency next to the Phase 2 template | Documented in `ADR-006` with the same reasoning Phase 2 used for `refresh_tokens`, and the table holds no tenant content |
| Phase 4 has to retrofit the outbox insert | The upload transaction would need reshaping | The transaction already wraps `insert_pending` and commits. Phase 4 adds two inserts inside the same block and nothing else moves |

---

## 7. Phase 3 deliverable checklist

### Must complete (P0)

- [ ] Workspace dependencies added and building on 1.99.1
- [ ] `AppError::PayloadTooLarge` and `AppError::UnsupportedMediaType`
- [ ] `StorageConfig` extended, `.env.example` and README updated, storage keys required only for `s3`
- [ ] `AppState.storage` built in `AppState::new`, with a `with_storage` test override
- [ ] `003_documents.sql`: `documents`, `storage_orphans`, partial dedup index, the §9.1 indexes, the trigger, grants with no DELETE on `documents`, RLS with ENABLE plus FORCE and four policies
- [ ] `FileStorage` and `ObjectWriter` traits, `StorageError`, `ObjectStoreBackend`, `build_storage`
- [ ] Part buffering with the single `put` path for small objects
- [ ] `domain/document.rs` value objects and `DocumentStatus`
- [ ] `UploadValidator`: sniffing, incremental UTF-8, byte counter, SHA-256
- [ ] `document_repo` with the race free `insert_pending`
- [ ] `document_service`: upload, get, list, delete, with the ownership check
- [ ] Compensating delete plus `storage_orphans` on both failure paths
- [ ] Five endpoints wired under `/api/v1/tenants/{tenant_id}`
- [ ] Route local body limit and timeout
- [ ] `object_key` absent from every response DTO and every log line
- [ ] `tests/integration/documents.rs` covering the full error table and the concurrency case
- [ ] `tests/security/cross_tenant_documents.rs` covering HTTP and direct SQL
- [ ] `pool_reuse.rs` extended to `documents`
- [ ] CI: MinIO service, bucket creation, env, `documents` RLS sanity, the DELETE denial, the private bucket probe
- [ ] `ADR-005` and `ADR-006`
- [ ] `docs/api/documents.md`

### Phase 1 and 2 regression guards (P0)

- [ ] `cargo fmt --all --check` clean
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean
- [ ] `/health/live` still returns 200 with PostgreSQL stopped
- [ ] `/health/ready` still returns 503 only because of PostgreSQL, not because of storage
- [ ] `app_runtime` still cannot run DDL and still cannot read `password_hash`
- [ ] The `from_verified_membership` grep guard still passes
- [ ] Every Phase 2 test still passes unchanged

### Should complete (P1)

- [ ] `storage_reconciler` with all three passes and six tests
- [ ] `reconcile-orphans` entry point on the API binary
- [ ] D12: connection level sqlx errors map to 503
- [ ] `/health/dependencies` reporting storage and Redis
- [ ] `docs/runbooks/orphan-cleanup.md`
- [ ] `object_key` grep guard in CI

### Nice to have (P2)

- [ ] A storage backed test running against the real MinIO adapter, not just `InMemory`
- [ ] `size_bytes` and document count aggregates per tenant, as groundwork for the Phase 7 quota work
- [ ] A `k6` script for the upload path, so Phase 7's load testing starts from something

---

## 8. Phase 3 to Phase 4 handoff

| Concern | State at the end of Phase 3 | How Phase 4 uses it |
|---|---|---|
| Upload transaction | `begin_tenant_tx`, then `insert_pending`, then commit | Add the `ingestion_jobs` insert and the `outbox_events` insert inside the same block. Nothing else about the ordering changes |
| Document status | `pending` is written on upload. The CHECK already allows `processing`, `ready`, `retry_scheduled` and `failed` | The worker moves rows through the rest of §10.4 with no migration needed |
| Composite FK target | `documents` has `UNIQUE (tenant_id, id)` | `document_chunks` and `ingestion_jobs` reference `(tenant_id, document_id)` per §7.6 |
| RLS template | Proven on `documents` with ENABLE plus FORCE and four explicit policies | Copy it for `ingestion_jobs`, `document_chunks` and `outbox_events` in migrations `004` and `005` |
| Reading document bytes | `FileStorage::get(key)` works and `object_key` is on `DocumentRow` | The worker downloads with the same adapter. No HTTP route is involved, so nothing new is exposed |
| Storage adapter | One adapter behind `Arc<dyn FileStorage>`, selectable per environment | The worker binary builds it from the same `build_storage` factory |
| Orphan cleanup | `reconcile(state, deep)` plus a one shot command | Becomes a supervised Tokio task in `tenant-saas-worker`, next to the outbox publisher, on a configurable interval |
| Delete path | Soft delete sets `deleting`, the object is removed, then `deleted` | Phase 4 adds chunk removal between those two steps and may need a DELETE grant or a definer function |
| Permissions | `DocumentsDeleteOwn` now has a real row level ownership check | Reuse the same pattern for retry authorization |
| Retry endpoint | Not present | `POST /documents/{id}/retry` lands in Phase 4 once a job row exists to reset |
| Content hash | Computed once during the upload stream and stored as 64 hex characters | The worker uses it for idempotency checks and the Phase 5 re embedding migration uses it to detect unchanged content |
| Test harness | `upload_file`, `create_document`, `document_row`, `object_exists`, and a memory backend by default | Ingestion tests seed documents through `create_document` instead of building rows by hand |
| CI | MinIO service, private bucket probe, `documents` RLS and grant assertions | Add Redis Streams assertions and an outbox backlog check in the same style |
| Config | `MAX_UPLOAD_BYTES` live, `CHUNK_SIZE_TOKENS` and friends still commented out in `.env.example` | Uncomment and wire them in Phase 4 |

---

## 9. Definition of done for Phase 3

1. A fresh clone can run `cp .env.example .env`, then `docker compose -f infra/docker-compose.yml up -d --wait`, then `cargo run --bin tenant-saas-api`, which applies migrations `001` through `003`. Using only README commands, a developer can register a user, create a tenant, upload a PDF, read its status, upload the same file again and see the 200 with the same id, delete it, and upload it once more successfully.
2. The exact Phase 1 and Phase 2 CI commands pass locally and in CI: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo test --workspace`. Phase 1 acceptance behavior is unchanged, including `/health/live` returning 200 with PostgreSQL down, `/health/ready` returning 503, and `app_runtime` being unable to run DDL.
3. Every exit criterion in §0 names a test that exists and passes.
4. No code path holds a PostgreSQL transaction open across a storage call. Verified by reading `document_service` and by the rustdoc rule on `begin_tenant_tx`.
5. No endpoint trusts a client supplied tenant id, filename, MIME type or content length. The tenant comes from `TenantCtx`, the MIME type from the server's own sniff, and the size from the server's own counter.
6. `object_key` never leaves the process, asserted in tests and guarded in CI.
7. Both failure paths in §10.2.2 leave either no object or a recorded orphan, never a silent leak.
8. `ADR-005` and `ADR-006` are merged, and the P0 and P1 sections of §7 are fully ticked.
