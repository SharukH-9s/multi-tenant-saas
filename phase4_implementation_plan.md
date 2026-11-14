# Phase 4: Reliable Ingestion Implementation Plan

> Source of truth: `multi_tenant_ai_saas_system_design.md` §5.3, §7.3, §7.3.1, §7.6, §8.6–§8.8, §10.4–§10.10, §12.4, §16, §23 (Phase 4).
> Decisions already accepted: `docs/adr/ADR-007-ingestion-outbox-worker.md` (written in T0).
> Builds on: `phase1_implementation_plan.md`, `phase2_implementation_plan.md`, `phase3_implementation_plan.md` (all complete).
> Budget: Weeks 4–5, about 40–45 focused hours.

---

## 0. Goal and exit criteria

An uploaded document goes from `pending` to `ready` without help. The worker parses it, splits it into token-bounded chunks, embeds each chunk, and stores the chunks with their vectors. No step loses work when a process crashes, and no step holds a database connection during network I/O.

Phase 4 stops before search. It adds no `tsvector`, no HNSW index, no search query and no RAG. Phase 5 builds those on the chunks this phase writes.

Roadmap exit criteria and the test that proves each one:

| Exit criterion | Proof |
|---|---|
| API acceptance is durable | `tests/integration/ingestion_enqueue.rs`: an upload commits the document, job and outbox event in one transaction. An upload with Redis on a dead port (`TestApp::spawn_with_dead_redis`) still returns 202 and still leaves a job plus an unpublished outbox row. A forced failure after `insert_pending` leaves no document, no job and no object |
| Publisher and worker crashes do not lose durable work | `tests/integration/ingestion_recovery.rs`: (a) **publisher crash**: claim plus XADD, skip the mark, expire the claim, publish again, and the job still ends `succeeded` exactly once. (b) **worker crash**: claim the job, drop it, expire the lease, sweep, and the job is requeued and finishes with `attempts = 2`. (c) **lost doorbell**: delete the stream entry, age the job, sweep, and the job is re-announced and finishes |
| Duplicate events do not create duplicate logical chunks | `tests/integration/ingestion_pipeline.rs`: the same event processed twice (in sequence and with two concurrent tasks) leaves one `succeeded` job, an unchanged `chunk_count`, and `COUNT(*)` of chunks equal to `chunk_count`. A **stale worker** whose lease expired is refused at persist time and the newer attempt's chunks survive |
| Failed jobs can be retried safely | `tests/integration/ingestion_retry.rs`: a transient failure retries with backoff and reaches `ready`. A permanent failure lands in `failed` with a safe code and is not retried. Exhausted attempts reach `failed`. `POST .../retry` returns 202 for a failed document, 409 otherwise, 403 for a Viewer, and two concurrent retries create exactly one job |

Earlier properties that must not regress:

| Carried property | Proof |
|---|---|
| Tenant A cannot see or touch Tenant B jobs or chunks | `tests/security/cross_tenant_ingestion.rs`: direct SQL through `app_runtime` with B's context sees no A rows and cannot write A rows. An event carrying A's `job_id` with B's `tenant_id` changes nothing |
| Pool reuse leaks no tenant context | `pool_reuse.rs` gets a `begin_system_tenant_tx` case, run with `max_connections = 1` |
| No transaction is open during I/O | Code review of `ingestion_service`, plus the rustdoc rule on every `*_repo` function the worker calls, plus a clippy-clean build |
| Phase 1–3 behavior unchanged | Every existing test passes without edits. Upload still returns 202/200 with the same body |

---

## 1. Baseline after Phase 3 and T0 (what exists, do not redo)

Checked in the repository on 2026-11-14 (`git log` head `edc509b`, plus the uncommitted T0 diff):

| Area | State now | How Phase 4 uses it |
|---|---|---|
| Toolchain | Rust **1.99.1** pinned in `rust-toolchain.toml`, CI and the Dockerfile | Every new dependency must build on 1.99.1 |
| `documents` | `003_documents.sql`. The status CHECK already allows `processing`, `ready`, `retry_scheduled`, `failed`. `UNIQUE (tenant_id, id)` exists for composite FKs. RLS is **ENABLE without FORCE**, so `db_owner` is exempt | No `documents` migration needed. New tables copy the no-FORCE choice (see D2) |
| Upload transaction | `document_service::upload` opens `begin_tenant_tx`, calls `insert_pending`, and commits inside one `match`. On `Err` it drops the tx, deletes the object and records an orphan | T2 adds the job and outbox inserts inside the `Created` arm, before `commit`, and routes their errors through the same compensating path |
| Delete path | `mark_deleting` (tx 1, includes the ownership check), storage delete, then `mark_deleted` (tx 2). Reconciler pass 2 finishes rows stuck in `deleting` | T2 deletes chunks inside tx 1. The job is **not** touched there (see D9) |
| Status endpoint | `GET .../documents/{id}/status` returns `{id, status, chunk_count, failure_code, updated_at}` | Already enough for polling. No DTO change |
| `DocumentStatus` | `domain/document.rs` has every variant plus `as_str`, `all_str` and `FromStr` | Reused. T5 adds `JobStatus` next to it in `domain/ingestion.rs` |
| `TenantContext` | Built only in `auth::tenant_resolver`, enforced by a CI grep guard. `begin_tenant_tx(&pool, &ctx)` sets `app.current_tenant_id` **and** `app.current_user_id` | The worker has no user, so T5 adds `begin_system_tenant_tx(pool, tenant_id)` with its own grep guard (ADR-007 §2) |
| SECURITY DEFINER precedent | `002_identity.sql` has three narrow definer functions with `SET search_path = pg_catalog, public`, `REVOKE ... FROM PUBLIC`, `GRANT EXECUTE ... TO app_runtime` | `sweep_ingestion_jobs()` copies this template exactly |
| Cross-tenant maintenance | `AppState.owner_pool: Option<PgPool>` (db_owner), set only when `DATABASE_MIGRATION_URL` is present. The reconciler uses it for passes 2 and 3 | The **sweeper does not** use it (ADR-007 §4). The reconciler moves into the worker unchanged and keeps using it when present |
| Redis | Workspace `redis 1.x` with `tokio-comp`, `connection-manager`, `script`. `RateLimiter` builds a lazy `ConnectionManager` with 150 ms timeouts. **Default response timeout in redis 1.8 is 500 ms** (`client.rs: DEFAULT_RESPONSE_TIMEOUT`) | A `XREADGROUP BLOCK 2000` on a default connection times out. T3 uses a separate reader connection with a longer timeout (D6) |
| `common-messaging` | Stub crate with an empty `[dependencies]` | T3 fills it |
| `FileStorage::get` | Returns the whole object as `Bytes`. The doc comment already says "bounded by `MAX_UPLOAD_BYTES` in the Phase 4 worker" | The worker checks `size_bytes` before downloading, then the length and hash after (T9) |
| `map_db_error` | Maps unique violations by constraint name. `classify_sqlx_error` turns pool timeouts, I/O errors and SQLSTATE `08` into `ExternalService("database")` | The worker classifies `ExternalService` as transient (T9). T10 adds one constraint name |
| Tests | `integration-tests` crate. `TestApp` uses one shared database, a **memory** storage backend by default, an `owner_pool`, `spawn_with_dead_redis`, `create_document`, `document_row` and `captured_logs()`. Tests run in parallel | New ingestion tests need stream isolation and a lock around global tasks (D12) |
| CI | `check` (fmt, clippy, two grep guards), `test` (Postgres `pgvector/pgvector:0.8.0-pg16`, Redis 7, MinIO, role and RLS assertions), `build` (`--bin tenant-saas-api` only) | T11 adds a guard, new assertions, and the worker binary to `build` |
| Dockerfile | Builds and copies `tenant-saas-api` only | T11 builds and copies both binaries |

### T0 status: done, plus five small follow-ups

T0 delivered `IngestionConfig` with validation and tests, the new workspace dependencies (`tokio-util 0.7`, `tiktoken-rs 0.6`, `pdf-extract 0.7`, `reqwest 0.12` with rustls), and ADR-007. The diff leaves five gaps. They are small, but T1–T5 depend on them, so close them first. Each one is a commit-sized change:

| # | Gap | Fix |
|---|---|---|
| G1 | The workspace `redis` dependency has no **`streams`** feature, so `xadd`, `xread_options`, `xautoclaim_options` and the reply types do not exist | Add `"streams"` to the features list. Fallback: raw `redis::cmd("XADD")`, which needs no feature |
| G2 | `.env.example` was not updated. It still has the commented `Phase 4+` block | Replace it with a live `INGESTION` block listing every key in `IngestionConfig`, defaults included, and `EMBEDDING_API_KEY` commented out |
| G3 | Validation does not reject `EMBEDDING_PROVIDER=openai` with no `EMBEDDING_API_KEY` | Add that rule plus a test. The worker should refuse to start rather than fail every job |
| G4 | The app crate lacks `async-trait` (needed for `EmbeddingProvider`) and `rand` (needed for jitter) | Add both from the workspace |
| G5 | pgvector is not installed. `001_foundation.sql` says "pgvector is added in the Phase 5 migration", and it is unverified whether `db_owner` may run `CREATE EXTENSION vector` (pgvector may not be marked `trusted`) | Add `CREATE EXTENSION IF NOT EXISTS vector;` to `infra/postgres/init/00_roles.sql`, which runs as the bootstrap superuser in Compose and in CI. Migration 004 keeps `IF NOT EXISTS` as a no-op. **Existing local volumes do not re-run init scripts**, so the README tells developers to run that line once as `postgres` |

**Done when** `cargo check --workspace` passes and `cargo test -p common-config` is green with the G3 test.

### Design-doc conflicts this plan resolves

1. §5.3 originally held a `SKIP LOCKED` transaction across Redis I/O. This was already fixed in the doc and in ADR-007 §1 (claim-with-timeout).
2. §8.6 lists `tsv_content` and an HNSW index on `document_chunks`. Both move to Phase 5 (ADR-007 §6). The column set otherwise follows §8.6, including `embedding_version`.
3. §10.10 recommends versioned staging for large documents. Phase 4 uses one-transaction replace with a chunk cap instead (ADR-007 §5). See D10.
4. §10.4 has no `cancelled` state. Deleting a document mid-processing needs one for the job. Documents still use the §10.4 values only.
5. §16 asks for metrics and traces. Phase 1–3 emit structured `tracing` logs only. Phase 4 adds structured log fields and a heartbeat. OpenTelemetry stays in Phase 7.

---

## 2. Key design decisions

ADR-007 already settles the big choices: claim-with-timeout, `begin_system_tenant_tx`, no RLS on the outbox, a definer-function sweeper, one-transaction chunk replace, the vector column now with indexes later, `pdf-extract` in `spawn_blocking`, the fake and OpenAI providers, and the versioned event. The decisions below are the implementation-level ones that ADR-007 does not cover. Add a short "Amendments" section to ADR-007 for D1, D2, D4 and D9 when T11 lands.

### D1: One recovery path, owned by the database

Redis delivers a doorbell, and that is all it does. Every decision about what work exists comes from `ingestion_jobs`:

| Situation | Who notices | What happens |
|---|---|---|
| Normal upload | Upload tx writes the job and the outbox event | Publisher sends the event, consumer claims and processes |
| Publisher crashed after XADD | Claim on the outbox row expires | Republished. Duplicate event, harmless |
| Worker crashed mid-job | Sweeper: `processing` with `locked_until < now()` | Job back to `pending`, new outbox event |
| Transient failure | Worker writes `retry_scheduled` with `available_at` | Sweeper turns it into `pending` plus an event once `available_at` passes |
| Event lost (stream trimmed, Redis flushed, poison-acked) | Sweeper: `pending` and `last_enqueued_at` older than `JOB_LEASE_SECS`, and no unpublished event exists | New outbox event |
| Unacked message whose consumer died | Consumer reclaim loop (`XAUTOCLAIM`) | Delivered again. The claim either succeeds or the message is acked as stale |

