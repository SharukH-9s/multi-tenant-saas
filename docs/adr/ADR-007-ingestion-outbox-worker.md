# ADR-007: Reliable Ingestion Pipeline, Outbox Pattern, and Worker Architecture

**Status**: Accepted  
**Date**: 2026-11-13  
**Deciders**: Platform Engineering  
**Phase**: 4 — Reliable Ingestion  

---

## Context

Document ingestion transforms uploaded files (PDF, Markdown, plain text) into token-bounded chunks and semantic vector embeddings stored in PostgreSQL. This pipeline involves multiple distinct systems and failure boundaries:

1. **API Server (`tenant-saas-api`)**: Accepts document upload requests, writes metadata, and must reliably enqueue ingestion work without dropping requests when downstream components (Redis, workers) are degraded or restarting.
2. **PostgreSQL**: System of record for tenants, documents, ingestion jobs, outbox events, and vector chunks.
3. **Redis Streams**: High-throughput, low-latency event doorbell transport between API nodes and ingestion workers.
4. **Worker (`tenant-saas-worker`)**: Standalone background process executing three asynchronous loops:
   - Outbox publisher (reads unpublished outbox events from PostgreSQL and publishes them to Redis Streams).
   - Ingestion consumer (reads stream events, claims jobs, extracts text, chunks, computes embeddings, and persists results).
   - Sweeper (detects dead workers, expired leases, and due retries).
5. **External Embedding Providers**: External network dependencies (e.g. OpenAI) with latency, rate limits, and transient availability failures.

A naive architecture would perform text extraction, chunking, or embedding calls within the upload request or within long-lived database transactions, risking connection pool exhaustion (§7.3.1) and lost work upon worker crash.

Phase 4 defines the reliable asynchronous ingestion pipeline that guarantees durable acceptance, at-least-once delivery, crash recovery without work loss, idempotent processing, and strict multi-tenant isolation.

---

## Decisions

### 1. Publisher Concurrency: Claim-with-Timeout (Deviation from §5.3)

In early architectural sketches (§5.3), the outbox publisher was envisioned as holding a row-level lock (`SELECT ... FOR UPDATE SKIP LOCKED`) while publishing events to Redis.

**Decision**: The publisher claims outbox rows using a **claim-with-timeout** inside a micro-scoped transaction, commits, and releases the database connection *before* executing any Redis `XADD` commands.

```sql
UPDATE outbox_events
SET claimed_until = now() + interval '30 seconds',
    publish_attempts = publish_attempts + 1
WHERE id IN (
    SELECT id
    FROM outbox_events
    WHERE published_at IS NULL
      AND dead_lettered_at IS NULL
      AND (claimed_until IS NULL OR claimed_until < now())
    ORDER BY created_at
    LIMIT 50
    FOR UPDATE SKIP LOCKED
)
RETURNING id, aggregate_id, event_type, payload;
```

**Rationale**:
- **Strict Micro-Scoped Transactions (§7.3.1)**: Holding a PostgreSQL transaction open across Redis network I/O violates our rule against open connections during network calls. If Redis slows down or stalls, the API and worker connection pools would exhaust rapidly.
- **Crash Recovery**: If the publisher process crashes after `XADD` but before marking `published_at = now()`, the claim expires after `OUTBOX_CLAIM_SECS` (default 30 s), and another publisher instance republishes the event. Downstream consumers are idempotent (see Decision 5).

### 2. Worker Cross-Tenant Access: `begin_system_tenant_tx` (D1)

The worker process receives an event from Redis Streams carrying `{ "tenant_id": "<uuid>", "job_id": "<uuid>", "document_id": "<uuid>" }`.

**Decision**: The worker executes tenant-scoped operations by opening a transaction with `begin_system_tenant_tx(pool, tenant_id)`. Row Level Security (RLS) remains fully active.

**Rationale**:
- Avoids granting `BYPASSRLS` to `app_runtime` or introducing a dedicated bypass role.
- Setting `app.current_tenant_id` ensures that even in background processing, all database queries are constrained by tenant RLS policies.
- If a forged or corrupted event carries a mismatched `(tenant_id, document_id)`, the query matches zero rows and aborts safely without leaking cross-tenant data.

### 3. `outbox_events` Has No RLS (D2)

**Decision**: The `outbox_events` table does **not** have Row Level Security enabled.

