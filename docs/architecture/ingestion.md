# Ingestion Architecture

**Scope**: how an uploaded document becomes searchable chunks (Phase 4).
**Decisions**: [ADR-007](../adr/ADR-007-ingestion-outbox-worker.md) and `phase4_implementation_plan.md` §2.
**Operations**: [ingestion-recovery runbook](../runbooks/ingestion-recovery.md).

---

## 1. Components

```text
tenant-saas-api                                 PostgreSQL (source of truth)
  POST /documents ── one tx ──────────────►  documents + ingestion_jobs + outbox_events
  POST /documents/{id}/retry ── one tx ───►  documents + ingestion_jobs + outbox_events

tenant-saas-worker  (one process, supervised Tokio tasks)
  ├─ outbox_publisher    outbox_events ──XADD──►  Redis stream "ingestion:events"
  ├─ ingestion_consumer  XREADGROUP ─► process(job) ─► XACK
  ├─ sweeper             sweep_ingestion_jobs()  (every SWEEP_INTERVAL_SECS)
  ├─ storage_reconciler  Phase 3 reconciler, shallow passes (every 10 min)
  └─ heartbeat           one log line every 30 s
```

Every task runs inside `supervise()`. A task that returns or panics is logged and restarted after 1 s. On SIGTERM or Ctrl+C the worker stops reading new messages and gives in-flight jobs 30 s to finish. Anything still running after that is recovered by the sweeper once its lease expires.

The worker never runs migrations.

## 2. Redis only rings the doorbell

The stream message carries three ids (`{"v":1,"job_id","tenant_id","document_id"}`). Every decision about what work exists comes from `ingestion_jobs`. So one recovery path, owned by the database, covers every failure:

| Situation | Who notices | What happens |
|---|---|---|
| Normal upload | Upload tx writes job + outbox event | Publisher sends the event, consumer claims and processes |
| Publisher crashed after `XADD`, before marking the row | The row's `claimed_until` expires (`OUTBOX_CLAIM_SECS`) | Row is published again. Duplicate event, harmless |
| Redis down | Publisher `XADD` fails | Claimed rows are released, publisher backs off (200 ms → 5 s). Nothing is dead-lettered. Rows drain when Redis returns |
| Worker crashed mid-job | Sweeper: `processing` with `locked_until < now()` | Job back to `pending` with a new outbox event. At the final attempt: `failed` / `ATTEMPTS_EXHAUSTED` |
| Transient failure | Worker writes `retry_scheduled` with `available_at` | Sweeper turns it into `pending` + event once due |
| Event lost (stream trimmed, Redis flushed) | Sweeper: `pending` and `last_enqueued_at` older than `JOB_LEASE_SECS`, with no unpublished event | New outbox event |
| Consumer died holding unacked messages | Consumer `XAUTOCLAIM` every 30 s (min idle = `JOB_LEASE_SECS`) | Redelivered. The database claim decides whether anything runs |
| Document deleted while its job is active | Worker claim, or sweeper rule 0 | Job `cancelled` |

A `retry_scheduled` job is never claimable directly. Only the sweeper moves it back to `pending`, so an early duplicate message cannot skip the backoff.

## 3. One job: claim, process, persist

```text
event ─► [tx A] lock document, claim job (new lease_token, attempts+1),
                document → processing                                   commit
      ─► download object, check size_bytes and sha256 = content_hash    (no tx)
      ─► parse + normalize      spawn_blocking, PARSE_TIMEOUT_SECS       (no tx)
      ─► chunk                  spawn_blocking                          (no tx)
      ─► embed in batches       lease extended after every batch        (no tx)
      ─► [tx B] lock document (must still be processing), check lease,
                replace chunks, document → ready, job → succeeded       commit
      ─► XACK
```

No connection is held during storage, parser, Redis or provider I/O (design §7.3.1).

Tx B refuses to write when the lease token changed or the document is no longer `processing`. The outcome is then `Superseded`. So a stale worker can never overwrite a newer attempt, and a late worker cannot resurrect a deleted document.

Chunks are replaced with delete-then-insert in tx B. Together with `UNIQUE (tenant_id, document_id, chunk_index)` this makes duplicate events harmless: processing the same document twice leaves exactly one set of chunks. A document that would produce more than `MAX_CHUNKS_PER_DOC` chunks fails with `DOCUMENT_TOO_LARGE`. It is never truncated.

Each chunk records `embedding_model` and `embedding_version` (`p4-v1`: parser + normalizer + chunker version), so a future re-chunk or re-embed can find stale rows.

## 4. Lock order

Several transactions touch both `documents` and `ingestion_jobs`: claim, persist, failure write, retry endpoint and sweeper. To rule out deadlocks:

- Every Rust-side transaction locks the `documents` row first, then the job row.
- The sweeper selects with `FOR UPDATE OF j, d SKIP LOCKED`. It never waits, so it cannot be part of a deadlock.
- The API delete path locks only `documents` and `document_chunks`. It does not touch the job. The worker or the sweeper cancels the job later.