A `retry_scheduled` job is **not** directly claimable. Only the sweeper moves it back to `pending`. A duplicate message that arrives early therefore cannot bypass the backoff, and the consumer acks it as stale.

### D2: New tenant tables use ENABLE without FORCE

`ingestion_jobs` and `document_chunks` get four tenant policies each, like `documents`, with **no `FORCE`**. The sweeper is a `SECURITY DEFINER` function owned by `db_owner`, and it can only see every tenant's rows if the owner is exempt from RLS. `app_runtime` is never the owner, so it stays fully bound. CI already asserts that neither role has `BYPASSRLS`. T11 adds an assertion that `relforcerowsecurity` is false for these two tables, so a later "tidy-up" cannot silently blind the sweeper.

### D3: Claim, process, persist: three steps, two short transactions

```text
event -> [tx A: lock document, claim job (lease token), document -> processing] -> commit
      -> download object, verify size+hash        (no tx)
      -> parse + normalize   (spawn_blocking)      (no tx)
      -> chunk               (spawn_blocking)      (no tx)
      -> embed in batches    (HTTP)                (no tx, lease extended per batch)
      -> [tx B: lock document (must be processing), verify lease, replace chunks,
                document -> ready, job -> succeeded] -> commit
      -> XACK
```

If tx B finds the lease changed, or the document no longer `processing`, it rolls back and reports `Superseded`. A stale worker can therefore never overwrite newer results.

### D4: Lock order is documents first, then ingestion_jobs, everywhere

Several transactions touch both tables: the claim, the persist, the failure write, the retry endpoint, and the sweeper. If two of them lock the rows in opposite orders they can deadlock. The rule:

- Every Rust-side transaction locks the `documents` row first (`SELECT ... FOR UPDATE`, or an `UPDATE` on it), then the job row.
- The sweeper selects with `FOR UPDATE OF j, d SKIP LOCKED`. It never waits for a lock, so it cannot take part in a deadlock.
- The API delete path locks only `documents` and `document_chunks` and never touches the job (D9).

Rustdoc on `ingestion_repo` states the rule. A test runs 20 concurrent claim/persist/delete operations and requires zero `40P01` errors.

### D5: Publisher: Redis outages never dead-letter

ADR-007's claim SQL increments `publish_attempts` on every claim, so it counts claims and is diagnostic only. The rules:

- XADD fails with a connection or timeout error: stop the batch, **release** the remaining claims (`claimed_until = NULL`), sleep with exponential backoff capped at 5 s, then try again. Nothing is dead-lettered, so a 30-minute Redis outage loses nothing and drains once Redis returns.
- Payload is not a valid `IngestionEvent` (this should never happen, but an old row or a manual insert could cause it): set `dead_lettered_at`, `last_error`, and log at `error`. This is the only dead-letter path.
- Successful rows are marked in one statement per batch: `UPDATE ... SET published_at = now() WHERE id = ANY($1)`.
- The sweeper deletes published rows older than 7 days (bounded batch), so the table does not grow without limit.

### D6: Two Redis connections in the worker

- `cmd`: a `ConnectionManager` with 2 s timeouts for `XADD`, `XACK`, `XAUTOCLAIM`, `XLEN` and `PING`.
- `reader`: a second `ConnectionManager` with response timeout `block_ms + 2000`, used only by the single `XREADGROUP BLOCK` loop.

A multiplexed connection processes commands in order, so a blocking read on the shared connection would delay every ack behind it. The default 500 ms response timeout would also kill every `BLOCK 2000` read. Both problems disappear with a dedicated reader.

The stream is capped with `XADD ... MAXLEN ~ 100000`. Trimming can drop an entry that is still pending, and D1's lost-doorbell rule recovers it. The group is created with start id `0` (not `$`) plus `MKSTREAM`, so events published before the first worker ever starts are not skipped.

### D7: Ack policy

| Outcome of `process(event)` | Ack? |
|---|---|
| `Succeeded`, `Failed`, `RetryScheduled`, `Cancelled` | yes, after the commit |
| `Skipped` (job not claimable: done, leased by someone else, `retry_scheduled`, or exhausted) | yes |
| `Superseded` (lease lost before persist) | yes |
| `Poison` (bad JSON, unknown `v`, tenant/job/document mismatch) | yes, plus a `warn` log with the stream id and **no payload body** |
| `Unavailable` (database unreachable **before** a claim committed) | **no**. Left pending for `XAUTOCLAIM` |
| Task panicked | **no**. The lease expires and the sweeper takes over |

Each message runs in its own `tokio::spawn` holding a semaphore permit, so a panic kills one job, not the consumer loop.

### D8: Vectors cross the wire as text literals

sqlx 0.9 has no native `vector` type, and whether the `pgvector` crate's sqlx integration supports sqlx 0.9 has not been checked. Phase 4 therefore binds each embedding as a text literal `'[0.0123,-0.4,...]'` and casts it in SQL. Bulk insert uses `UNNEST` over parallel arrays:

```sql
INSERT INTO document_chunks
    (tenant_id, document_id, chunk_index, content, token_count,
     embedding, embedding_model, embedding_version, metadata)
SELECT $1, $2, t.idx, t.content, t.tokens, t.emb::vector, $3, $4, t.meta
  FROM UNNEST($5::int[], $6::text[], $7::int[], $8::text[], $9::jsonb[])
       AS t(idx, content, tokens, emb, meta)
```

Each statement carries 200 rows, which is about 3 MB of vector text. A 2000-chunk document is 10 statements inside the same transaction. Formatting uses Rust's `{}` for `f32`, which round-trips exactly. `vector_dims(embedding)` checks dimensions in tests. Replacing this with the `pgvector` crate later only changes `chunk_repo`.

### D9: Deleting a document does not touch its job

`document_service::delete` adds one statement to its first transaction: `DELETE FROM document_chunks WHERE document_id = $1`. It does not cancel the job. Correctness comes from elsewhere:

- The persist transaction requires `documents.status = 'processing'`. A deleted document is `deleting` or `deleted`, so a late worker writes nothing.
- When the worker claims a job whose document is deleted, it marks the job `cancelled` and acks.
- The sweeper also cancels any active job whose document has `deleted_at IS NOT NULL`.

This keeps the user-facing delete to the tables it already locks, which is what makes D4 easy to hold.

### D10: Chunk cap instead of staging

`MAX_CHUNKS_PER_DOC` (2000) times 500 tokens is roughly 1M tokens. That covers a 20 MiB text document with room to spare. A document over the cap fails permanently with `DOCUMENT_TOO_LARGE` rather than being truncated silently. Search results must never be secretly incomplete.

### D11: The embedder lives in AppState, the tokenizer lives in the worker

- `AppState.embedder: Arc<dyn EmbeddingProvider>` is built in `AppState::new` from `IngestionConfig` (fake by default) and overridden with `with_embedder`. Phase 5's search endpoint needs query embeddings in the API, so it belongs here. Existing tests are unaffected because the default is the offline fake.
- The tokenizer (`tiktoken_rs::CoreBPE`, about 100 ms to build) is built once at worker start and held in `IngestionDeps` behind an `Arc`.

### D12: Test isolation for global background tasks

Tests share one database and run in parallel, but the publisher and the sweeper act on **all** tenants. To stop tests interfering:

1. Every `TestApp` gets a unique `INGEST_STREAM` (`test:ingest:{uuid}`) and group, so consumers never steal another test's messages.
2. Most pipeline tests skip Redis and the outbox entirely. They read the job row and call `ingestion_service::process(&deps, event)` directly.
3. Tests that run the publisher or the sweeper hold `static INGEST_GLOBAL: tokio::sync::Mutex<()>`. They assert only on their own tenant's rows and drain with a loop (`publish_batch` until it returns 0) because other tests' backlog may come first.
4. Time is never mocked. Tests push timestamps into the past through the owner pool (`UPDATE ingestion_jobs SET locked_until = now() - interval '1 second'`).

### D13: Failure codes are a closed set

```rust
pub enum FailureCode {
    ParseFailed,            // PARSE_FAILED                permanent
    DocumentEmpty,          // DOCUMENT_EMPTY              permanent
    DocumentTooLarge,       // DOCUMENT_TOO_LARGE          permanent
    UnsupportedFormat,      // UNSUPPORTED_FORMAT          permanent
    ObjectMissing,          // OBJECT_MISSING              permanent
    ObjectCorrupt,          // OBJECT_CORRUPT              permanent (size or hash mismatch)
    EmbeddingRejected,      // EMBEDDING_REJECTED          permanent (4xx except 401/403/429)
    EmbeddingInvalid,       // EMBEDDING_INVALID_RESPONSE  permanent
    StorageUnavailable,     // STORAGE_UNAVAILABLE         transient
    EmbeddingUnavailable,   // EMBEDDING_UNAVAILABLE       transient (429, 5xx, timeout, 401/403)
    DatabaseUnavailable,    // DATABASE_UNAVAILABLE        transient
    ParseTimeout,           // PARSE_TIMEOUT               transient (machine may be busy)
    AttemptsExhausted,      // ATTEMPTS_EXHAUSTED          written by sweeper/worker
    Internal,               // INTERNAL_ERROR              transient
}
```

A unit test checks every `as_str()` against the `documents.failure_code` regex `^[A-Z][A-Z0-9_]{2,63}$`. `401`/`403` from the provider count as transient on purpose. A bad API key is an operator mistake, so the job should wait in retry for the operator to fix the key rather than fail every document permanently. The worker logs it at `error` with `"check EMBEDDING_API_KEY"`.

`failure_detail` is built by the worker, never copied from a provider body or a parser panic message. It is at most 300 characters and never contains the object key, file name, API key or document text. Examples: `"pdf parser error: invalid xref"` or `"provider returned 400"`.

---

## 3. State machines

### Job (`ingestion_jobs.status`)

```text
             upload / manual retry
                     |
                     v
   +----------> [pending] <-----------------------------+
   |                 | claim (attempts+1, lease)        |
   |                 v                                  |
   |           [processing] --- success ---> [succeeded]|
   |              |   |   \                             |
   |  lease expired   |    \-- permanent / exhausted --> [failed]
   |  (sweeper)       |                                 |
   +------------------+ transient, attempts left        |
                      v                                 |
              [retry_scheduled] -- available_at due ----+
                                    (sweeper)
   any active state --- document deleted ---> [cancelled]
```

`JobStatus::can_transition_to` encodes exactly these edges, and a unit test walks the full 6×6 table.

### Job and document together

| Event | Job | Document | Writer |
|---|---|---|---|
| Upload accepted | `pending` | `pending` | API, upload tx |
| Claim | `processing`, `attempts+1`, new `lease_token` | `processing`, failure fields cleared | Worker, tx A |
| Success | `succeeded`, lease cleared, `completed_at` | `ready`, `chunk_count = n` | Worker, tx B |
| Transient, attempts left | `retry_scheduled`, `available_at` | `retry_scheduled`, `failure_code` set | Worker |
| Transient at max, or permanent | `failed`, `last_error_code` | `failed`, `failure_code`, `failure_detail` | Worker |
| Lease expired, attempts left | `pending`, `last_enqueued_at = now()` | `retry_scheduled` | Sweeper |
| Lease expired at max attempts | `failed`, `ATTEMPTS_EXHAUSTED` | `failed`, `ATTEMPTS_EXHAUSTED` | Sweeper |
| Retry due | `pending` | unchanged (`retry_scheduled`) | Sweeper |
| Document deleted | `cancelled` | `deleting`/`deleted` (unchanged) | Worker claim or sweeper |
| Manual retry | new row `pending` (old stays `failed`) | `pending`, failure cleared | API, retry tx |

