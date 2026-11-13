# ADR-005: Object Storage Abstraction (`FileStorage` trait)

**Status**: Accepted  
**Date**: 2026-11-09  
**Deciders**: Platform Engineering  
**Phase**: 3 — Document Upload

---

## Context

Phase 3 adds document upload to the multi-tenant platform. Uploaded files must be
stored durably and retrieved by an opaque, tenant-scoped key. Three deployment
environments need distinct backends:

| Environment | Backend needed |
|---|---|
| Unit / integration tests | In-memory (zero infrastructure, deterministic) |
| Local development | Local filesystem or MinIO (Docker Compose) |
| Production | AWS S3 or S3-compatible (MinIO / Silo) |

A naive approach — writing three separate upload code paths or using `object_store`'s
concrete types directly throughout the codebase — would scatter infrastructure
concerns across the domain layer and make test injection impossible without
conditional compilation.

Additionally, S3 multipart uploads require an explicit `CompleteMultipartUpload`
call; abandoning a multipart upload without calling `AbortMultipartUpload` leaves
incomplete parts in the bucket, accruing storage costs indefinitely.

---

## Decision

Introduce a `crates/common-storage` crate that wraps the `object_store` crate
behind two purpose-built traits:

```
FileStorage          — creates writers; lists objects; deletes objects
  └─ begin_write(key) → Box<dyn ObjectWriter>

ObjectWriter         — streams bytes into an object
  ├─ write(&[u8])
  ├─ finish() → object_key   (commits; triggers CompleteMultipartUpload on S3)
  └─ abort()                 (cancels; triggers AbortMultipartUpload on S3)
```

`AppState` holds the backend as `Arc<dyn FileStorage + Send + Sync>`. Three
concrete implementations are provided:

| Impl | Used in |
|---|---|
| `InMemoryStorage` | Tests (default when `OBJECT_STORAGE_BACKEND` is absent) |
| `LocalStorage` | Offline development with a local filesystem path |
| `S3Storage` | Staging and production (wraps `object_store::aws::AmazonS3`) |

Test helpers expose `AppState::with_storage(impl FileStorage)` so individual
tests can inject a fresh in-memory backend without touching global state.

---

## Rationale

### Why wrap `object_store` instead of using it directly?

`object_store` provides a unified `ObjectStore` trait, but it is designed for
byte-range reads and bulk operations. Its `PutPayload` / `MultipartUpload` APIs
differ between sync and async styles across versions, and its trait bounds are
not `dyn`-safe without boxing. By wrapping it behind `FileStorage` / `ObjectWriter`:

- The domain layer (`document_service`) depends only on our narrow trait, not on
  `object_store` version details.
- Backend swaps (e.g., GCS instead of S3) require changes only in `common-storage`,
  not in any domain or handler code.
- Tests use `InMemoryStorage` without linking `object_store`'s S3 feature at all.

### Why an explicit `finish()` / `abort()` contract?

S3 multipart uploads are started with `CreateMultipartUpload` and must be
concluded with either `CompleteMultipartUpload` (commit) or `AbortMultipartUpload`
(cancel). An `ObjectWriter` dropped without calling either leaks parts that remain
billable indefinitely.

The explicit contract makes the two outcomes visible at the type level and in code
review. `abort()` is called in the error path of `document_service::upload()`;
`finish()` is called only after the SHA-256 and size are confirmed correct. An S3
lifecycle rule (see `docs/runbooks/orphan-cleanup.md`) provides a safety net for
processes that crash before reaching either branch.

### Why `Arc<dyn FileStorage>` in `AppState`?

Axum handlers clone `AppState` per request; `Arc` avoids copying the backend.
`dyn FileStorage` (object-safe) means the concrete type is chosen once at startup
and injected through a single code path, rather than propagated as a generic
parameter that would infect every handler signature.

---

## Consequences

**Positive**:
- Tests have zero infrastructure requirements for the storage layer; `cargo test
  --workspace` runs fully offline by default.
- S3 upload errors surface through `StorageError` before the DB transaction opens,
  so a failed upload never leaves a dangling DB row.
- Adding a new backend (GCS, Azure Blob) requires only a new `impl FileStorage`
  in `common-storage`.

**Negative / Trade-offs**:
- The `ObjectWriter` trait requires `async_trait` for object safety, adding a
  small runtime overhead per method call (one heap allocation for the future).
- `InMemoryStorage` stores full object bytes in RAM; large uploads in tests that
  do not bound file size could OOM the test runner. Upload tests must use small
  fixtures.
- The ADR sequence (ADR-005) no longer matches §22 of the original design
  document, which had a different numbering. The design doc is considered
  superseded on this point.