**Rationale**:
- Following the precedent of `storage_orphans` (ADR-006) and `refresh_tokens` (ADR-004), `outbox_events` is an internal system transport table.
- The outbox publisher is a global process with no tenant context; it scans and claims unpublished events across all tenants in FIFO order.
- The table contains only opaque identifiers, event names, and JSON payloads. No tenant-facing API endpoint reads or writes `outbox_events` directly.

### 4. Cross-Tenant Sweeper via Audited `SECURITY DEFINER` Function (D3)

The background sweeper must identify stuck jobs (crashed workers whose leases expired) and retry-ready jobs across all tenants.

**Decision**: A single SQL function, `sweep_ingestion_jobs()`, owned by `db_owner` with `SECURITY DEFINER` and a fixed `search_path`, is granted `EXECUTE` to `app_runtime`.

**Rationale**:
- Prevents `app_runtime` from needing superuser or `BYPASSRLS` rights to inspect multi-tenant job statuses.
- The attack surface is strictly bounded to one audited SQL procedure that returns only affected row IDs and schedules outbox events, without exposing raw tenant data.

### 5. Chunk Replacement Strategy: Single-Transaction Delete-Then-Insert (D4)

When an ingestion attempt completes, generated chunks must replace any previous chunks for the document.

**Decision**: The worker persists chunks using a delete-then-insert pattern inside a single short transaction, guarded by the job's unique lease token and capped by `MAX_CHUNKS_PER_DOC` (default 2,000).

```sql
-- Inside tenant transaction:
-- 1. Validate lease token:
UPDATE ingestion_jobs
SET status = 'succeeded', completed_at = now()
WHERE id = $job_id AND lease_token = $lease AND status = 'processing';
-- If 0 rows updated, worker was superseded -> rollback and abort.

-- 2. Atomically replace chunks:
DELETE FROM document_chunks WHERE document_id = $doc_id;
INSERT INTO document_chunks (...) VALUES (...);

-- 3. Mark document ready:
UPDATE documents SET status = 'ready', chunk_count = $count WHERE id = $doc_id;
```

**Rationale**:
- A document uploaded at max size (20 MiB) produces at most ~2,000 chunks. Replacing 2,000 rows in one transaction takes milliseconds in PostgreSQL and maintains ACID consistency.
- Complex versioned chunk staging tables are deferred until benchmarks demonstrate transaction lock contention (§6).

### 6. Vector Column in Phase 4 Schema, Indexing in Phase 5 (D5)

**Decision**: The `document_chunks` table is created in Migration 004 with `embedding vector(1536) NOT NULL`. The HNSW index and full-text `tsvector` columns are deferred to Phase 5.

**Rationale**:
- Phase 4 must store and validate embedding outputs.
- HNSW indexes require substantial dataset volumes and realistic tuning parameters (`m`, `ef_construction`) to benchmark effectively. Building indexes on empty tables prematurely wastes maintenance overhead during ingestion.

### 7. PDF Parsing with `pdf-extract` in `spawn_blocking` (D6)

**Decision**: PDF text extraction uses the pure-Rust `pdf-extract` crate, executed inside Tokio's `spawn_blocking` thread pool with strict timeouts (`PARSE_TIMEOUT_SECS = 60`), page limits (`MAX_PDF_PAGES = 500`), and extracted character limits (`MAX_EXTRACTED_CHARS = 5,000,000`).

**Rationale**:
- Pure-Rust implementation avoids external binary dependencies (e.g. `pdftotext` or Python subprocesses).
- `spawn_blocking` isolates CPU-bound parsing from Tokio async workers.
- Any parser panic or memory exhaustion is caught, mapped to `ParseError::Corrupt` / `ParseError::TooLarge`, and recorded as a permanent `PARSE_FAILED` error code without crashing the worker process.
- OS-level process sandboxing (bubblewrap/seccomp) is documented and deferred to production hardening.

### 8. Pluggable Embedding Provider Abstraction (D7)

**Decision**: Define an asynchronous `EmbeddingProvider` trait with two implementations:
- `FakeEmbeddingProvider`: Deterministic, offline provider generating unit vectors derived from sha256 hashing. Used by default in local dev, unit tests, and CI without requiring API keys.
- `OpenAiCompatibleProvider`: HTTP client (`reqwest` with `rustls`) supporting OpenAI and compatible endpoints (e.g., Azure OpenAI, vLLM, Ollama), honoring HTTP 429 `Retry-After` headers.

**Rationale**:
- CI and test suites run completely offline without flaky external network calls or secrets.
- Real provider integrations can be swapped via configuration (`EMBEDDING_PROVIDER=fake|openai`).
- Config validation enforces that configured dimensions match the database schema (`1536`).

