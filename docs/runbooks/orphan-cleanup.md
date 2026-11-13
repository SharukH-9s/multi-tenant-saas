# Runbook: Orphan Object Cleanup

**Service**: `tenant-saas-api`  
**Component**: Storage reconciler (`services::storage_reconciler`)  
**Severity**: P2 (cost/hygiene) → escalate to P1 if `still_failing` count is growing

---

## Symptoms

| Symptom | Likely Cause |
|---|---|
| S3 storage costs grow faster than document counts | Orphan objects accumulating — reconciler not running or failing |
| `storage_orphans` table row count trending up over days | Pass 1 retries failing repeatedly (check `attempts` column) |
| Documents visible in DB with `status = 'deleting'` for > 5 minutes | Pass 2 stuck-deletion cleanup not running |
| Reconciler exits non-zero or logs `StorageError` on most items | S3 credentials/endpoint misconfigured, or bucket permissions changed |

---

## Diagnostic Queries

Run these as `db_owner` (or via `DATABASE_MIGRATION_URL`):

```sql
-- How many orphans are due for retry?
SELECT count(*), reason, max(attempts)
FROM storage_orphans
WHERE next_attempt_at <= now()
GROUP BY reason
ORDER BY count DESC;

-- All orphans including those not yet due (full picture)
SELECT object_key, reason, attempts, created_at, next_attempt_at
FROM storage_orphans
ORDER BY next_attempt_at;

-- Documents stuck in 'deleting' for more than 1 minute
SELECT id, tenant_id, deleted_at, now() - deleted_at AS stuck_for
FROM documents
WHERE status = 'deleting'
  AND deleted_at < now() - INTERVAL '1 minute'
ORDER BY deleted_at;

-- Reconciler health summary (run after a reconcile pass)
-- Check application logs for the ReconcileReport JSON output.
```

---

## Running the Reconciler

```sh
# One-shot pass (connects to DATABASE_URL and OBJECT_STORAGE_* from environment)
./tenant-saas-api reconcile-orphans
```

The command reads the same environment variables as the API server. Ensure
`DATABASE_MIGRATION_URL` is also set (the reconciler uses `db_owner` to bypass
RLS on `storage_orphans` and to update `documents` stuck in `deleting` state).

### Typical Log Output

A successful run emits a structured log line followed by a `ReconcileReport`:

```json
{
  "retried": 3,
  "deleted_orphans": 2,
  "stuck_deletions_fixed": 1,
  "still_failing": 0
}
```

| Field | Meaning |
|---|---|
| `retried` | Orphan keys from Pass 1 that were retried (deletion attempted again) |
| `deleted_orphans` | Objects confirmed absent from DB and older than retention threshold, now deleted from S3 (Pass 3) |
| `stuck_deletions_fixed` | Documents in `deleting` state whose S3 object was cleaned up (Pass 2) |
| `still_failing` | Orphan keys that failed deletion even after retry — requires investigation |

Exit code is `0` on success (even if `still_failing > 0`). Non-zero exit means the reconciler itself crashed.

---

## Scheduling

### Cron (Linux/macOS)

```sh
# /etc/cron.d/tenant-saas-reconcile
# Run nightly at 02:00, append to rotating log
0 2 * * * deploy /opt/tenant-saas/tenant-saas-api reconcile-orphans \
  >> /var/log/tenant-saas/reconcile.log 2>&1
```

### Kubernetes CronJob

```yaml
apiVersion: batch/v1
kind: CronJob
metadata:
  name: tenant-saas-reconcile-orphans
  namespace: tenant-saas
spec:
  schedule: "0 2 * * *"
  concurrencyPolicy: Forbid          # never run two passes at once
  successfulJobsHistoryLimit: 3
  failedJobsHistoryLimit: 5
  jobTemplate:
    spec:
      template:
        spec:
          restartPolicy: OnFailure
          containers:
            - name: reconciler
              image: tenant-saas-api:latest
              command: ["/app/tenant-saas-api", "reconcile-orphans"]
              envFrom:
                - secretRef:
                    name: tenant-saas-secrets   # DATABASE_MIGRATION_URL, OBJECT_STORAGE_*
```

---

## S3 Lifecycle Rule (Recommended)

Apply this to the `tenant-documents` bucket to abort incomplete multipart uploads
after 24 hours. This protects against `begin_write()` calls that never reached
`finish()` or `abort()` (e.g., API server crash mid-upload).

```json
{
  "Rules": [
    {
      "ID": "abort-incomplete-multipart",
      "Status": "Enabled",
      "Filter": { "Prefix": "" },
      "AbortIncompleteMultipartUpload": {
        "DaysAfterInitiation": 1
      }
    }
  ]
}
```

Apply via AWS CLI:

```sh
aws s3api put-bucket-lifecycle-configuration \
  --bucket tenant-documents \
  --lifecycle-configuration file://lifecycle.json
```

Or via MinIO client (`mcli`):

```sh
mcli ilm import local/tenant-documents < lifecycle.json
```

> **Note**: `STORAGE_ORPHAN_RETENTION_SECS` (default 86400 s = 24 h) must exceed
> your maximum expected upload duration. Lowering it below the lifecycle window
> risks deleting objects that belong to in-flight uploads.

---

## Escalation

Escalate to on-call engineering when any of the following are true:

- `still_failing` is non-zero **and** growing across consecutive nightly runs
- More than 1 000 rows in `storage_orphans` with `attempts >= 5`
- The reconciler itself exits non-zero (crashed, not just soft-failing items)
- Documents stuck in `deleting` for > 30 minutes (potential data integrity issue)
- S3 costs spike > 20% week-over-week without matching document count growth

Provide the on-call engineer with:
1. The `ReconcileReport` JSON from the failing run(s)
2. Output of the diagnostic queries above
3. Recent `tenant-saas-api` logs filtered by `storage_reconciler`

