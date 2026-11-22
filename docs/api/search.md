# Search API Contract

`POST /api/v1/tenants/{tenant_id}/search`

Hybrid search over a tenant's indexed documents: PostgreSQL full-text search and pgvector similarity, merged with Reciprocal Rank Fusion. Design decisions are in [ADR-008](../adr/ADR-008-hybrid-search.md).

Requires `Authorization: Bearer <token>` and a membership with the `DocumentsSearch` permission (every role, including Viewer). Tenant isolation is enforced by RLS and by an explicit tenant filter in every query.

---

## 1. Request

```json
{
  "query": "how much annual leave do I get",
  "limit": 10,
  "document_ids": ["8d1b0b9e-0c2f-4a53-9e8e-5a3f1a7d2c11"]
}
```

| Field | Type | Required | Rules |
|---|---|---|---|
| `query` | string | yes | Trimmed. 1 to 1000 characters (characters, not bytes). |
| `limit` | integer | no | 1 to 50. Default 10. Out-of-range values are rejected, never clamped. |
| `document_ids` | array of UUID | no | At most 100. Duplicates are removed. `[]` means no filter. Ids of other tenants' documents match nothing. |

Unknown fields are rejected. Query syntax is forgiving: `"exact phrase"`, `-excluded` and `OR` work, and malformed syntax never causes an error.

## 2. Response `200 OK`

```json
{
  "mode": "hybrid",
  "results": [
    {
      "document_id": "8d1b0b9e-0c2f-4a53-9e8e-5a3f1a7d2c11",
      "chunk_index": 3,
      "chunk_id": "2f6f3c0e-6f0e-4a8b-bd0e-3f4a0c7f9d10",
      "document_title": "Employee Handbook",
      "snippet": "Employees accrue annual leave monthly … carries over up to ten days",
      "page_start": 12,
      "page_end": 12,
      "heading": "Leave policy",
      "score": 0.0325,
      "vector_rank": 1,
      "lexical_rank": 2
    }
  ]
}
```

An empty `results` array is a valid `200`. Responses never include full chunk text, storage keys or embeddings.

| Field | Meaning |
|---|---|
| `mode` | `hybrid`: both branches ran. `lexical_only`: the query could not be embedded in time (provider timeout or error), so only keyword search ran. The request still succeeds. |
| `snippet` | Plain text excerpt around the matching words. No markup. |
| `score` | Reciprocal Rank Fusion score. **Relative within one response only. Do not compare scores across queries or threshold on them.** |
| `vector_rank`, `lexical_rank` | 1-based position in each branch, or `null` when the chunk was not a candidate there. |
| `page_start`, `page_end`, `heading` | From the chunk metadata. `null` when the source has none (plain text has no pages). |

### Citation key

Cite a result by **`(document_id, chunk_index)`**. `chunk_id` is returned for debugging, but it changes whenever a document is reprocessed, because chunks are replaced.

### Ranking note

Lexical ranking uses PostgreSQL `ts_rank_cd`. It is **not** BM25.

## 3. Errors

| Case | Status | Code |
|---|---|---|
| No or invalid token | 401 | `UNAUTHORIZED` |
| Not a member, unknown tenant or malformed tenant id | 404 | `NOT_FOUND` |
| Suspended membership or tenant | 403 | `FORBIDDEN` |
| Empty or over-long query, bad `limit`, more than 100 `document_ids`, unknown field, missing `query`, wrong field types | 422 | `VALIDATION_ERROR` |
| Body is not valid JSON, or the `Content-Type` is not `application/json` | 400 | `BAD_REQUEST` |
| Database unavailable or statement timeout | 503 | `SERVICE_UNAVAILABLE` |

## 4. What is searchable

A document's chunks are searchable once ingestion has written them, and stay searchable while the document is reprocessed, until the new set atomically replaces them. Soft-deleted documents never appear. See [documents.md](documents.md) for the processing lifecycle.
