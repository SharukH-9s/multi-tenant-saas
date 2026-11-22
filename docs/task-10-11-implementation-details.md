# Implementation Details: Task 10 & Task 11
> **Dedicated Schema Redesign — Phase 5**  
> Covers: `ChunkTarget` domain types, request context propagation, type-safe dynamic table routing in search, chunk & ingestion repositories, and regression guards.

---

## 1. Architectural Overview

The goal of Phase 5 (Tasks 10 & 11) is to introduce support for routing document chunk reads and writes to dedicated PostgreSQL schemas (`tenant_<id>.document_chunks`) for promoted high-volume tenants, while keeping unpromoted tenants on the shared `public.document_chunks` table—with **zero regressions** on existing endpoints, tests, or CI grep guards.

```
                  ┌────────────────────────────────────────────────────────┐
                  │                 HTTP / API Layer                       │
                  │  TenantCtx(RequestContext)                             │
                  │  ├─ TenantContext (identity, role, RBAC)               │
                  │  └─ ChunkTarget (Shared | Dedicated)                   │
                  └────────────────────────┬───────────────────────────────┘
                                           │
                    ┌──────────────────────┴──────────────────────┐
                    ▼                                             ▼
       ┌────────────────────────┐                   ┌────────────────────────┐
       │     search_service     │                   │    document_service    │
       │    &ctx.chunk_target   │                   │   &ctx.chunk_target    │
       └────────────┬───────────┘                   └─────────────┬──────────┘
                    │                                             │
                    ▼                                             ▼
       ┌────────────────────────┐                   ┌────────────────────────┐
       │      search_repo       │                   │       chunk_repo       │
       │ vector_candidates(...) │                   │ delete_for_document    │
       │ lexical_candidates(...)│                   │ replace_for_document   │
       │ hydrate(...)           │                   │ count_for_document     │
       └────────────┬───────────┘                   └─────────────┬──────────┘
                    │                                             │
                    └──────────────────────┬──────────────────────┘
                                           │
                                           ▼
                       PostgreSQL (search_path = public)
                         ├─ public.document_chunks (99% shared)
                         └─ tenant_<id>.document_chunks (promoted)
                                (JOINs always qualify public.documents)
```

---

## 2. Task 10: Routing Domain & Request Context

### 2.1 Keeping `common-auth` Pure (Decision #3)
Rather than polluting `common-auth` with database storage layout, `common-auth::TenantContext` remains untouched. Routing types were established strictly inside `apps/tenant-saas`.

### 2.2 `ValidatedSchema` and `ChunkTarget`
* **File:** [`apps/tenant-saas/src/domain/chunk.rs`](../apps/tenant-saas/src/domain/chunk.rs)

#### Key Invariants & PostgreSQL 63-Byte Identifier Limit:
PostgreSQL's identifier limit (`NAMEDATALEN - 1`) is 63 bytes total. Because the prefix `tenant_` is 7 bytes, the suffix must be at most 56 bytes (`63 - 7 = 56`). Suffixes beyond 56 bytes get silently truncated by PostgreSQL, which would cause the `tenants.dedicated_schema` catalog string to diverge from `pg_namespace.nspname`.
To enforce this, `MAX_SCHEMA_SUFFIX_LEN` is set to 56, validated against `^tenant_[a-z0-9_]{1,56}$`, and backed by migration `008_tighten_dedicated_schema_length.sql`.

```rust
/// Maximum allowed length of the suffix that follows `tenant_` in a schema name.
///
/// PostgreSQL identifiers are capped at `NAMEDATALEN - 1 = 63` bytes.
/// Because our schemas are prefixed with `tenant_` (7 bytes), the suffix must
/// not exceed `63 - 7 = 56` bytes so the entire schema name never exceeds 63 bytes.
const MAX_SCHEMA_SUFFIX_LEN: usize = 56;

/// A PostgreSQL schema name validated against `^tenant_[a-z0-9_]{1,56}$`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedSchema(String);

impl ValidatedSchema {
    pub fn from_db(raw: &str) -> Result<Self, AppError> {
        if is_valid_schema(raw) {
            Ok(Self(raw.to_owned()))
        } else {
            Err(AppError::Internal(format!(
                "dedicated_schema value '{raw}' violates the naming invariant \
                 (^tenant_[a-z0-9_]{{1,56}}$)"
            )))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Which `document_chunks` table a chunk read or write targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChunkTarget {
    Shared,
    Dedicated(ValidatedSchema),
}

impl ChunkTarget {
    pub fn from_db_opt(raw: Option<&str>) -> Result<Self, AppError> {
        match raw {
            None => Ok(Self::Shared),
            Some(s) => ValidatedSchema::from_db(s).map(Self::Dedicated),
        }
    }

    pub fn table(&self) -> Cow<'static, str> {
        match self {
            Self::Shared => Cow::Borrowed("public.document_chunks"),
            Self::Dedicated(schema) => Cow::Owned(format!("{}.document_chunks", schema.as_str())),
        }
    }

    pub fn is_shared(&self) -> bool {
        matches!(self, Self::Shared)
    }
}
```