---

## 4. Priority tiers

| Tier | Meaning |
|---|---|
| **P0** | Required for an exit criterion |
| **P1** | Required by a design-doc security or reliability rule. Do it unless blocked |
| **P2** | Nice to have if time remains |

---

## 5. Module layout (end of Phase 4)

Matches §6.1 (`workers/`, `ai/`, `domain/ingestion`):

```text
crates/common-messaging/src/
  lib.rs            // re-exports
  event.rs          // IngestionEvent {v, job_id, tenant_id, document_id}, EventError
  streams.rs        // StreamClient: ensure_group, publish, read, ack, reclaim, stats, health
  error.rs          // StreamError { Unavailable, Protocol }

crates/common-database/src/lib.rs
  + begin_system_tenant_tx(pool, tenant_id)

apps/tenant-saas/
  migrations/004_ingestion.sql
  migrations/005_ingestion_sweeper.sql
  src/bin/worker.rs                    // tenant-saas-worker entry point
  src/domain/ingestion.rs              // JobStatus, FailureCode, FailureKind, retry_delay()
  src/ingestion/mod.rs                 // pure pipeline pieces, no I/O
  src/ingestion/parser.rs              // bytes + mime -> ParsedDocument
  src/ingestion/normalize.rs
  src/ingestion/chunker.rs
  src/ai/mod.rs                        // build_embedder(&IngestionConfig)
  src/ai/embedding.rs                  // trait, EmbeddingError, validate_vectors, embed_in_batches
  src/ai/fake.rs
  src/ai/openai.rs
  src/repositories/ingestion_repo.rs   // enqueue, claim, extend_lease, schedule_retry, mark_failed, cancel
  src/repositories/outbox_repo.rs      // insert, claim_batch, mark_published, release, dead_letter, purge
  src/repositories/chunk_repo.rs       // replace_for_document, delete_for_document, count
  src/services/ingestion_service.rs    // IngestionDeps, process(event) -> ProcessOutcome
  src/workers/mod.rs
  src/workers/supervisor.rs            // run(), supervise(), shutdown
  src/workers/outbox_publisher.rs
  src/workers/ingestion_consumer.rs
  src/workers/sweeper.rs
```

Pure modules (`ingestion/*`, `ai/fake.rs`, `domain/ingestion.rs`) import no `sqlx`, `axum` or `redis`.

---

## 6. Tasks

Suggested order: T0 follow-ups → T1 → T2 → T3 → T5 → T6 → T7 → T8 → T9 → T4 → T10 → T11. T3, T6, T7 and T8 need no database and can be done in any order, or on a machine without Docker. Tests are written inside each task, not saved for the end.

---

### T1: Migration `004_ingestion.sql` (P0, ~3h)

Runs as `db_owner`, forward-only, in the `003` house style: extension, tables, indexes, triggers, grants, RLS, backfill.

```sql
-- No-op when 00_roles.sql already installed it (see T0 follow-up G5).
CREATE EXTENSION IF NOT EXISTS vector;
```

**`ingestion_jobs`** (§8.7 plus the lease fields):

```sql
CREATE TABLE ingestion_jobs (
    id               uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id        uuid        NOT NULL,
    document_id      uuid        NOT NULL,
    status           text        NOT NULL DEFAULT 'pending' CHECK (status IN (
                         'pending', 'processing', 'retry_scheduled',
                         'succeeded', 'failed', 'cancelled')),
    attempts         integer     NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    max_attempts     integer     NOT NULL CHECK (max_attempts BETWEEN 1 AND 100),
    available_at     timestamptz NOT NULL DEFAULT now(),
    last_enqueued_at timestamptz NOT NULL DEFAULT now(),
    lease_token      uuid,
    locked_by        text        CHECK (char_length(locked_by) <= 200),
    locked_until     timestamptz,
    last_error_code  text        CHECK (last_error_code IS NULL
                         OR last_error_code ~ '^[A-Z][A-Z0-9_]{2,63}$'),
    last_error_at    timestamptz,
    created_at       timestamptz NOT NULL DEFAULT now(),
    updated_at       timestamptz NOT NULL DEFAULT now(),
    completed_at     timestamptz,
    CONSTRAINT ingestion_jobs_tenant_id_id_unique UNIQUE (tenant_id, id),
    CONSTRAINT ingestion_jobs_document_fk FOREIGN KEY (tenant_id, document_id)
        REFERENCES documents (tenant_id, id) ON DELETE CASCADE,
    -- A lease exists exactly while the job is processing.
    CONSTRAINT ingestion_jobs_lease_consistency CHECK (
        (status = 'processing') = (lease_token IS NOT NULL AND locked_until IS NOT NULL))
);

-- At most one active job per document. Manual retry and duplicate enqueue rely on this.
CREATE UNIQUE INDEX ingestion_jobs_one_active
    ON ingestion_jobs (tenant_id, document_id)
    WHERE status IN ('pending', 'processing', 'retry_scheduled');

CREATE INDEX ingestion_jobs_document_idx
    ON ingestion_jobs (tenant_id, document_id, created_at DESC);

-- Sweeper probes, one partial index per rule.
CREATE INDEX ingestion_jobs_lease_idx   ON ingestion_jobs (locked_until)     WHERE status = 'processing';
CREATE INDEX ingestion_jobs_retry_idx   ON ingestion_jobs (available_at)     WHERE status = 'retry_scheduled';
CREATE INDEX ingestion_jobs_pending_idx ON ingestion_jobs (last_enqueued_at) WHERE status = 'pending';
```

**`outbox_events`** (§8.8 as amended):

```sql
CREATE TABLE outbox_events (
    id               uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id        uuid,
    aggregate_type   text        NOT NULL CHECK (char_length(aggregate_type) BETWEEN 1 AND 64),
    aggregate_id     uuid        NOT NULL,
    event_type       text        NOT NULL CHECK (char_length(event_type) BETWEEN 1 AND 128),
    payload          jsonb       NOT NULL CHECK (jsonb_typeof(payload) = 'object'),
    created_at       timestamptz NOT NULL DEFAULT now(),
    claimed_until    timestamptz,
    publish_attempts integer     NOT NULL DEFAULT 0 CHECK (publish_attempts >= 0),
    published_at     timestamptz,
    last_error       text        CHECK (char_length(last_error) <= 500),
    dead_lettered_at timestamptz
);
CREATE INDEX outbox_unpublished_idx ON outbox_events (created_at)
    WHERE published_at IS NULL AND dead_lettered_at IS NULL;
CREATE INDEX outbox_unpublished_aggregate_idx ON outbox_events (aggregate_id)
    WHERE published_at IS NULL AND dead_lettered_at IS NULL;   -- sweeper "already announced?" check
CREATE INDEX outbox_published_idx ON outbox_events (published_at)
    WHERE published_at IS NOT NULL;                             -- retention purge
```

**`document_chunks`** (§8.6 without `tsv_content`):

```sql
CREATE TABLE document_chunks (
    id                uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id         uuid        NOT NULL,
    document_id       uuid        NOT NULL,
    chunk_index       integer     NOT NULL CHECK (chunk_index >= 0),
    content           text        NOT NULL CHECK (char_length(content) > 0),
    token_count       integer     NOT NULL CHECK (token_count > 0),
    embedding         vector(1536) NOT NULL,
    embedding_model   text        NOT NULL CHECK (char_length(embedding_model) BETWEEN 1 AND 200),
    embedding_version text        NOT NULL CHECK (char_length(embedding_version) BETWEEN 1 AND 64),
    metadata          jsonb       NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(metadata) = 'object'),
    created_at        timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT document_chunks_position_unique UNIQUE (tenant_id, document_id, chunk_index),
    CONSTRAINT document_chunks_document_fk FOREIGN KEY (tenant_id, document_id)
        REFERENCES documents (tenant_id, id) ON DELETE CASCADE
);
```

The unique constraint's index has `(tenant_id, document_id)` as its leading columns, so it also serves "all chunks of a document". No separate §9.1 `idx_chunks_tenant_document` is needed.

**Triggers.** `ingestion_jobs_set_updated_at` reuses `set_updated_at()`. Chunks are immutable, so they get no trigger.

**Grants**:

```sql
REVOKE ALL ON ingestion_jobs, outbox_events, document_chunks FROM PUBLIC;
GRANT SELECT, INSERT, UPDATE         ON ingestion_jobs  TO app_runtime;  -- history kept, no DELETE
GRANT SELECT, INSERT, DELETE         ON document_chunks TO app_runtime;  -- immutable, replace = delete+insert
GRANT SELECT, INSERT, UPDATE, DELETE ON outbox_events   TO app_runtime;  -- DELETE for retention purge
```

**RLS.** Four policies each on `ingestion_jobs` and `document_chunks`, copied from `documents` (`tenant_id = (SELECT app_tenant_id())`). **ENABLE only, no FORCE**, per D2, with a migration comment pointing at D2 and at `005`. `outbox_events` has no RLS (ADR-007 §3), with a comment that names the ADR.

**Backfill.** Documents uploaded during Phase 3 are sitting in `pending` with no job. The migration runs as the owner, so it can see every tenant:

```sql
WITH new_jobs AS (
    INSERT INTO ingestion_jobs (tenant_id, document_id, max_attempts)
    SELECT d.tenant_id, d.id, 5
      FROM documents d
     WHERE d.status = 'pending' AND d.deleted_at IS NULL
    RETURNING id, tenant_id, document_id
)
INSERT INTO outbox_events (tenant_id, aggregate_type, aggregate_id, event_type, payload)
SELECT tenant_id, 'ingestion_job', id, 'ingestion.requested',
       jsonb_build_object('v', 1, 'job_id', id, 'tenant_id', tenant_id, 'document_id', document_id)
  FROM new_jobs;
```

`5` is a literal because SQL cannot read the config, and a comment says so.

**Tests** (`tests/security/rls_policies.rs` style, plus a new `tests/security/cross_tenant_ingestion.rs`):

| Case | Expectation |
|---|---|
| Tenant B context selects A's jobs and chunks | 0 rows |
| B inserts a job or chunk with `tenant_id = A` | RLS violation |
| B updates or deletes A's rows | 0 rows affected |
| No tenant context at all | 0 rows, writes refused (fails closed) |
| Job whose `(tenant_id, document_id)` pair does not exist | FK violation (composite FK) |
| Second active job for the same document | unique violation on `ingestion_jobs_one_active` |
| `processing` with no lease, or `pending` with a lease | CHECK violation |
| `app_runtime` `UPDATE document_chunks` / `DELETE ingestion_jobs` | permission denied |
| Chunk with a 768-dimension vector | error `expected 1536 dimensions` |
| Backfill: a pre-existing `pending` document gets one job and one event | as stated |

**Done when** `sqlx migrate run` applies on an empty database and on a database at `003`, and the table above passes as `app_runtime`.

---

### T2: Enqueue on upload, chunk cleanup on delete (P0, ~2.5h)

**`repositories/ingestion_repo.rs`**, first function:

```rust
/// Inserts a pending job and its `ingestion.requested` outbox event.
/// Must run on the same tx as the document insert (§10.5).
pub async fn enqueue(tx: &mut Tx, tenant_id: Uuid, document_id: Uuid, max_attempts: u32)
    -> Result<Uuid /* job_id */, AppError>;
```