## 5. Tenant isolation in the worker

- Every worker query runs inside `begin_system_tenant_tx(pool, tenant_id)`. That sets `app.current_tenant_id` from the event, so RLS still applies. An event with a mismatched tenant matches zero rows and is acked as poison. CI restricts callers of this helper to the ingestion service, the ingestion repository and `workers/`.
- `outbox_events` has no RLS. It holds ids only and no endpoint reads it.
- `sweep_ingestion_jobs()` is the single cross-tenant function. It is `SECURITY DEFINER`, owned by `db_owner`, has a fixed `search_path`, has no `PUBLIC` execute, and returns ids and an action only. It works because `ingestion_jobs` and `document_chunks` use RLS **without FORCE**, so the owner is exempt. CI fails if FORCE is ever added.

## 6. Acknowledgement policy

| Outcome of `process(event)` | `XACK`? |
|---|---|
| `Succeeded`, `Failed`, `RetryScheduled`, `Cancelled` | yes, after the commit |
| `Skipped` (already done, leased by someone else, `retry_scheduled`, or out of attempts) | yes |
| `Superseded` (lease lost before persist) | yes |
| `Poison` (bad JSON, unknown `v`, tenant/job/document mismatch) | yes, with a `warn` log carrying the stream id only |
| `Unavailable` (database unreachable before the claim committed) | **no**. Left pending for `XAUTOCLAIM` |
| Task panicked | **no**. The lease expires and the sweeper takes over |

## 7. Failure codes

Written to `documents.failure_code` and `ingestion_jobs.last_error_code`. `failure_detail` is short fixed text written by the worker. It never contains document text, file names, object keys, API keys or provider response bodies.

| Code | Kind | Meaning |
|---|---|---|
| `PARSE_FAILED` | permanent | Parser error or panic (corrupt or encrypted PDF, invalid UTF-8) |
| `DOCUMENT_EMPTY` | permanent | No text after normalization. Scanned PDFs without a text layer land here (no OCR) |
| `DOCUMENT_TOO_LARGE` | permanent | Over `MAX_PDF_PAGES`, `MAX_EXTRACTED_CHARS`, `MAX_CHUNKS_PER_DOC` or `MAX_UPLOAD_BYTES` |
| `UNSUPPORTED_FORMAT` | permanent | MIME type the parser does not handle |
| `OBJECT_MISSING` | permanent | Stored object not found |
| `OBJECT_CORRUPT` | permanent | Stored bytes do not match `size_bytes` or `content_hash` |
| `EMBEDDING_REJECTED` | permanent | Provider returned a 4xx other than 401, 403 or 429 |
| `EMBEDDING_INVALID_RESPONSE` | permanent | Wrong vector count or dimension, NaN or Inf, or an oversized body |
| `STORAGE_UNAVAILABLE` | transient | Object storage unreachable |
| `EMBEDDING_UNAVAILABLE` | transient | 429, 5xx, timeout, connection error, or 401/403 (a bad key is an operator fix, so it waits in retry) |
| `DATABASE_UNAVAILABLE` | transient | PostgreSQL unreachable mid-job |
| `PARSE_TIMEOUT` | transient | Parser exceeded `PARSE_TIMEOUT_SECS` |
| `INTERNAL_ERROR` | transient | Unexpected worker error |
| `ATTEMPTS_EXHAUSTED` | permanent | Worker did not finish within the lease on the final attempt (crash or OOM) |

Transient failures retry with exponential backoff and equal jitter: `RETRY_BASE_SECS × 2^(attempt-1)`, capped at `RETRY_MAX_SECS`, never shorter than a provider's `Retry-After`. When the last attempt fails transiently, the job is marked `failed` with that original code, so the document shows the real cause.

## 8. Configuration

All keys and defaults are listed in `.env.example` under `INGESTION` and `EMBEDDINGS`. The ones operators usually touch:

| Key | Default | Notes |
|---|---|---|
| `WORKER_CONCURRENCY` | 4 | Keep `DATABASE_MAX_CONNECTIONS ≥ WORKER_CONCURRENCY + 3`. The worker warns at startup otherwise |
| `JOB_LEASE_SECS` | 300 | Also the "stale pending" threshold and the `XAUTOCLAIM` idle time |
| `JOB_MAX_ATTEMPTS` | 5 | Per job. A manual retry creates a new job with fresh attempts |
| `SWEEP_INTERVAL_SECS` | 15 | Upper bound on how late a due retry starts |
| `EMBEDDING_PROVIDER` | `fake` | `fake` vectors are deterministic and have **no semantic meaning**. Use `openai` for any relevance measurement |
| `EMBEDDING_DIM` | 1536 | Fixed by the `vector(1536)` column. Config rejects other values |