### 2.3 `RequestContext` and Transparent Deref Coercion
* **File:** [`apps/tenant-saas/src/auth/request_context.rs`](../apps/tenant-saas/src/auth/request_context.rs)

`RequestContext` bundles tenant identity with chunk routing. Implementing `Deref<Target = TenantContext>` ensures backwards compatibility with any existing service accepting `&TenantContext`.

```rust
#[derive(Debug, Clone)]
pub struct RequestContext {
    inner: TenantContext,
    pub chunk_target: ChunkTarget,
}

impl RequestContext {
    pub(crate) fn new(inner: TenantContext, chunk_target: ChunkTarget) -> Self {
        Self { inner, chunk_target }
    }

    /// Factory for test suites outside HTTP middleware.
    pub fn for_shared(inner: TenantContext) -> Self {
        Self::new(inner, ChunkTarget::Shared)
    }
}

impl Deref for RequestContext {
    type Target = TenantContext;

    fn deref(&self) -> &TenantContext {
        &self.inner
    }
}
```

### 2.4 Middleware & Axum Extractor
* **File:** [`apps/tenant-saas/src/auth/tenant_resolver.rs`](../apps/tenant-saas/src/auth/tenant_resolver.rs)
* **File:** [`apps/tenant-saas/src/auth/extractors.rs`](../apps/tenant-saas/src/auth/extractors.rs)

The `tenant_resolver` queries membership and tenant metadata in a single query:
```rust
let chunk_target = ChunkTarget::from_db_opt(row.dedicated_schema.as_deref())
    .map_err(|e| AppError::Internal(format!("chunk target resolution: {e}")))?;

Ok(RequestContext::new(
    TenantContext::from_verified_membership(tenant_id, user_id, role),
    chunk_target,
))
```
Axum handler extractor:
```rust
#[derive(Debug, Clone)]
pub struct TenantCtx(pub RequestContext);
```

### 2.5 Ingestion Worker Boundary Resolution & Error Classification
* **File:** [`apps/tenant-saas/src/repositories/tenant_repo.rs`](../apps/tenant-saas/src/repositories/tenant_repo.rs)
* **File:** [`apps/tenant-saas/src/services/ingestion_service.rs`](../apps/tenant-saas/src/services/ingestion_service.rs)

Background workers do not process incoming HTTP requests and have no `TenantCtx`. Instead, they query `tenants.dedicated_schema` dynamically at the execution transaction boundary:
```rust
// apps/tenant-saas/src/services/ingestion_service.rs
let mut tx = begin_system_tenant_tx(&deps.state.db, claimed.tenant_id).await?;

let chunk_target = match crate::repositories::tenant_repo::get_dedicated_schema(&mut tx, claimed.tenant_id).await {
    Ok(target) => target,
    Err(AppError::NotFound { .. }) => {
        // Tenant was deleted between claim and persist — supersede job
        return Ok(Stop::Superseded);
    }
    Err(AppError::Internal(msg)) => {
        // Malformed dedicated_schema invariant violation — poison pill, fail without retry
        return Ok(Stop::Failed(msg));
    }
    Err(other) => {
        // Transient DB or connection error — retryable
        return Ok(Stop::Unavailable(other.to_string()));
    }
};

ingestion_repo::complete(
    &mut tx,
    &chunk_target,
    claimed,
    &deps.state.config.ingestion.embedding_model,
    EMBEDDING_VERSION,
    &new_chunks,
).await?;
```

---

## 3. Task 11: Dynamic Table Routing in Repositories & Services

Repositories accept a validated `&ChunkTarget` (rather than a raw `&str`) to preserve compile-time safety and prevent bypassing schema validation. The repository extracts `let chunk_table = target.table();` right before passing the interpolated string into `sqlx::AssertSqlSafe(sql.as_str())`.