The function inserts the job `RETURNING id`, builds `IngestionEvent { v: 1, job_id, tenant_id, document_id }`, and calls `outbox_repo::insert(tx, Some(tenant_id), "ingestion_job", job_id, "ingestion.requested", &event)`. The event struct comes from `common-messaging` (T3). If T3 is not done yet, write the JSON with `serde_json::json!` and switch it over in T3.

**`document_service::upload`.** Only the `Created` arm changes. Restructure it so an enqueue failure takes the same compensating path as an insert failure:

```rust
let result: Result<InsertOutcome, AppError> = async {
    let outcome = document_repo::insert_pending(&mut tx, new).await?;
    if let InsertOutcome::Created(ref row) = outcome {
        ingestion_repo::enqueue(&mut tx, ctx.tenant_id(), row.id,
                                state.config.ingestion.job_max_attempts).await?;
    }
    Ok(outcome)
}.await;
match result {
    Ok(o)  => { tx.commit().await?; o }
    Err(e) => { drop(tx); /* existing delete-object + record_orphan path */ return Err(e); }
}
```

A duplicate upload enqueues nothing. The module rustdoc's "the transaction covers only `insert_pending`" becomes "covers `insert_pending` and `enqueue`, three INSERTs, no I/O".

**`document_service::delete`.** After the ownership check, before `tx.commit()`, call `chunk_repo::delete_for_document(&mut tx, id)`. Per D9 the job is not touched.

**Tests** (`tests/integration/ingestion_enqueue.rs`):

| Case | Expectation |
|---|---|
| Upload a TXT | 202. One job `pending`, `attempts = 0`, `max_attempts = 5`. One unpublished outbox row whose payload deserializes into `IngestionEvent` with the right ids |
| Upload with dead Redis | Still 202, same rows (exit criterion 1) |
| Duplicate upload | 200. Still one job and one event |
| Enqueue failure (see note) | 5xx. No document, no job, no object left in storage |
| Delete a document that has chunks (seed chunks through the owner pool) | Chunks gone in the same commit as `deleting` |
| All Phase 3 document tests | Pass unchanged |

Note on the failure case. Config validation rejects bad values such as `job_max_attempts = 0`, so the failure has to be injected at the database. Inside a test that holds `INGEST_GLOBAL`, the owner pool adds `ALTER TABLE outbox_events ADD CONSTRAINT test_block CHECK (false) NOT VALID`. The test runs the upload and then drops the constraint in a guard that runs even if the test panics. This case is P1: if it proves flaky in CI, drop it and rely on code review of the shared error path.

**Done when** the table passes and `cargo test --workspace` stays green.

---

### T3: `common-messaging`: event and Redis Streams client (P0, ~3h, no DB)

`Cargo.toml`: `redis` (workspace, with the G1 `streams` feature), `serde`, `serde_json`, `uuid`, `thiserror`, `tokio`, `tracing`. Dev: `tokio`, `dotenvy`.

**`event.rs`:**

```rust
pub const INGESTION_REQUESTED: &str = "ingestion.requested";
pub const EVENT_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestionEvent { pub v: u32, pub job_id: Uuid, pub tenant_id: Uuid, pub document_id: Uuid }

impl IngestionEvent {
    pub fn new(job_id: Uuid, tenant_id: Uuid, document_id: Uuid) -> Self;
    pub fn to_json(&self) -> String;
    /// Rejects unknown `v`, missing fields and non-UUID ids. Never panics.
    pub fn parse(raw: &str) -> Result<Self, EventError>;
}
```

`#[serde(deny_unknown_fields)]` is **not** used, so `v = 1` stays forward-compatible with added fields. Only `v` decides compatibility.

**`streams.rs`:**

```rust
pub struct StreamConfig { pub url: String, pub stream: String, pub group: String,
                          pub block_ms: u64, pub max_len: usize }

pub struct Delivery { pub stream_id: String, pub event: Result<IngestionEvent, EventError> }

pub struct StreamClient { cmd: ConnectionManager, reader: ConnectionManager, cfg: StreamConfig }

impl StreamClient {
    pub fn new_lazy(cfg: StreamConfig) -> Result<Self, StreamError>;           // two managers (D6)
    pub async fn ensure_group(&self) -> Result<(), StreamError>;              // XGROUP CREATE .. 0 MKSTREAM; BUSYGROUP = Ok
    pub async fn publish(&self, event: &IngestionEvent, outbox_id: Uuid) -> Result<String, StreamError>;
        // XADD stream MAXLEN ~ max_len * event <json> outbox_id <uuid>
    pub async fn read(&self, consumer: &str, count: usize) -> Result<Vec<Delivery>, StreamError>;
        // XREADGROUP GROUP g consumer COUNT n BLOCK block_ms STREAMS stream >
    pub async fn ack(&self, ids: &[String]) -> Result<(), StreamError>;
    pub async fn reclaim(&self, consumer: &str, min_idle_ms: u64, cursor: &str, count: usize)
        -> Result<(String /* next cursor */, Vec<Delivery>), StreamError>;   // XAUTOCLAIM
    pub async fn stats(&self) -> Result<StreamStats, StreamError>;            // XLEN + XPENDING summary
    pub async fn health(&self) -> bool;                                       // PING
}
```

Error mapping: I/O, timeout and connection-refused errors become `StreamError::Unavailable`. Anything else is `Protocol`. A missing `event` field or invalid JSON produces a `Delivery` with `event: Err(..)` rather than failing the whole read, so one bad message cannot block the stream.

`max_len` is a constant `100_000`, not new config. `block_ms` is a constant `2000`.

**Tests** (real Redis, unique stream key per test, skipped with a message when `REDIS_URL` is unreachable, as the DB tests do):

| Case | Expectation |
|---|---|
| `IngestionEvent` round trip, unknown `v = 2`, missing field, extra field, bad UUID | Ok / `UnsupportedVersion` / `Malformed` / Ok / `Malformed` |
| `ensure_group` twice | Both Ok |
| Publish before `ensure_group`, then create the group, then read | Message delivered (start id `0`) |
| Publish → read → ack → read | Second read is empty |
| Read without ack, `reclaim` with `min_idle_ms = 0` from another consumer | Same message returned |
| Raw `XADD` of garbage | `Delivery.event` is `Err`, other messages in the batch are fine |
| `BLOCK` read on an empty stream | Returns empty after ~2 s, **not** a timeout error (proves D6) |
| Client pointed at `redis://127.0.0.1:1` | `Unavailable`, and `health()` is false, both within 3 s |

**Done when** `cargo test -p common-messaging` passes locally with Redis and skips cleanly without it.

---

### T4: Outbox publisher (P0, ~3h, needs T1, T3)

**`repositories/outbox_repo.rs`** (raw `&PgPool`, no tenant context, like `storage_orphans`):

```rust
pub async fn insert(tx: &mut Tx, tenant_id: Option<Uuid>, aggregate_type: &str, aggregate_id: Uuid,
                    event_type: &str, payload: &serde_json::Value) -> Result<Uuid, AppError>;
pub async fn claim_batch(pool: &PgPool, limit: i64, claim_secs: u64) -> Result<Vec<OutboxRow>, AppError>; // ADR-007 §1 SQL
pub async fn mark_published(pool: &PgPool, ids: &[Uuid]) -> Result<(), AppError>;
pub async fn release(pool: &PgPool, ids: &[Uuid]) -> Result<(), AppError>;     // claimed_until = NULL
pub async fn dead_letter(pool: &PgPool, id: Uuid, err: &str) -> Result<(), AppError>;
pub async fn purge_published(pool: &PgPool, older_than_days: i32, limit: i64) -> Result<u64, AppError>;
pub async fn backlog(pool: &PgPool) -> Result<(i64 /* count */, Option<f64> /* oldest age secs */), AppError>;
```

`claim_batch` is a single auto-committed statement (`UPDATE ... WHERE id IN (SELECT ... FOR UPDATE SKIP LOCKED) RETURNING`). It needs no explicit transaction, and its row locks are released when the statement ends. That is the whole point of ADR-007 §1.

**`workers/outbox_publisher.rs`:**

```rust
pub struct PublishReport { pub published: usize, pub dead_lettered: usize, pub released: usize }

/// One claim → publish → mark cycle. Returns how many rows it handled.
pub async fn publish_batch(pool: &PgPool, streams: &StreamClient, cfg: &IngestionConfig)
    -> Result<PublishReport, PublisherError>;

/// Loop: publish_batch; sleep OUTBOX_POLL_MS if it handled 0 rows, else loop at once;
/// on PublisherError::RedisUnavailable sleep with backoff (200ms → 5s). Exits on cancel.
pub async fn run(pool: PgPool, streams: Arc<StreamClient>, cfg: IngestionConfig, cancel: CancellationToken);
```

Inside `publish_batch`, for each row in order:

1. `IngestionEvent::parse(payload)`. On `Err`, dead-letter the row and continue (D5).
2. `streams.publish(...)`. On `Unavailable`, release **this and every remaining** id, return `RedisUnavailable`. Already published ids are still marked first.
3. Collect successes and call `mark_published` once at the end.

A crash between step 2 and step 3 leaves rows claimed but unmarked. Their claim expires after `OUTBOX_CLAIM_SECS` and they are republished. That is the documented duplicate.

**Tests** (`tests/integration/outbox_publisher.rs`, all holding `INGEST_GLOBAL`, D12):

| Case | Expectation |
|---|---|
| Upload, then drain | Own event appears exactly once in the test stream, row has `published_at` |
| Two `publish_batch` calls running concurrently over 30 own events | Each event published exactly once in the happy path (SKIP LOCKED) |
| **Crash simulation**: `claim_batch` + `publish`, no mark; age `claimed_until` into the past; drain | Event appears **twice** in the stream. The row is published once in the DB. The T9 consumer test later proves the duplicate is harmless |
| Redis dead (`StreamClient` on port 1) | `RedisUnavailable`, rows released (`claimed_until IS NULL`), `dead_lettered_at IS NULL`, `published_at IS NULL` |
| Same rows, Redis back | Drained |
| Row with payload `{"v": 99}` inserted through the owner pool | Dead-lettered, `last_error` set, other rows published |
| `purge_published` | Deletes only published rows older than the cutoff |

**Done when** the table passes and no code path holds a transaction during `publish`.

---

### T5: Worker binary, supervisor, claim and lease (P0, ~4h, needs T1, T3)

**`common-database::begin_system_tenant_tx`:**

```rust
/// Worker-only: starts a transaction scoped to `tenant_id` with no user.
///
/// The tenant id MUST come from an `ingestion_jobs` row or a validated
/// `IngestionEvent`, never from an HTTP request. RLS still applies, so an
/// event carrying the wrong tenant simply matches zero rows.
/// A CI grep guard limits callers to `services/ingestion_service.rs` and `workers/`.
pub async fn begin_system_tenant_tx(pool: &PgPool, tenant_id: Uuid) -> Result<Tx, AppError>;
```

It sets only `app.current_tenant_id` with `is_local = true`. Unit tests mirror the existing four tx tests: the setting is visible inside, cleared after commit, cleared after rollback, cleared after drop, and `app.current_user_id` is empty.

**`domain/ingestion.rs`:** `JobStatus` (as_str, FromStr, `all_str` diffed against the SQL CHECK, `can_transition_to`), `FailureKind`, `FailureCode` (D13), and the pure backoff function:

```rust
/// Exponential backoff with "equal jitter": half fixed, half random.
/// attempt 1 → base, 2 → 2·base, ... capped at max. `rand01` is injected for tests.
pub fn retry_delay(attempt: u32, base: Duration, max: Duration, rand01: f64) -> Duration;
```

**`repositories/ingestion_repo.rs`**, claim side. Lock order per D4:

```rust
pub struct ClaimedJob { pub job_id: Uuid, pub tenant_id: Uuid, pub document_id: Uuid,
                        pub attempts: i32, pub max_attempts: i32, pub lease_token: Uuid,
                        pub object_key: String, pub mime_type: String,
                        pub size_bytes: i64, pub content_hash: String }

pub enum ClaimResult { Claimed(ClaimedJob), NotClaimable, DocumentGone, Mismatch }

pub async fn claim(tx: &mut Tx, job_id: Uuid, document_id: Uuid, worker_id: &str, lease_secs: u64)
    -> Result<ClaimResult, AppError>;
```

Inside `claim`, in order:

1. `SELECT status, deleted_at, object_key, mime_type, size_bytes, content_hash FROM documents WHERE id = $doc FOR UPDATE`. No row (RLS hides it, or it does not exist) → `Mismatch`.
2. The claim itself:

   ```sql
   UPDATE ingestion_jobs
      SET status = 'processing', attempts = attempts + 1,
          lease_token = gen_random_uuid(), locked_by = $2,
          locked_until = now() + make_interval(secs => $3)
    WHERE id = $1 AND document_id = $4
      AND attempts < max_attempts
      AND (status = 'pending' OR (status = 'processing' AND locked_until < now()))
   RETURNING id, tenant_id, document_id, attempts, max_attempts, lease_token
   ```

   No row → `NotClaimable`.
3. If the document has `deleted_at IS NOT NULL`, set the job to `cancelled` with the lease cleared → `DocumentGone`.
4. `UPDATE documents SET status = 'processing', failure_code = NULL, failure_detail = NULL WHERE id = $doc`.

```rust
/// Pushes locked_until forward. Single-row update, no document lock (D4-safe).
pub async fn extend_lease(pool: &PgPool, tenant_id: Uuid, job_id: Uuid, lease: Uuid, lease_secs: u64)
    -> Result<bool /* still ours */, AppError>;
```

**`src/bin/worker.rs`** (new `[[bin]] name = "tenant-saas-worker"` in `apps/tenant-saas/Cargo.toml`):

```rust
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = AppConfig::load()?;
    common_telemetry::init_tracing(&config.app.rust_log);
    // Never runs migrations: the API (dev) or the migration job (prod) owns that.
    let db = common_database::create_pool(&config.database).await?;
    let mut state = AppState::new(db, Arc::new(config.clone()))?;
    if let Some(url) = config.database.migration_url.as_deref() {        // reconciler only
        if let Ok(p) = PgPool::connect(url).await { state = state.with_owner_pool(p); }
    }
    let streams = Arc::new(StreamClient::new_lazy(stream_config(&config))?);
    let cancel = CancellationToken::new();
    tokio::spawn(shutdown_signal(cancel.clone()));                         // ctrl_c + SIGTERM on unix
    workers::supervisor::run(state, streams, cancel).await
}
```

It warns at startup when `DATABASE_MAX_CONNECTIONS < WORKER_CONCURRENCY + 3` (publisher, sweeper, reconciler).

**`workers/supervisor.rs`:**

```rust
/// Runs `make()` in a loop. A task that returns or panics is logged and restarted
/// after 1 s, unless `cancel` fired. This is what §5.3 calls "supervised".
async fn supervise<F, Fut>(name: &'static str, cancel: CancellationToken, make: F)
where F: Fn() -> Fut, Fut: Future<Output = ()> + Send + 'static;

pub async fn run(state: AppState, streams: Arc<StreamClient>, cancel: CancellationToken) -> anyhow::Result<()> {
    // ensure_group (retry until Redis is reachable or cancel)
    // join!( supervise("outbox_publisher"), supervise("ingestion_consumer"),
    //        supervise("sweeper"), supervise("storage_reconciler"), heartbeat )
    // after cancel: wait up to SHUTDOWN_GRACE (30 s, constant) for in-flight jobs
}
```

The consumer and sweeper loops are stubs in this task (they log and sleep) and are filled in by T9 and T10. The storage reconciler task calls the existing `storage_reconciler::reconcile(&state, false)` every 10 minutes (a constant), which completes the Phase 3 handoff. The worker id is `"{hostname}:{pid}:{8 hex}"` and is used as both the Redis consumer name and `locked_by`.

**Tests:**

| Case | Expectation |
|---|---|
| `JobStatus` 6×6 transition table | Exactly the §3 edges allowed |
| `JobStatus::all_str` vs the SQL CHECK text | Equal |
| `retry_delay` for attempts 1..10 with `rand01` = 0, 0.5, 1 | In `[d/2, d]`, monotonic in attempt, never above max |
| `FailureCode::as_str` for every variant | Matches the `failure_code` regex |
| `claim` on a pending job | `Claimed`, attempts 1, document `processing` |
| Two concurrent `claim`s on one job | Exactly one `Claimed`, one `NotClaimable` |
| `claim` on `processing` with expired lease | `Claimed` with a **new** lease token |
| `claim` on `retry_scheduled` | `NotClaimable` (D1) |
| `claim` at `attempts = max_attempts` | `NotClaimable` |
| `claim` with tenant B's tx for A's job | `Mismatch`, A's rows unchanged |
| `claim` on a deleted document | `DocumentGone`, job `cancelled` |
| `extend_lease` with a stale token | `false`, `locked_until` unchanged |
| `supervise` with a task that panics twice | Restarted twice, then exits on cancel |
| `begin_system_tenant_tx` | The four setting tests, plus a `pool_reuse.rs` case |

**Done when** `cargo run --bin tenant-saas-worker` starts, logs a heartbeat, and stops cleanly on Ctrl+C.

---

### T6: Parser and normalizer (P0, ~4h, pure)

**`ingestion/parser.rs`:**

```rust
pub struct Section { pub text: String, pub page: Option<u32>, pub heading: Option<String> }
pub struct ParsedDocument { pub sections: Vec<Section>, pub char_count: usize }

pub struct ParseLimits { pub max_pdf_pages: u32, pub max_chars: usize }

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("unsupported format")]      Unsupported,
    #[error("document too large")]      TooLarge,
    #[error("document is empty")]       Empty,
    #[error("corrupt document: {0}")]   Corrupt(&'static str),   // static text only, never parser output
}

/// Synchronous and CPU-bound. The service runs it inside spawn_blocking.
pub fn parse(bytes: &[u8], mime_type: &str, limits: &ParseLimits) -> Result<ParsedDocument, ParseError>;
```