### 9. Versioned Event Schema (D8)

**Decision**: Ingestion events published to Redis Streams follow a minimal, versioned JSON schema:

```json
{
  "v": 1,
  "job_id": "018d3b7e-9f3a-7a1b-9f2d-8e4a1b2c3d4e",
  "tenant_id": "018d3b7e-9f3a-7a1b-9f2d-8e4a1b2c3d4f",
  "document_id": "018d3b7e-9f3a-7a1b-9f2d-8e4a1b2c3d50"
}
```

**Rationale**:
- Redis is merely an ephemeral notification doorbell; PostgreSQL is the single source of truth.
- Carrying only primary keys minimizes Redis memory consumption.
- Version tag (`v`) guarantees that future payload additions or format migrations can be handled cleanly without crashing running consumers. Unknown versions are rejected and acknowledged to prevent poison pill loops.

---

## Consequences

### Positive
- **Guaranteed Durability**: Ingestion jobs and outbox events commit in the same transaction as document creation. Redis downtime never causes upload failures.
- **Connection Safety**: Micro-scoped transactions prevent connection pool starvation across Redis and AI provider calls.
- **Multi-Tenant Security**: RLS is strictly enforced for all worker queries through `begin_system_tenant_tx`.
- **Zero Cost CI**: `FakeEmbeddingProvider` ensures complete testability offline without credentials.

### Negative
- **At-Least-Once Delivery**: A crash after Redis `XADD` but before PostgreSQL `outbox_events` update causes duplicate events. Consumers must implement lease token checks and unique constraints.
- **Parser Limitations**: `pdf-extract` does not support scanned OCR or complex PDF forms. Unsupported formats will fail with safe `PARSE_FAILED` codes.


---

## Amendments (Phase 4 implementation, 2026-11-15)

Implementing Phase 4 refined the decisions above. The original text is kept unchanged so the reasoning stays traceable. Where they differ, these amendments win. The labels D1, D2, D4, D5 and D9 refer to `phase4_implementation_plan.md` §2.

### A1. One recovery path, owned by the database (plan D1)

Redis carries a doorbell only. Every recovery decision is made from `ingestion_jobs` by `sweep_ingestion_jobs()`: expired leases, due retries, lost events, and jobs of deleted documents. A `retry_scheduled` job is not claimable directly. Only the sweeper moves it back to `pending`, so an early duplicate message cannot bypass the backoff. Unacked messages of dead consumers are redelivered with `XAUTOCLAIM`, and the database claim then decides whether anything runs.

### A2. New tenant tables use RLS without FORCE (plan D2)

Decision 4 only works if `db_owner`, which owns the sweeper function, is exempt from RLS on `ingestion_jobs` and `document_chunks`. Both tables therefore `ENABLE` RLS without `FORCE`, like `documents`. `app_runtime` is never the owner and stays fully bound. CI fails if either table gains `FORCE`.

### A3. Lock order: documents first, then ingestion_jobs (plan D4)

The SQL sketch in Decision 5 updates the job before the document. The implementation reverses that: every transaction that touches both tables locks the `documents` row first. The sweeper uses `FOR UPDATE OF j, d SKIP LOCKED` and never waits, so it cannot take part in a deadlock.

### A4. `publish_attempts` is diagnostic. Redis outages never dead-letter (plan D5)

The claim SQL in Decision 1 increments `publish_attempts` on every claim, so the column counts claims, not failures. On a Redis connection or timeout error the publisher releases its claimed rows (`claimed_until = NULL`) and backs off, capped at 5 s. A row is dead-lettered only when its payload can never become a valid `IngestionEvent`. A long Redis outage therefore loses nothing, and the backlog drains when Redis returns.

### A5. Deleting a document does not touch its job (plan D9)

The API delete path removes chunks in the same transaction that marks the document `deleting`. It never locks the job row. A late worker is stopped by the persist step's `status = 'processing'` check. The job is marked `cancelled` by the worker's claim or by the sweeper.

### A6. Correction to Decision 7

Decision 7 says memory exhaustion is "caught". It is not: an out-of-memory abort kills the worker process. It is still contained without special code. The job's lease expires, the sweeper requeues it, and after `max_attempts` crashes the job ends `failed` with `ATTEMPTS_EXHAUSTED`. Panics and timeouts are caught as described. The behaviour is covered in `docs/runbooks/ingestion-recovery.md` (F).
