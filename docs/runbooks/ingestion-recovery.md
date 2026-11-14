# Runbook: Ingestion Recovery

**Service**: `tenant-saas-worker`
**Component**: outbox publisher, ingestion consumer, sweeper
**Background**: [docs/architecture/ingestion.md](../architecture/ingestion.md)
**Severity**: P2 when documents are slow to become `ready`. P1 when nothing progresses for more than 15 minutes, or `failed` counts spike across tenants.

The worker recovers from crashes, Redis outages and lost events without help (architecture doc §2). Use this runbook when that self-healing is not enough or something looks wrong.

---

## Symptoms

| Symptom | Likely cause | Go to |
|---|---|---|
| Heartbeat `outbox_oldest_age_secs` keeps growing, `warn` "outbox is lagging" | Redis unreachable, or no worker running | [A](#a-outbox-not-draining) |
| No `worker heartbeat` lines at all | Worker not running, or crash-looping on startup | [B](#b-worker-not-running) |
| `stream_pending` high, `warn` "delivered but unacknowledged" | Consumers dying mid-job (OOM, panics), or the database is down | [C](#c-jobs-stuck-in-processing) |
| Documents sit in `processing` long after `JOB_LEASE_SECS` | Sweeper not running, or every worker is dead | [C](#c-jobs-stuck-in-processing) |
| Many documents `failed` with the same `failure_code` | Provider misconfiguration, parser regression, or bad input | [D](#d-many-failed-documents) |
| Rows with `dead_lettered_at` in `outbox_events` | Malformed outbox payload (never expected) | [E](#e-dead-lettered-outbox-rows) |
| One PDF keeps killing the worker | Poison document | [F](#f-poison-documents) |

The heartbeat is one `info` line every 30 s with `target = "ingestion"` and fields `outbox_backlog`, `outbox_oldest_age_secs`, `stream_len`, `stream_pending`, `jobs_in_flight`, `db_ok` and `redis_ok`. A field is missing when its source could not be read on that tick.

---

## Diagnostic queries

Run as `db_owner` (`DATABASE_MIGRATION_URL`). `app_runtime` sees no job rows without a tenant context.

```sql
-- Jobs by status
SELECT status, count(*) FROM ingestion_jobs GROUP BY status ORDER BY status;

-- Leases that have expired (the sweeper should pick these up within SWEEP_INTERVAL_SECS)
SELECT id, tenant_id, document_id, attempts, max_attempts, locked_by, locked_until,
       now() - locked_until AS expired_for
  FROM ingestion_jobs
 WHERE status = 'processing' AND locked_until < now()
 ORDER BY locked_until
 LIMIT 50;

-- Outbox backlog and age
SELECT count(*) AS unpublished, now() - min(created_at) AS oldest
  FROM outbox_events
 WHERE published_at IS NULL AND dead_lettered_at IS NULL;

-- Failures in the last 24 h, by cause
SELECT failure_code, count(*)
  FROM documents
 WHERE status = 'failed' AND updated_at > now() - interval '24 hours'
 GROUP BY failure_code
 ORDER BY count DESC;

-- History of one document's jobs
SELECT id, status, attempts, max_attempts, last_error_code, last_error_at,
       created_at, completed_at
  FROM ingestion_jobs
 WHERE document_id = '<document-id>'
 ORDER BY created_at;
```

Redis (stream and group names come from `INGEST_STREAM` and `INGEST_GROUP`):

```sh
redis-cli XLEN ingestion:events
redis-cli XPENDING ingestion:events ingestion-workers
redis-cli XINFO CONSUMERS ingestion:events ingestion-workers
```

---

## A. Outbox not draining

1. Check `redis_ok` in the heartbeat, and run `redis-cli PING` from the worker host.
2. If Redis was down: no action is needed. The publisher released its claims and backs off at most 5 s. The backlog drains on its own once Redis is back. Nothing is lost or dead-lettered by an outage.
3. If Redis is up but the backlog still grows, check the worker logs for `outbox publish cycle failed` and fix the underlying database or Redis error.

## B. Worker not running

1. Start it: `./tenant-saas-worker`, or the image with `--entrypoint ./tenant-saas-worker`.
2. A config error stops it at startup with the failing key named. One example: `ingestion.embedding_api_key` when `EMBEDDING_PROVIDER=openai` has no key.
3. Uploads keep working while the worker is down. They return 202, and their jobs wait in the outbox.

## C. Jobs stuck in processing

1. Make sure at least one worker is running and logging `ingestion sweep` lines. The sweeper is the only thing that moves an expired lease along.
2. An expired lease with attempts left goes back to `pending` and gets a new event. On the final attempt the job and document become `failed` / `ATTEMPTS_EXHAUSTED`.
3. A job whose lease has **not** expired is still owned by a live worker. Do not touch it. If the job is genuinely slow (very large documents, a slow provider), raise `JOB_LEASE_SECS` rather than editing rows. The worker also extends its own lease after every embedding batch.

## D. Many failed documents

1. Group by `failure_code` (query above). Then:
   - `EMBEDDING_UNAVAILABLE` across many tenants: provider outage, or a bad `EMBEDDING_API_KEY` (401/403 is logged at `error` naming the key). Fix the cause. Jobs still in retry recover on their own.
   - `PARSE_FAILED` / `DOCUMENT_EMPTY`: usually bad input (encrypted or scanned PDFs). Check whether it is one tenant or a parser regression after a deploy.
   - `EMBEDDING_REJECTED`: model name or request shape rejected by the provider. Check `EMBEDDING_MODEL` and `EMBEDDING_BASE_URL`.
2. Re-drive one document once the cause is fixed. Use the API, which enforces permissions and writes an audit line:
   ```sh
   curl -X POST -H "Authorization: Bearer $TOKEN" \
     http://localhost:8080/api/v1/tenants/$TENANT_ID/documents/$DOCUMENT_ID/retry
   ```
3. Re-drive many documents during an incident. Run as `db_owner` in **one** transaction. It does exactly what the retry endpoint does: reset the document, add a fresh job and an outbox event. Narrow the `WHERE` clause to the affected tenant and failure code, and review the `SELECT` count first:
   ```sql
   BEGIN;
   SELECT count(*) FROM documents
    WHERE status = 'failed' AND deleted_at IS NULL
      AND failure_code = 'EMBEDDING_UNAVAILABLE'
      AND tenant_id = '<tenant-id>';

   WITH docs AS (
       UPDATE documents
          SET status = 'pending', failure_code = NULL, failure_detail = NULL
        WHERE status = 'failed' AND deleted_at IS NULL
          AND failure_code = 'EMBEDDING_UNAVAILABLE'
          AND tenant_id = '<tenant-id>'
       RETURNING tenant_id, id),
   jobs AS (
       INSERT INTO ingestion_jobs (tenant_id, document_id, max_attempts)
       SELECT tenant_id, id, 5 FROM docs
       RETURNING id, tenant_id, document_id)
   INSERT INTO outbox_events (tenant_id, aggregate_type, aggregate_id, event_type, payload)
   SELECT tenant_id, 'ingestion_job', id, 'ingestion.requested',
          jsonb_build_object('v', 1, 'job_id', id, 'tenant_id', tenant_id, 'document_id', document_id)
     FROM jobs;
   COMMIT;
   ```
   The `ingestion_jobs_one_active` unique index makes this safe to re-run: a document that already has an active job makes the statement fail instead of creating a duplicate.

## E. Dead-lettered outbox rows

Dead-lettering happens only when a payload can never become a valid event (unknown `event_type`, or JSON that is not a v1 `IngestionEvent`). A Redis outage never dead-letters.

```sql
SELECT id, aggregate_id, event_type, last_error, created_at, dead_lettered_at
  FROM outbox_events WHERE dead_lettered_at IS NOT NULL ORDER BY dead_lettered_at DESC;
```

The job behind the row is still `pending`. Once it is older than `JOB_LEASE_SECS` the sweeper re-announces it with a fresh, valid event, so no manual re-drive is needed. Investigate how the bad row was written. Application code builds payloads only through `IngestionEvent` and the sweeper function.

## F. Poison documents

An out-of-memory abort or a hard crash inside the PDF parser kills the whole worker process. Recovery needs no special handling:

1. The lease of the job that was running expires.
2. The sweeper requeues it and another attempt starts.
3. After `JOB_MAX_ATTEMPTS` crashes the job and document become `failed` / `ATTEMPTS_EXHAUSTED`, and the worker stops picking the document up.

Each crash also interrupts the other jobs that were in flight. They are recovered the same way and do not use up extra attempts beyond the one they were on. To stop the crash loop faster, find the document (`locked_by` on the expired job names the process) and delete it, or mark its active job `cancelled` as `db_owner`.

---

## Routine maintenance

None is required:

- Published outbox rows older than 7 days are purged by the sweeper (1000 per tick).
- Each worker restart leaves an idle consumer name in the Redis group. It is harmless. Remove old ones with `XGROUP DELCONSUMER` only after `XINFO CONSUMERS` shows `pending = 0`.
