# Documents API Contract

Base path: `/api/v1/tenants/{tenant_id}/documents`

All endpoints require authentication via `Authorization: Bearer <token>` and a valid membership in the specified tenant. Tenant isolation is enforced via PostgreSQL Row-Level Security (RLS) and verified `TenantContext`.

---

## 1. Endpoints

| Method and Path | Success Status | Required Permission | Description / Notes |
|---|---|---|---|
| `POST /documents` | **202 Accepted** (new)<br>**200 OK** (duplicate) | `DocumentsUpload` | Multipart file upload. Deduplicated by `(tenant_id, content_hash)`. |
| `GET /documents` | **200 OK** | `DocumentsSearch` | Keyset-paginated list, ordered newest-first: `(created_at DESC, id DESC)`. |
| `GET /documents/{document_id}` | **200 OK** | `DocumentsSearch` | Fetches a single document by ID. |
| `GET /documents/{document_id}/status` | **200 OK** | `DocumentsSearch` | Lightweight status polling endpoint (`id`, `status`, `chunk_count`, `failure_code`, `updated_at`). |
| `DELETE /documents/{document_id}` | **204 No Content** | `DocumentsDeleteAny` or (`DocumentsDeleteOwn` + ownership) | Soft-deletes the document and removes the object from storage. |

---

## 2. Multipart Upload Contract

`POST /api/v1/tenants/{tenant_id}/documents`

### Request Requirements
- Header `Content-Type: multipart/form-data; boundary=...` is mandatory.
- Route timeout: **120 seconds** (overrides default 30s deadline to allow large uploads).
- Maximum declared body size: `MAX_UPLOAD_BYTES + 1 MiB` headroom (default: 21 MiB / 22,020,096 bytes).
- Maximum streamed file content size: `MAX_UPLOAD_BYTES` (default: 20 MiB / 20,971,520 bytes).

### Parts
| Field Name | Type | Required | Rules |
|---|---|---|---|
| `file` | Binary file | **Yes** | Exactly one `file` part allowed. Must have a non-empty filename. Extension must be `.pdf`, `.txt`, `.md`, or `.markdown`. Size must be between 1 byte and `MAX_UPLOAD_BYTES`. |
| `title` | Text | No | Optional. If provided, must be between 1 and 200 characters (trimmed). If omitted, defaults to the filename without its extension (truncated to at most 200 characters). |
| *Other* | Any | No | Any unknown part names are skipped without reading. |

### Validation Order (D9)
1. Request `Content-Type` is `multipart/form-data` (415).
2. Declared `Content-Length` within `MAX_UPLOAD_BYTES + 1 MiB` (413).
3. `file` part present and unique (422).
4. Filename sanitizes to a non-empty string, ≤ 255 chars, no control chars, not `.` or `..` (422).
5. Extension is one of `pdf`, `txt`, `md`, `markdown` (415).
6. Magic-byte sniffing: PDF files must begin with `%PDF-` (415).
7. Streamed byte count stays at or under `MAX_UPLOAD_BYTES` (413).
8. Text files (`.txt`, `.md`): valid UTF-8, no NUL bytes (`0x00`) (415).
9. File size > 0 bytes (422).

---

## 3. Keyset Pagination Contract

`GET /api/v1/tenants/{tenant_id}/documents`

Query parameters:
- `after_created_at` (optional): ISO-8601 timestamp (`DateTime<Utc>`) cursor from the previous page.
- `after_id` (optional): UUID cursor from the previous page.
- `limit` (optional): Integer, default `50`, clamped to `[1, 100]`.
- `status` (optional): Filter by document status (e.g., `pending`, `processing`, `ready`, `failed`).

Ordering:
- Strictly ordered by `created_at DESC, id DESC`.
- Soft-deleted documents (`deleted_at IS NOT NULL`) are excluded.

---

## 4. Full Error Table

All error responses use the standard JSON error envelope:
```json
{
  "error": {
    "code": "STRING_CODE",
    "message": "Human-readable safe explanation.",
    "request_id": "00000000-0000-0000-0000-000000000000"
  }
}
```

| Case | HTTP Status | Error `code` | Notes |
|---|---|---|---|
| Request is not `multipart/form-data` | **415** | `UNSUPPORTED_MEDIA_TYPE` | Missing or invalid Content-Type header. |
| Declared body over the route limit | **413** | `PAYLOAD_TOO_LARGE` | `Content-Length` > `MAX_UPLOAD_BYTES + 1 MiB`. |
| Streamed bytes over `MAX_UPLOAD_BYTES` | **413** | `PAYLOAD_TOO_LARGE` | Raised on the chunk that crosses the threshold. |
| Extension not accepted | **415** | `UNSUPPORTED_MEDIA_TYPE` | Not one of `pdf`, `txt`, `md`, `markdown`. |
| Body disagrees with the extension | **415** | `UNSUPPORTED_MEDIA_TYPE` | E.g., `.pdf` without `%PDF-` magic header. |
| Text file not valid UTF-8, or contains NUL | **415** | `UNSUPPORTED_MEDIA_TYPE` | Invalid byte sequence or embedded NUL byte. |
| No `file` part, two `file` parts, or bad filename | **422** | `VALIDATION_ERROR` | Multipart structure or filename validation failure. |
| Zero byte file | **422** | `VALIDATION_ERROR` | Empty upload stream. |
| `title` longer than 200 characters | **422** | `VALIDATION_ERROR` | Or empty when explicitly provided. |
| Caller lacks the permission | **403** | `FORBIDDEN` | E.g. Viewer attempting upload or delete. |
| Caller is not an active member, tenant unknown, or tenant ID malformed | **404** | `NOT_FOUND` | Concealment: cross-tenant and missing look identical. |
| Membership or tenant suspended | **403** | `FORBIDDEN` | Inactive status on tenant or membership. |
| Document ID belongs to another tenant, or does not exist, or is deleted | **404** | `NOT_FOUND` | Cross-tenant concealment via RLS. |
| Member deleting another member's document | **403** | `FORBIDDEN` | Caller has `DocumentsDeleteOwn` but is not uploader. |
| Object storage unreachable | **503** | `SERVICE_UNAVAILABLE` | Storage backend connection or I/O failure. |
| PostgreSQL unreachable | **503** | `SERVICE_UNAVAILABLE` | Database pool timeout, connection drop, or class 08 exception. |

---

## 5. Response Schemas

### `DocumentResponse`
Exposed on `POST /documents`, `GET /documents`, `GET /documents/{document_id}`.
```json
{
  "id": "11111111-1111-1111-1111-111111111111",
  "title": "Quarterly Report",
  "original_filename": "report.pdf",
  "content_hash": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
  "mime_type": "application/pdf",
  "size_bytes": 1048576,
  "status": "pending",
  "chunk_count": 0,
  "uploaded_by": "22222222-2222-2222-2222-222222222222",
  "created_at": "2026-11-13T00:00:00Z",
  "updated_at": "2026-11-13T00:00:00Z"
}
```
*Note: `object_key`, `failure_detail`, and `deleted_at` are intentionally excluded to prevent leaking storage layout and personal information.*

### `DocumentStatusResponse`
Exposed on `GET /documents/{document_id}/status`.
```json
{
  "id": "11111111-1111-1111-1111-111111111111",
  "status": "pending",
  "chunk_count": 0,
  "failure_code": null,
  "updated_at": "2026-11-13T00:00:00Z"
}
```

