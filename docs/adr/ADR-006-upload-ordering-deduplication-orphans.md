# ADR-006: Upload Ordering, Deduplication, and Orphan Management

**Status**: Accepted  
**Date**: 2026-11-09  
**Deciders**: Platform Engineering  
**Phase**: 3 — Document Upload

---

## Context

A document upload involves two independent systems — PostgreSQL and S3 — with no
cross-system transaction primitive. Any ordering of the two writes can leave one
system ahead of the other if the process dies at the wrong moment:

- **Storage first, then DB**: an S3 object without a DB row (an *orphan*).
- **DB first, then storage**: a DB row referencing an S3 key that does not exist
  yet (a *dangling reference*).

Additionally, the platform must handle:
- **Deduplication**: the same file uploaded twice (by any user in a tenant) should
  not create two objects in S3.
- **Idempotency**: a client retrying a failed upload request should not receive
  an error if the document was already committed.
- **Cleanup**: the two-phase write means a crash window exists; a reconciler is
  needed to close it.

---

## Decisions

### 1. Storage-First Write Ordering

The object is written to S3 **before** the `INSERT` into PostgreSQL.

**Rationale**: SHA-256 is computed by streaming bytes through a digest as they are
written. The hash is not known until `ObjectWriter::finish()` returns. The database
row stores `content_hash NOT NULL`; therefore the DB insert can only happen after
storage is complete.

A storage-first order means the crash window produces an *orphan* (S3 object with
no DB row). This is recoverable: the orphan is eventually cleaned up by the
reconciler. A DB-first order would produce a *dangling reference* (DB row pointing
to a missing S3 key), which causes runtime errors on every read of that document
and is harder to detect and repair automatically.

### 2. Partial Unique Index for Deduplication

```sql
CREATE UNIQUE INDEX documents_tenant_content_hash_live
    ON documents (tenant_id, content_hash)
    WHERE deleted_at IS NULL;
```

This index enforces that no two *live* documents within a tenant share the same
content hash. It does **not** prevent re-uploading a file after the original has
been soft-deleted, which is the desired behavior (a user may delete and re-upload).

SHA-256 is content-addressed: two files with identical bytes have the same hash
regardless of filename or MIME type. This means deduplication works across
renames.

### 3. `200 OK` on Duplicate Upload (Not `409 Conflict`)

When a duplicate is detected (the unique index constraint is violated), the
endpoint returns `200 OK` with the existing document record rather than a
`409 Conflict`.

**Rationale**: A duplicate upload is not an error from the client's perspective —
the file they wanted stored *is* stored. `409 Conflict` would force every client
to implement a separate error handler for a scenario where nothing went wrong.
`200 OK` makes the operation idempotent: retrying a failed upload is safe and
produces the same result. The existing S3 object is left in place; the newly
written orphan copy is removed by the reconciler.

**Rejected alternative**: Return `409 Conflict` with a `Location` header pointing
to the existing document. Rejected because it adds client-side complexity for no
user-visible benefit.

### 4. `storage_orphans` Table with No RLS

The reconciler tracks objects that require cleanup in a `storage_orphans` table.
This table has `ENABLE ROW LEVEL SECURITY` **not applied** (no RLS at all).

**Rationale**:
- The reconciler runs as `db_owner` (not `app_runtime`) because it has no tenant
  context — it operates across all tenants simultaneously.
- RLS policies on `storage_orphans` would need to allow `db_owner` full access
  anyway, which is the default when RLS is not enabled for that role.
- The table stores only opaque S3 object keys, attempt counters, and timestamps.
  It contains no PII, no tenant-scoped business data, and no content.
- `app_runtime` is granted full CRUD on `storage_orphans` so the API server can
  `INSERT` an orphan record when a storage write succeeds but the subsequent DB
  insert fails.

This follows the same pattern used for `refresh_tokens` in ADR-004: tables whose
access pattern does not align with tenant-scoped RLS skip the policy rather than
carry a policy that is effectively a no-op.

### 5. Age Guard on Pass 3 Deep Sweep (Orphan Deletion)

The reconciler's Pass 3 ("deep sweep") only deletes S3 objects that are **older
than `STORAGE_ORPHAN_RETENTION_SECS`** (default: 86 400 s / 24 hours).

**Rationale**: An S3 object that has no DB row is not necessarily an orphan — it
may belong to an upload that is still in progress (the API server wrote the object
but has not yet committed the DB insert). The age guard prevents the reconciler
from deleting an object that is concurrently being finalized.

The 24-hour default far exceeds any reasonable upload duration. Operators who
lower `STORAGE_ORPHAN_RETENTION_SECS` below the maximum expected upload
duration risk the reconciler deleting in-flight uploads. **Removing the guard
entirely would cause data loss.** The age guard is the only protection in this
window; there is no other lock or reservation mechanism.

---

## Consequences

**Positive**:
- The system is eventually consistent: a crash at any point leaves the platform in
  a repairable state. Orphans are cleaned up nightly; dangling references never
  occur.
- Deduplication is automatic and transparent; no client-side fingerprinting is
  required.
- Clients can retry uploads without special error handling.

**Negative / Trade-offs**:
- During the reconciler retention window (default 24 h), duplicate S3 objects may
  exist transiently for deduplicated uploads. This is a known and acceptable cost.
- The `storage_orphans` table is the system of record for failed cleanup. If it is
  lost (e.g., accidental `DROP TABLE`), the reconciler loses track of previously
  identified orphans and Pass 3 must re-identify them from scratch on the next run.
- Lowering `STORAGE_ORPHAN_RETENTION_SECS` is an operational risk. The value must
  be documented and protected in deployment configuration management.
- `200 OK` on duplicate means API clients cannot distinguish "new upload" from
  "deduplicated upload" by HTTP status code alone. The response body includes
  `created_at`, which will differ if the document already existed.