### 3.1 Type-Safe Dynamic SQL in `search_repo.rs`
* **File:** [`apps/tenant-saas/src/repositories/search_repo.rs`](../apps/tenant-saas/src/repositories/search_repo.rs)

#### Vector Candidate Query:
```rust
pub async fn vector_candidates(
    tx: &mut Tx,
    target: &ChunkTarget,
    tenant_id: Uuid,
    query_vec: &[f32],
    doc_filter: Option<&[Uuid]>,
    n: i64,
) -> Result<Vec<Candidate>, AppError> {
    let chunk_table = target.table();
    let sql = format!(
        "WITH nearest AS MATERIALIZED ( \
             SELECT id, embedding <=> $2::vector AS distance \
               FROM {chunk_table} \
              WHERE tenant_id = $1 \
                AND ($3::uuid[] IS NULL OR document_id = ANY($3)) \
              ORDER BY embedding <=> $2::vector \
              LIMIT $4 \
         ) \
         SELECT id FROM nearest ORDER BY distance, id"
    );
    let rows: Vec<(Uuid,)> = sqlx::query_as(sqlx::AssertSqlSafe(sql.as_str()))
        .bind(tenant_id)
        .bind(vector_literal(query_vec))
        .bind(doc_filter)
        .bind(n)
        .fetch_all(&mut **tx)
        .await
        .map_err(map_db_error)?;
    Ok(to_candidates(rows))
}
```

#### Lexical Candidate Query:
```rust
pub async fn lexical_candidates(
    tx: &mut Tx,
    target: &ChunkTarget,
    tenant_id: Uuid,
    query: &str,
    doc_filter: Option<&[Uuid]>,
    n: i64,
) -> Result<Vec<Candidate>, AppError> {
    let chunk_table = target.table();
    let sql = format!(
        "SELECT c.id \
           FROM {chunk_table} c, websearch_to_tsquery('english', $2) AS q \
          WHERE c.tenant_id = $1 \
            AND ($3::uuid[] IS NULL OR c.document_id = ANY($3)) \
            AND c.tsv_content @@ q \
          ORDER BY ts_rank_cd(c.tsv_content, q) DESC, c.id \
          LIMIT $4"
    );
    let rows: Vec<(Uuid,)> = sqlx::query_as(sqlx::AssertSqlSafe(sql.as_str()))
        .bind(tenant_id)
        .bind(query)
        .bind(doc_filter)
        .bind(n)
        .fetch_all(&mut **tx)
        .await
        .map_err(map_db_error)?;
    Ok(to_candidates(rows))
}
```

#### Hydration Query with Explicit Schema Qualification:
```rust
pub async fn hydrate(
    tx: &mut Tx,
    target: &ChunkTarget,
    tenant_id: Uuid,
    chunk_ids: &[Uuid],
    query: &str,
) -> Result<Vec<HitRow>, AppError> {
    let chunk_table = target.table();
    let sql = format!(
        "SELECT c.id, c.document_id, c.chunk_index, c.content, c.metadata, \
                c.embedding_version, d.title, \
                ts_headline('english', c.content, websearch_to_tsquery('english', $3), \
                    'StartSel=\"\", StopSel=\"\", MaxFragments=2, MinWords=10, MaxWords=35, \
                     FragmentDelimiter=\" \u{2026} \"') AS snippet \
           FROM {chunk_table} c \
           JOIN public.documents d ON d.tenant_id = c.tenant_id AND d.id = c.document_id \
          WHERE c.tenant_id = $1 AND c.id = ANY($2) AND d.deleted_at IS NULL"
    );
    sqlx::query_as::<_, HitRow>(sqlx::AssertSqlSafe(sql.as_str()))
        .bind(tenant_id)
        .bind(chunk_ids)
        .bind(query)
        .fetch_all(&mut **tx)
        .await
        .map_err(map_db_error)
}
```

### 3.2 Dynamic Routing in `chunk_repo.rs` & `ingestion_repo.rs`
* **File:** [`apps/tenant-saas/src/repositories/chunk_repo.rs`](../apps/tenant-saas/src/repositories/chunk_repo.rs)
* **File:** [`apps/tenant-saas/src/repositories/ingestion_repo.rs`](../apps/tenant-saas/src/repositories/ingestion_repo.rs)