| Format | Rule |
|---|---|
| `text/plain` | `std::str::from_utf8`. Phase 3 already validated it, so failure → `Corrupt("invalid utf-8")`. One section, no page |
| `text/markdown` | Same decode. Split into sections at ATX headings (`^#{1,6} `), keeping the heading line inside the section text and in `heading`. Fenced code blocks (```` ``` ````) are tracked so a `#` inside code is not a heading. Markdown syntax is **not** stripped, so tables and code survive into chunks |
| `application/pdf` | `pdf_extract::extract_text_from_mem_by_pages(bytes)` → one `Section` per page with `page: Some(n)`. Page count `> max_pdf_pages` → `TooLarge` (checked after extraction because pdf-extract has no cheap page-count API; the timeout bounds the cost). Any `OutputError` → `Corrupt("pdf parse error")`. Encrypted PDFs fail here too |
| other | `Unsupported` |

After extraction: total chars `> max_chars` → `TooLarge`. Everything whitespace-only after normalization → `Empty`. A scanned PDF with no text layer produces `Empty`, and that is the documented behavior (no OCR).

**`ingestion/normalize.rs`** (applied per section):

1. `\r\n` and `\r` become `\n`.
2. Remove control characters except `\n` and `\t`. NUL and `U+FEFF` go.
3. Trim trailing whitespace on each line.
4. Collapse 3 or more consecutive newlines to 2.
5. PDF only: join a hyphenated line break (`exam-\nple` → `example`) when both sides are lowercase letters.
6. Trim the section.

There is no Unicode NFC step. It would add a dependency for an effect nobody has measured, and Phase 5's relevance evaluation can justify adding it.

**Running it.** `ingestion_service` (T9) calls `parse` like this:

```rust
let permit = deps.parse_slots.clone().acquire_owned().await?;          // Semaphore(max(1, concurrency/2))
let handle = tokio::task::spawn_blocking(move || { let _p = permit; parse(&bytes, &mime, &limits) });
match tokio::time::timeout(parse_timeout, handle).await {
    Err(_)                       => /* ParseTimeout, transient */,
    Ok(Err(join)) if join.is_panic() => /* ParseFailed, permanent */,
    Ok(Err(_))                   => /* Internal */,
    Ok(Ok(r))                    => r,
}
```

The permit moves **into** the closure. A blocking thread cannot be cancelled, so after a timeout it keeps running until it finishes, and keeping the permit until then stops timed-out parses from piling up. An out-of-memory abort kills the whole process. Then the lease expires, the sweeper retries the job, and after `max_attempts` crashes it ends `failed` with `ATTEMPTS_EXHAUSTED`. A poison PDF is therefore contained by D1 without special code. The runbook says this.

**Fixtures** in `apps/tenant-saas/tests/fixtures/ingestion/` (small, committed): `simple.txt`, `headings.md` (with a `#` inside a code fence), `two_pages.pdf`, `empty_text_layer.pdf`, `corrupt.pdf` (valid header, garbage body), `crlf.txt`, `bom_control_chars.txt`.

**Tests** (unit, no I/O): one per fixture, plus `max_chars` exceeded → `TooLarge`, `max_pdf_pages = 1` on the two-page PDF → `TooLarge`, unknown mime → `Unsupported`, every normalize rule in isolation, and normalize is idempotent (applying it twice gives the same output).

**Done when** `cargo test -p tenant-saas ingestion::parser` and `ingestion::normalize` pass with no database.

---

### T7: Token-aware chunker (P0, ~4h, pure)

```rust
pub const EMBEDDING_VERSION: &str = "p4-v1";   // parser + normalizer + chunker version, stored per chunk

pub struct ChunkerConfig { pub target_tokens: usize, pub overlap_tokens: usize, pub max_chunks: usize }

pub struct Chunk { pub index: u32, pub content: String, pub token_count: u32,
                   pub page_start: Option<u32>, pub page_end: Option<u32>, pub heading: Option<String> }

#[derive(Debug, thiserror::Error)]
pub enum ChunkError { #[error("too many chunks")] TooMany, #[error("no content")] Empty }

pub struct Chunker { bpe: Arc<CoreBPE>, cfg: ChunkerConfig }
impl Chunker {
    pub fn new(bpe: Arc<CoreBPE>, cfg: ChunkerConfig) -> Self;
    pub fn chunk(&self, doc: &ParsedDocument) -> Result<Vec<Chunk>, ChunkError>;
}

/// Encoding bound to the embedding model (§10.8). text-embedding-3-* and ada-002 → cl100k_base.
/// Unknown model → cl100k_base plus a startup WARN that token counts are approximate.
pub fn tokenizer_for_model(model: &str) -> anyhow::Result<CoreBPE>;
```

**Algorithm.** Everything is string-level except the hard split, so a chunk can never cut a UTF-8 character:

1. **Pieces.** Each section splits into paragraphs on blank lines. A heading paragraph (markdown `#` line) merges with the paragraph after it, so a heading never ends a chunk on its own. Each piece remembers its section's `page` and `heading`.
2. **Oversize pieces.** A paragraph whose token count exceeds `target` splits into sentences: break after `. `, `? `, `! ` or a single `\n`. A sentence still over `target` gets a **hard split**: encode, take windows of `target` tokens, `decode`, and if `decode` fails (the window ends mid-character) shrink the window by one token and retry, up to 4 times.
3. **Packing.** Pieces are added greedily. Before adding one, count tokens of `current + separator + piece` (separator `\n\n` between paragraphs, ` ` between sentences). If it would exceed `target`, close the current chunk. Re-counting the joined string matters because BPE counts are not additive across the join.
4. **Overlap.** A new chunk starts with the trailing pieces of the previous chunk whose total is at most `overlap` tokens. If the last piece alone is bigger than `overlap`, the new chunk has no overlap. That is preferable to cutting mid-sentence.
5. **Metadata.** `page_start` and `page_end` are the min and max pages of the pieces in the chunk. `heading` is the first heading seen. Chunks may span pages and sections.
6. **Guards.** Whitespace-only chunks are dropped. `chunks.len() > max_chunks` → `TooMany` (D10). Zero chunks → `Empty`.

Chunking runs inside `spawn_blocking` in T9 because tokenizing 5M characters takes noticeable CPU.

**Tests:**

| Case | Expectation |
|---|---|
| 10k-word text, target 500 / overlap 75 | Every `token_count ≤ 500` and equals a fresh re-count of `content` |
| Same input | Neighbouring chunks share their overlap text, and overlap is ≤ 75 tokens |
| Run twice | Byte-identical output (determinism, needed for idempotency) |
| CJK text, emoji, combining marks, each at many lengths | Every chunk is valid UTF-8 and no character is lost or duplicated outside the overlaps |
| One 3000-token "sentence" with no punctuation | Hard split, every piece ≤ target |
| Markdown heading then paragraph exactly at a chunk boundary | Heading stays with its paragraph |
| Two-page PDF fixture | `page_start`/`page_end` correct |
| `max_chunks = 3` on a large input | `TooMany` |
| Only whitespace | `Empty` |
| `tokenizer_for_model("text-embedding-3-small")` | cl100k_base, and `"hello world"` encodes to the known 2 tokens |

**Done when** all pass with no database.

---

### T8: Embedding provider (P0, ~4h, no DB)

**`ai/embedding.rs`:**

```rust
#[derive(Debug, thiserror::Error)]
pub enum EmbeddingError {
    #[error("embedding provider unavailable: {reason}")]
    Transient { reason: String, retry_after: Option<Duration> },
    #[error("embedding request rejected: {reason}")]
    Permanent { reason: String },
    #[error("invalid embedding response: {reason}")]
    InvalidResponse { reason: String },
}

#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    fn model(&self) -> &str;
    fn dim(&self) -> usize;
    async fn embed(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError>;
}

/// Count matches inputs, every vector has `dim` entries, every value is finite, no all-zero vector.
pub fn validate_vectors(inputs: usize, dim: usize, vectors: &[Vec<f32>]) -> Result<(), EmbeddingError>;

/// Sequential batches of `batch_size`. Calls `on_batch` after each one (used for lease extension).
pub async fn embed_in_batches<F, Fut>(p: &dyn EmbeddingProvider, inputs: &[String], batch_size: usize, on_batch: F)
    -> Result<Vec<Vec<f32>>, EmbeddingError>
where F: FnMut(usize) -> Fut, Fut: Future<Output = ()>;
```

`reason` strings are built by our code and never include a response body (D13).

**`ai/fake.rs`.** Deterministic and offline. For each input, `seed = sha256(model || 0x00 || text)`. Expand the seed with `sha256(seed || i)` for `i in 0..ceil(dim/8)`, map each 4-byte group to `[-1, 1]`, then L2-normalize. Same text gives the same vector, different texts give near-orthogonal vectors, and nothing has any semantic meaning. The rustdoc says so plainly, because Phase 5 relevance numbers measured with the fake would be meaningless.

**`ai/openai.rs`.** `reqwest::Client` is built once with `timeout(embedding_timeout_secs)` and `connect_timeout(5s)`.

- `POST {base_url or https://api.openai.com/v1}/embeddings` with bearer auth and body `{"model", "input": [...], "encoding_format": "float"}`.
- Response size cap: reject `Content-Length > inputs × dim × 24 + 1 MiB`. Without a length header, read with `Response::chunk()` and stop at the same cap. That needs no extra reqwest feature.
- Parse `{"data": [{"index", "embedding"}], "usage": {"prompt_tokens"}}`. **Sort by `index`** and require indices exactly `0..n`. Providers do not promise order.
- Log `prompt_tokens` at debug as a `usage_tokens` field, so Phase 7 usage records can pick it up.

| HTTP / error | Mapping |
|---|---|
| 200 with a valid body | Ok, after `validate_vectors` |
| 200 with a bad body, wrong count or dimension, NaN | `InvalidResponse` |
| 429 | `Transient`, `retry_after` from the `Retry-After` header (seconds) |
| 500, 502, 503, 504, timeout, connect error | `Transient` |
| 401, 403 | `Transient` plus an `error` log naming `EMBEDDING_API_KEY` (D13) |
| other 4xx | `Permanent` |

**`ai/mod.rs`:** `build_embedder(&IngestionConfig) -> Result<Arc<dyn EmbeddingProvider>, AppError>`. `AppState::new` calls it, and `with_embedder` overrides it (D11).

**Tests:**

| Case | Expectation |
|---|---|
| Fake: same text twice | Identical vectors |
| Fake: two texts | Different vectors, cosine < 0.2 |
| Fake: any text | Norm 1 ± 1e-5, length 1536 |
| `validate_vectors` | Rejects wrong count, wrong dim, NaN, Inf, all-zero |
| `embed_in_batches` with 70 inputs, batch 32 | 3 calls of 32/32/6, order preserved, `on_batch` called 3 times |
| OpenAI against a local axum mock server (`tokio::net::TcpListener` on port 0) | One test per mapping row above, plus shuffled `index` order |
| OpenAI mock body over the cap | `InvalidResponse`, memory stays bounded |
| `Authorization` header | Present in the request. The key never appears in `captured_logs()` |

**Done when** all pass offline.

---

### T9: Ingestion pipeline and idempotent persistence (P0, ~5h, needs T1, T3, T5–T8)

**`repositories/chunk_repo.rs`:**

```rust
pub struct NewChunk<'a> { pub index: i32, pub content: &'a str, pub token_count: i32,
                          pub embedding: &'a [f32], pub metadata: serde_json::Value }

/// Deletes all chunks of the document, then inserts `chunks` in UNNEST batches of 200 (D8).
pub async fn replace_for_document(tx: &mut Tx, tenant_id: Uuid, document_id: Uuid,
                                  model: &str, version: &str, chunks: &[NewChunk<'_>]) -> Result<(), AppError>;
pub async fn delete_for_document(tx: &mut Tx, document_id: Uuid) -> Result<u64, AppError>;
pub async fn count_for_document(tx: &mut Tx, document_id: Uuid) -> Result<i64, AppError>;
fn vector_literal(v: &[f32]) -> String;   // "[a,b,...]"
```

**`ingestion_repo::complete`** (tx B, D3/D4 order):

```sql
-- 1. document first: must still be processing and live
SELECT 1 FROM documents WHERE id = $doc AND status = 'processing' AND deleted_at IS NULL FOR UPDATE;
-- 2. lease check on the job
UPDATE ingestion_jobs SET status = 'succeeded', lease_token = NULL, locked_until = NULL,
       locked_by = NULL, completed_at = now(), last_error_code = NULL
 WHERE id = $job AND lease_token = $lease AND status = 'processing';
-- 0 rows in step 1 or 2 → caller rolls back → Superseded
-- 3. chunk_repo::replace_for_document(...)
-- 4. UPDATE documents SET status = 'ready', chunk_count = $n, failure_code = NULL, failure_detail = NULL WHERE id = $doc;
```

**`ingestion_repo::mark_failed`** (permanent here; T10 adds `schedule_retry`): same lock order and lease check, then job `failed` with `last_error_code`/`last_error_at`/`completed_at`, and document `failed` with `failure_code`/`failure_detail`.

**`services/ingestion_service.rs`:**

```rust
pub struct IngestionDeps {
    pub state: AppState,                       // db, storage, embedder, config
    pub chunker: Arc<Chunker>,
    pub parse_slots: Arc<Semaphore>,
    pub worker_id: String,
}

pub enum ProcessOutcome {
    Succeeded { chunks: usize }, RetryScheduled { at: DateTime<Utc> }, Failed { code: FailureCode },
    Cancelled, Skipped, Superseded, Poison, Unavailable,
}
impl ProcessOutcome { pub fn should_ack(&self) -> bool; }       // D7 table

#[tracing::instrument(skip(deps), fields(job_id = %event.job_id, tenant_id = %event.tenant_id,
                                         document_id = %event.document_id, attempt))]
pub async fn process(deps: &IngestionDeps, event: &IngestionEvent) -> ProcessOutcome;
```

`process` step by step. Every arrow marked "no tx" means no connection is held:

1. **Claim** (tx A via `begin_system_tenant_tx(event.tenant_id)`): `Mismatch` → `Poison`. `NotClaimable` → `Skipped`. `DocumentGone` → `Cancelled`. A DB connection error → `Unavailable`. Commit.
2. **Download** (no tx): if `size_bytes > MAX_UPLOAD_BYTES` → permanent `DocumentTooLarge`. `storage.get(object_key)`: `NotFound` → permanent `ObjectMissing`, `Unavailable` → transient. Then verify `len == size_bytes` and `sha256 == content_hash`, otherwise permanent `ObjectCorrupt`. This catches a key that points at the wrong bytes for the cost of one hash over a buffer already in memory.
3. **Parse and normalize** (spawn_blocking, T6 pattern).
4. **Chunk** (spawn_blocking).
5. **Embed** (no tx): `embed_in_batches`. After each batch, `extend_lease`. If it returns `false`, stop and return `Superseded` without calling the provider again.
6. **Persist** (tx B, `complete`). Superseded → rollback → `Superseded`.
7. Any classified failure in 2–5 → `fail(deps, claimed, failure)`. In T9 that always means `mark_failed`. T10 makes the transient branch real.

Every step logs one line at `info` with `duration_ms`. The final line carries `outcome`, `chunks`, `attempt` and `duration_ms`. No log line contains document text, file names or object keys.

**`workers/ingestion_consumer.rs`:**

```rust
pub async fn run(deps: Arc<IngestionDeps>, streams: Arc<StreamClient>, cancel: CancellationToken) {
    let slots = Arc::new(Semaphore::new(cfg.worker_concurrency));
    loop {
        // wait for at least one free slot, or cancel
        // read(consumer, free_slots) — on Unavailable: sleep with backoff
        // for each Delivery: Err(event) → ack + warn (Poison)
        //                    Ok(event)  → spawn(permit) { outcome = process(); if should_ack { ack } }
        // every 30 s: reclaim(min_idle = JOB_LEASE_SECS·1000) and feed results through the same path
    }
}
```

**Tests** (`tests/integration/ingestion_pipeline.rs`). They use the memory storage, the fake embedder (or a `ScriptedEmbedder` from `tests/common`) and the real DB, and call `process` directly:

| Case | Expectation |
|---|---|
| TXT, MD, PDF happy paths | `Succeeded`. Document `ready`, `chunk_count = n`, `n` rows, `vector_dims = 1536`, `embedding_model` and `embedding_version` set, `chunk_index` 0..n-1 |
| **Same event twice, sequential** | Second is `Skipped`. Chunk ids unchanged |
| **Same event, two concurrent tasks** | One `Succeeded`, one `Skipped`. Chunks = `chunk_count` |
| **Duplicate after a forced reprocess**: succeed, then reset the job to `pending` through the owner pool, process again | Chunks replaced, still `n` rows, `UNIQUE` never violated |
| **Stale worker**: A claims; expire A's lease; B claims and completes; A calls `complete` with its token | A `Superseded`, B's chunk ids survive |
| Lease lost during embedding (`ScriptedEmbedder` expires the lease from inside its `embed` call) | `Superseded`, no further provider calls |
| Document deleted after claim, before persist | `Superseded` at persist, zero chunks, document stays `deleting`/`deleted` |
| Event with B's tenant and A's job | `Poison`, nothing changed, one `warn` line |
| Object removed from storage | `Failed { ObjectMissing }` |
| Object bytes replaced | `Failed { ObjectCorrupt }` |
| Corrupt PDF fixture | `Failed { ParseFailed }`. Document `failed`, safe `failure_detail` |
| Empty-text-layer PDF | `Failed { DocumentEmpty }` |
| `ScriptedEmbedder` returns a wrong dimension | `Failed { EmbeddingInvalid }` |
| `captured_logs()` after the suite | Contains no object key and no fixture sentence |
| Consumer end to end: spawn the consumer task on a test stream, publish the event, wait | Document `ready` within 10 s, message acked (`XPENDING` = 0) |

**Done when** every row passes. The duplicate and stale-worker rows prove exit criterion 3.

---

### T10: Retry, sweeper, recovery and the retry endpoint (P0, ~5h, needs T9)

**Transient failures in the worker.** `ingestion_repo::schedule_retry` uses the same lock order and lease check as `mark_failed`:

```sql
UPDATE ingestion_jobs SET status = 'retry_scheduled', lease_token = NULL, locked_until = NULL,
       locked_by = NULL, available_at = now() + make_interval(secs => $delay),
       last_error_code = $code, last_error_at = now()
 WHERE id = $job AND lease_token = $lease AND status = 'processing';
UPDATE documents SET status = 'retry_scheduled', failure_code = $code, failure_detail = $detail WHERE id = $doc;
```

`delay = max(retry_delay(attempts, base, max, rand), retry_after)`. When `attempts >= max_attempts`, the service calls `mark_failed` instead with the original code. The document then shows *why* it failed rather than a generic `ATTEMPTS_EXHAUSTED`.

**Migration `005_ingestion_sweeper.sql`.** A definer function in the `002` template:

```sql
CREATE FUNCTION sweep_ingestion_jobs(p_batch integer, p_pending_stale_secs integer)
RETURNS TABLE (job_id uuid, tenant_id uuid, action text)
LANGUAGE plpgsql VOLATILE SECURITY DEFINER
SET search_path = pg_catalog, public
AS $$
#variable_conflict use_column
BEGIN
    -- Rule 0: active jobs whose document was deleted -> cancelled
    RETURN QUERY
    WITH c AS (
        SELECT j.id FROM ingestion_jobs j
          JOIN documents d ON d.tenant_id = j.tenant_id AND d.id = j.document_id
         WHERE j.status IN ('pending', 'processing', 'retry_scheduled') AND d.deleted_at IS NOT NULL
         LIMIT p_batch FOR UPDATE OF j SKIP LOCKED),
    jc AS (UPDATE ingestion_jobs j SET status = 'cancelled', lease_token = NULL, locked_until = NULL,
                  locked_by = NULL, completed_at = now()
             FROM c WHERE j.id = c.id RETURNING j.id, j.tenant_id)
    SELECT jc.id, jc.tenant_id, 'cancelled'::text FROM jc;

    -- Rule A: expired lease, attempts exhausted -> failed (job and document)
    RETURN QUERY
    WITH c AS (
        SELECT j.id, j.tenant_id, j.document_id FROM ingestion_jobs j
          JOIN documents d ON d.tenant_id = j.tenant_id AND d.id = j.document_id
         WHERE j.status = 'processing' AND j.locked_until < now()
           AND j.attempts >= j.max_attempts AND d.deleted_at IS NULL
         ORDER BY j.locked_until LIMIT p_batch
           FOR UPDATE OF j, d SKIP LOCKED),
    jf AS (UPDATE ingestion_jobs j SET status = 'failed', lease_token = NULL, locked_until = NULL,
                  locked_by = NULL, last_error_code = 'ATTEMPTS_EXHAUSTED',
                  last_error_at = now(), completed_at = now()
             FROM c WHERE j.id = c.id RETURNING j.id, j.tenant_id),
    df AS (UPDATE documents d SET status = 'failed', failure_code = 'ATTEMPTS_EXHAUSTED',
                  failure_detail = 'worker did not finish within the lease on the final attempt'
             FROM c WHERE d.tenant_id = c.tenant_id AND d.id = c.document_id)
    SELECT jf.id, jf.tenant_id, 'failed'::text FROM jf;

    -- Rule B: expired lease with attempts left, due retry, or stale pending -> pending + new event
    RETURN QUERY
    WITH c AS (
        SELECT j.id, j.tenant_id, j.document_id, (j.status = 'processing') AS was_leased
          FROM ingestion_jobs j
          JOIN documents d ON d.tenant_id = j.tenant_id AND d.id = j.document_id
         WHERE d.deleted_at IS NULL
           AND (   (j.status = 'processing'      AND j.locked_until < now())
                OR (j.status = 'retry_scheduled' AND j.available_at <= now())
                OR (j.status = 'pending'         AND j.last_enqueued_at
                                                     < now() - make_interval(secs => p_pending_stale_secs)))
           AND NOT EXISTS (SELECT 1 FROM outbox_events o
                            WHERE o.aggregate_id = j.id
                              AND o.published_at IS NULL AND o.dead_lettered_at IS NULL)
         LIMIT p_batch FOR UPDATE OF j, d SKIP LOCKED),
    jr AS (UPDATE ingestion_jobs j SET status = 'pending', lease_token = NULL, locked_until = NULL,
                  locked_by = NULL, last_enqueued_at = now()
             FROM c WHERE j.id = c.id RETURNING j.id, j.tenant_id, j.document_id),
    dr AS (UPDATE documents d SET status = 'retry_scheduled'
             FROM c WHERE c.was_leased AND d.tenant_id = c.tenant_id AND d.id = c.document_id),
    ob AS (INSERT INTO outbox_events (tenant_id, aggregate_type, aggregate_id, event_type, payload)
           SELECT jr.tenant_id, 'ingestion_job', jr.id, 'ingestion.requested',
                  jsonb_build_object('v', 1, 'job_id', jr.id, 'tenant_id', jr.tenant_id,
                                     'document_id', jr.document_id)
             FROM jr)
    SELECT jr.id, jr.tenant_id, 'requeued'::text FROM jr;
END $$;

REVOKE EXECUTE ON FUNCTION sweep_ingestion_jobs(integer, integer) FROM PUBLIC;
GRANT  EXECUTE ON FUNCTION sweep_ingestion_jobs(integer, integer) TO app_runtime;
```

Notes the implementation must keep:

- Each rule is a CTE chain ending in a `SELECT`, because `RETURN QUERY` needs a query. Data-modifying CTEs run even when the final `SELECT` does not reference them. `#variable_conflict use_column` stops the `RETURNS TABLE` names (`tenant_id`, `job_id`) from shadowing columns.
- Rule A runs before Rule B in the same call, so a job at its final attempt is failed, not requeued.
- `FOR UPDATE OF j, d SKIP LOCKED` means the sweeper never waits, so it cannot deadlock with a request (D4).
- Rule B's `NOT EXISTS` stops a slow publisher from causing a pile of duplicate events (D1).
- The `jsonb_build_object` payload is the second producer of `IngestionEvent` JSON. A test parses it with `IngestionEvent::parse`, so the Rust and SQL producers cannot drift.
- The function returns ids and an action only. It never returns document content.

**`workers/sweeper.rs`.** Every `SWEEP_INTERVAL_SECS`, the sweeper runs:

1. `SELECT * FROM sweep_ingestion_jobs($batch = 500, $stale = JOB_LEASE_SECS)` and logs counts per action, with `warn` if `failed > 0`.
2. `outbox_repo::purge_published(7 days, 1000)`.

**Retry endpoint.** `POST /api/v1/tenants/{tenant_id}/documents/{document_id}/retry` in `handlers/documents.rs`. `document_service::retry`:

```rust
pub async fn retry(state: &AppState, ctx: &TenantContext, id: Uuid) -> Result<DocumentRow, AppError> {
    let any = ctx.has(Permission::DocumentsDeleteAny);
    if !any { ctx.require(Permission::DocumentsUpload)?; }              // Viewer → 403 before any query
    let mut tx = begin_tenant_tx(&state.db, ctx).await?;
    let doc = document_repo::lock_live(&mut tx, id).await?;             // SELECT ... FOR UPDATE, 404 if hidden/deleted
    if !any && doc.uploaded_by != Some(ctx.user_id()) { return Err(AppError::Forbidden); }  // 403, same reasoning as delete
    if doc.status != "failed" {
        return Err(AppError::Conflict { code: "DOCUMENT_NOT_RETRYABLE",
                                        message: "Only failed documents can be retried.".into() });
    }
    document_repo::reset_for_retry(&mut tx, id).await?;                 // status pending, failure fields NULL
    ingestion_repo::enqueue(&mut tx, ctx.tenant_id(), id, state.config.ingestion.job_max_attempts).await?;
    tx.commit().await?;
    // audit: event = "document.retry_requested", tenant_id, document_id, actor
    Ok(doc_after)
}
```

It responds 202 with the existing `DocumentStatusResponse`. The document row lock plus the `status = 'failed'` gate serializes double clicks, so the second request gets 409. `map_db_error` also maps `ingestion_jobs_one_active` to `Conflict { code: "INGESTION_ALREADY_ACTIVE" }` as defense in depth. The permission rule ("Owner/Admin any, Member their own, Viewer never") goes into `docs/api/documents.md` and into the `escalation.rs` role matrix.

**Tests** (`tests/integration/ingestion_retry.rs` and `ingestion_recovery.rs`). Sweeper tests hold `INGEST_GLOBAL`:

| Case | Expectation |
|---|---|
| `ScriptedEmbedder` fails transiently twice, then succeeds; drive with `process` → age `available_at` → sweep → `process` | `ready`, `attempts = 3`, two `retry_scheduled` stops on the way |
| 429 with `Retry-After: 120` | `available_at ≥ now + 120 s` |
| Transient failure at the last attempt | `failed` with the **original** code |
| Permanent parse failure | `failed`, no retry, `attempts = 1` |
| **Worker crash**: claim, drop, age the lease, sweep | Job `pending`, one new outbox event, document `retry_scheduled`. Then `process` → `ready`, `attempts = 2` (exit criterion 2b) |
| Crash on the final attempt | Sweep → job and document `failed` / `ATTEMPTS_EXHAUSTED` |
| **Lost doorbell**: job `pending`, outbox row deleted, `last_enqueued_at` aged | Sweep re-announces once. A second sweep adds nothing while that event is unpublished (exit criterion 2c) |
| **Publisher crash** (T4 simulation) followed by consuming both copies | One `Succeeded`, one `Skipped`, one set of chunks (exit criterion 2a) |
| Retry `NOT EXISTS` guard: sweep twice with an unpublished event present | Exactly one event |
| Active job on a deleted document | Sweep → `cancelled` |
| SQL-built payload | Parses with `IngestionEvent::parse` |
| Sweep as `app_runtime` | Works. `SELECT` on another tenant's jobs directly still returns 0 rows |
| 20 concurrent claim/complete/delete/sweep mixes | Zero `40P01` deadlock errors (D4) |
| Retry endpoint: failed doc as Owner, Admin, own-Member | 202, new job, new event, document `pending` |
| Retry endpoint: other member's doc as Member | 403 |
| Retry endpoint: Viewer | 403 |
| Retry endpoint: `ready`, `processing` or `pending` doc | 409 `DOCUMENT_NOT_RETRYABLE` |
| Retry endpoint: deleted doc or another tenant's id | 404, identical body to a random UUID |
| Retry endpoint: 5 concurrent requests on one failed doc | One 202, four 409, one new job |

**Done when** every row passes. These rows prove exit criteria 2 and 4.

---

### T11: Operations, CI, docs and the end-to-end check (P0/P1, ~3h)

**Heartbeat** (supervisor, every 30 s, one `info` line with `target = "ingestion"`): `outbox_backlog`, `outbox_oldest_age_secs`, `stream_len`, `stream_pending`, `jobs_in_flight`, `worker_id`. Log `warn` when `outbox_oldest_age_secs > 60` or `stream_pending > 10 × concurrency`. These field names are the metric names Phase 7 will export through OpenTelemetry.

**CI** (`.github/workflows/ci.yml`):

1. `check` job, new grep guard: `begin_system_tenant_tx` may appear only under `apps/tenant-saas/src/services/ingestion_service.rs`, `apps/tenant-saas/src/repositories/ingestion_repo.rs` and `apps/tenant-saas/src/workers/`, plus its definition in `common-database`.
2. `check` job, new grep guard: no `sqlx::` import inside `apps/tenant-saas/src/ingestion/` or `src/ai/` (keeps the pure modules pure).
3. `test` job, RLS sanity: add `ingestion_jobs` and `document_chunks` to the `relname IN (...)` list, and fail if `relforcerowsecurity` is true for either (D2).
4. `test` job, grant assertions in the existing style: `app_runtime` cannot `UPDATE document_chunks`, cannot `DELETE FROM ingestion_jobs`.
5. `test` job: assert `sweep_ingestion_jobs` has `prosecdef = true`, a fixed `proconfig` search_path, and no `PUBLIC` EXECUTE (`has_function_privilege('public', ...) = false`).
6. `test` job: `SELECT extversion FROM pg_extension WHERE extname = 'vector'` returns `0.8.0` or newer.
7. `build` job: `cargo build --release --bins`.

**Dockerfile.** Build with `--bins`, copy both binaries. `ENTRYPOINT ["./tenant-saas-api"]` stays as it is. The worker runs with `--entrypoint ./tenant-saas-worker`, and the README shows the command.

**Docs:**

- `README.md`: a "Running the worker" section, the pgvector one-liner for existing volumes (G5), the env-var table for `IngestionConfig`, and the upload-to-`ready` curl walkthrough.
- `docs/api/documents.md`: the lifecycle table from §3, the retry endpoint with its permission and error table, and what `failure_code` values mean.
- `docs/architecture/ingestion.md`: the §2 D1 table, the D3 diagram, the lock-order rule, and the ack table.
- `docs/runbooks/ingestion-recovery.md`: symptoms (backlog age, stuck `processing`, many `failed`), diagnostic SQL run as `db_owner`, how to re-drive (`POST /retry` per document, or a guarded bulk SQL for an incident), what a dead-lettered outbox row means, and the poison-PDF behavior.
- `ADR-007`: append an "Amendments (Phase 4 implementation)" section covering D1, D2, D4, D5 and D9.
- Fix the stale comment in `storage_reconciler.rs`, which says the owner pool "bypasses the FORCE ROW LEVEL SECURITY on documents". `documents` has no FORCE. The owner is exempt because it owns the table.

**End-to-end check** (manual, recorded in the PR description): Compose up → API → worker → upload the three fixture formats → poll status until `ready` → `SELECT count(*), min(vector_dims(embedding)) FROM document_chunks` as `db_owner`. Then kill the worker with `taskkill /F` (or `kill -9`) during a large PDF, restart it, and watch the document reach `ready` after the lease expires.

**Done when** CI is green on a branch and a fresh clone reaches `ready` using README commands only.

---

## 7. Dependency graph

```text
T0 follow-ups (G1–G5)
  |
  +--> T1 (migration 004) --+--> T2 (enqueue, delete) -----------------------+
  |                         |                                                 |
  |                         +--> T5 (worker bin, claim, lease) --+            |
  |                                                              |            |
  +--> T3 (common-messaging) ------------------------------------+--> T4 (publisher)
  |                                                              |            |
  +--> T6 (parser) -----+                                        |            |
  +--> T7 (chunker) ----+--------------------------------------> T9 (pipeline, persist)
  +--> T8 (embedder) ---+                                                     |
                                                                              v
                                                     T10 (retry, sweeper 005, retry API)
                                                                              |
                                                                              v
                                                     T11 (ops, CI, docs, e2e)
```

Natural stopping points between sessions: after T2 (uploads enqueue, nothing consumes yet, safe to merge), after T5 (worker boots), after T9 (happy path works end to end), after T10 (exit criteria met).

---

## 8. Risks and mitigations

| Risk | Why it matters | Mitigation |
|---|---|---|
| `db_owner` cannot `CREATE EXTENSION vector` | Migration 004 fails in CI or on a fresh machine | G5: install it in `00_roles.sql` as superuser. 004 keeps `IF NOT EXISTS`. README line for existing volumes |
| Redis `streams` feature missing | Stream commands do not compile | G1. Fallback: `redis::cmd(...)` |
| 500 ms default response timeout | Every `BLOCK` read errors out | D6 dedicated reader with `block_ms + 2 s`. The T3 test "empty stream returns empty" catches it |
| Deadlocks between delete, claim, persist and sweeper | 500s to users, flaky tests | D4 lock order, SKIP LOCKED in the sweeper, the T10 concurrency test |
| Sweeper blinded by a future `FORCE` | Recovery silently stops | D2, plus the CI assertion that FORCE is off |
| `pdf-extract` panics, hangs or produces poor text on complex PDFs | Worker stalls or documents fail | Panics are caught (`JoinError::is_panic`), the timeout plus the permit-in-closure bounds hangs, OOM falls back to D1 lease recovery. Failures are visible codes, never crashes. A process sandbox is deferred |
| Tokenizer does not match a custom model | Chunks exceed the provider limit → 400 | `tokenizer_for_model` warns at startup. 500-token chunks leave wide margin under 8191-token input limits. A 400 lands as `EMBEDDING_REJECTED` with a clear code |
| Lease shorter than a slow job | Duplicate work | Lease extension after each embedding batch. A stale writer is refused anyway (D3) |
| Thundering herd after a provider outage | Provider rate limits hit again | Equal-jitter backoff plus `Retry-After` |
| Outbox table growth | Slow claims over time | Partial index on unpublished rows plus the 7-day purge |
| Parallel tests stepping on global tasks | Flaky CI | D12: unique streams, `INGEST_GLOBAL`, assertions scoped to the test's own tenant |
| `pgvector` crate vs sqlx 0.9 | Possible build break | D8 text-literal binding, no dependency |
| Fake embeddings mistaken for real ones | Misleading Phase 5 relevance numbers | Rustdoc warning, and Phase 5's relevance evaluation must run with `EMBEDDING_PROVIDER=openai` |
| SECURITY DEFINER misuse | Cross-tenant write path | One function, fixed `search_path`, no `PUBLIC` execute, returns ids only, CI assertion |
| Worker processes jobs for a suspended tenant | Spend on a suspended account | Accepted for Phase 4 and documented. Phase 7 quotas add the check |

---

## 9. Phase 4 deliverable checklist

### Must complete (P0)

- [x] T0 follow-ups G1–G5
- [x] `004_ingestion.sql`: three tables, indexes, grants, RLS (no FORCE), backfill
- [x] `common-messaging`: `IngestionEvent`, `StreamClient` with a separate reader connection
- [x] Upload enqueues job + event in the same transaction. Delete removes chunks
- [x] `begin_system_tenant_tx` plus its tests and pool-reuse case
- [x] `tenant-saas-worker` binary with supervised tasks and graceful shutdown
- [x] Claim with lease token, lease extension, D4 lock order
- [x] Parser and normalizer for TXT, MD, PDF with limits and timeout
- [x] Token-aware chunker, deterministic, UTF-8 safe, capped
- [x] `EmbeddingProvider`, fake, OpenAI-compatible, validation, batching
- [x] `AppState.embedder` with `with_embedder`
- [x] Idempotent persist with lease check and one-transaction replace
- [x] Outbox publisher with claim-with-timeout and outage-safe release
- [x] Transient retry with backoff, permanent failure codes
- [x] `005_ingestion_sweeper.sql` and the sweeper task
- [x] `POST .../documents/{id}/retry`
- [x] Every test named in §0 exists and passes

### Regression guards (P0)

- [x] `cargo fmt --all --check` and `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean
- [x] All Phase 1–3 tests pass unchanged
- [x] `/health/live`, `/health/ready`, `/health/dependencies` behave as before
- [x] `app_runtime` still has no DDL, no `BYPASSRLS`, no `password_hash`, no DELETE on `documents`
- [x] `from_verified_membership` and `object_key` grep guards still pass

### Should complete (P1)

- [x] Storage reconciler running as a supervised worker task
- [x] Heartbeat log with backlog and pending fields
- [x] CI: new grep guards, RLS/FORCE, grant, definer-function and pgvector assertions, `--bins` build
- [x] Dockerfile ships both binaries
- [x] `docs/architecture/ingestion.md`, `docs/runbooks/ingestion-recovery.md`, API doc, README
- [x] ADR-007 amendments

### Nice to have (P2)

- [ ] `XGROUP DELCONSUMER` cleanup of dead consumer names (each restart leaves one behind)
- [ ] A test using the real MinIO adapter for `storage.get` in the pipeline
- [ ] `ingestion_stats()` definer function for per-status job counts in the heartbeat
- [ ] Compose profile that runs API and worker containers

---

## 10. Phase 4 to Phase 5 handoff

| Concern | State at the end of Phase 4 | How Phase 5 uses it |
|---|---|---|
| Chunks | `document_chunks` with `content`, `embedding vector(1536)`, metadata, RLS, composite FK | Add `tsv_content tsvector GENERATED ALWAYS AS (to_tsvector('english', content)) STORED`, a GIN index and an HNSW index in migration 006 |
| Only-ready rule | Chunks exist only for `ready` documents, and are removed in the same tx as delete | Search joins `documents` on `status = 'ready' AND deleted_at IS NULL`. No version filter needed yet |
| Query embeddings | `AppState.embedder` is available in the API | The search service embeds the query with the same provider and model |
| Model/version tracking | `embedding_model` and `embedding_version = "p4-v1"` per chunk | A re-chunk or re-embed is "reset jobs whose chunks have an old version". The retry machinery already exists |
| Fake provider | Default in dev and CI | Phase 5 relevance evaluation must use a real provider, which the README says |
| Tenant filtering at scale | Every chunk carries `tenant_id` | Phase 5 tests `hnsw.iterative_scan = 'relaxed'` (§9.2) on this table |
| Worker | Supervised tasks, a pattern for adding loops | Nothing new needed for search |

---

## 11. Definition of done for Phase 4

1. From a fresh clone, using README commands only: `docker compose up -d --wait`, start the API, start the worker, upload a PDF, poll `GET .../status` until `ready`, and see chunks with 1536-dimension vectors.
2. Killing the worker during processing and restarting it still ends with the document `ready`, with no manual step.
3. Stopping Redis does not break uploads. They return 202, and their jobs complete after Redis returns.
4. Every exit criterion in §0 names a test that exists and passes in CI.
5. No code path holds a PostgreSQL transaction during storage, Redis, parser or provider I/O. Verified by review of `ingestion_service` and `outbox_publisher`.
6. No worker query runs without tenant RLS except the outbox table (no tenant data) and the single audited sweeper function.
7. No log line, error body or `failure_detail` contains document text, object keys, file names or API keys.
8. ADR-007 amendments are merged and the P0 and P1 sections of §9 are fully ticked.