Methods accept `target: &ChunkTarget` (or `&crate::domain::chunk::ChunkTarget`) and execute dynamic statements wrapped in `sqlx::AssertSqlSafe(sql.as_str())`:
* `chunk_repo::delete_for_document(tx, target, document_id)`
* `chunk_repo::count_for_document(tx, target, document_id)`
* `chunk_repo::replace_for_document(tx, target, tenant_id, document_id, model, version, chunks)`
* `ingestion_repo::complete(tx, target, claimed, model, version, chunks)`

### 3.3 Dynamic Routing in `search_service.rs` & `document_service.rs`
* **File:** [`apps/tenant-saas/src/services/search_service.rs`](../apps/tenant-saas/src/services/search_service.rs)
* **File:** [`apps/tenant-saas/src/services/document_service.rs`](../apps/tenant-saas/src/services/document_service.rs)

In `search_service.rs`:
```rust
let vector = match &query_vec {
    Some(v) => search_repo::vector_candidates(&mut tx, &ctx.chunk_target, tenant_id, v, filter, ...).await?,
    None => Vec::new(),
};
let lexical = search_repo::lexical_candidates(&mut tx, &ctx.chunk_target, tenant_id, &q.text, filter, ...).await?;
...
let rows = search_repo::hydrate(&mut tx, &ctx.chunk_target, tenant_id, &ids, &q.text).await?;
```

In `document_service.rs`:
```rust
chunk_repo::delete_for_document(&mut tx, &ctx.chunk_target, id).await?;
```

---

## 4. Defenses & Regression Protection

| Risk Area | Why It Was Vulnerable | Defense Implemented |
|---|---|---|
| **SQL Injection** | Table name cannot be bound via `$1` in SQL queries. | Type-safe `&ChunkTarget` ensures only compile-time `Shared` or validated regex `^tenant_[a-z0-9_]{1,56}$` can reach `AssertSqlSafe`. All data values remain parameterized (`$1`, `$2`, etc.). |
| **Postgres 63-Byte Identifier Truncation** | Postgres identifiers cap at 63 bytes total. Suffixes > 56 chars silently truncate in catalog. | Capped suffix at 56 bytes (`MAX_SCHEMA_SUFFIX_LEN = 56`). Enforced in domain unit tests and DB migration `008_tighten_dedicated_schema_length.sql`. |
| **Cross-Schema Joins** | When chunk table is `tenant_<id>.document_chunks`, unqualified `JOIN documents` looks for `tenant_<id>.documents` which does not exist. | Explicitly qualified as `JOIN public.documents d ON d.tenant_id = c.tenant_id AND d.id = c.document_id`. |
| **Worker Synchronization** | Ingestion worker has no HTTP `RequestContext`. Hardcoding would cause documents to persist to `public` while search queries dedicated schema. | Worker calls `tenant_repo::get_dedicated_schema(&mut tx, claimed.tenant_id)` at the database transaction boundary, with fine-grained error classification (`NotFound`, `Internal`, `Transient`). |
| **Document Deletion** | Deleting a document from a promoted tenant would leave orphaned chunks if hardcoded to `public`. | `document_service::delete` passes `&ctx.chunk_target` to `chunk_repo::delete_for_document`. |
| **CI Grep Guard Integrity** | Grep guard forbids `from_verified_membership` outside `tenant_resolver.rs` and `begin_system_tenant_tx` outside workers/ingestion. | Pure auth design left `TenantContext` constructor unchanged. Broadened pure-module guard to `apps/tenant-saas/src/domain/`. |

---

## 5. Test Suite Verification

### CI Grep Guards
* `from_verified_membership`: 0 violations
* `object_key` in handlers: 0 violations
* `begin_system_tenant_tx` outside worker/ingestion: 0 violations
* `sqlx` in pure modules (`ingestion/`, `ai/`, `domain/`): 0 violations

### Automated Test Runs
* **Unit Tests (`cargo test --lib`)**: 201 passed; 0 failed
* **Integration Tests (`cargo test --test integration search_repo`)**: 9 passed; 0 failed
* **Search Service Tests (`cargo test --test integration search::`)**: 6 passed; 0 failed
* **Security & RLS Tests (`cargo test --test security cross_tenant_search`)**: 8 passed; 0 failed
* **Dedicated Schema E2E & Query Plan Tests (`cargo test --test integration dedicated_schema`)**: All passed; 0 failed
* **Linter (`cargo clippy --workspace --tests -- -D warnings`)**: 0 warnings
