# Phase 5: Hybrid Search Implementation Plan

> Source of truth: `multi_tenant_ai_saas_system_design.md` §9.1, §9.2, §11, §13.5, §17, §19.5, §23 (Phase 5).
> Builds on: Phases 1–4 (all complete; Phase 4 = commit `35e7e5b`).
> Budget: Week 6, about 30–35 focused hours.

**How to use this plan.** §1 lists every decision that is already settled, with its source. Do not reopen those decisions during Phase 5. If a task seems to need one changed, stop, and record the case as a new ADR before writing code. §4 lists the only new decisions this phase makes. Everything else in the plan follows from §1 and §4.

---

## 0. Goal and exit criteria

A member of a tenant sends a query and gets back the most relevant chunks from that tenant's documents. Each result carries a snippet and a stable citation key. Results combine a keyword (full-text) ranking and a meaning (vector) ranking, merged with Reciprocal Rank Fusion (RRF). The phase also measures how good and how fast this is.

Phase 5 is read-only. It adds one migration, one endpoint, one service and one repository. The ingestion worker is unchanged, unless the chunk-boundary analysis in T9 shows the chunker needs a fix.

| Exit criterion (design §23) | Proof |
|---|---|
| Search results are tenant-scoped | `tests/security/cross_tenant_search.rs`: tenant B's search never returns A's chunks, through the API or through direct repository calls with B's context. An identical document in both tenants returns only the caller's copy |
| Exact keyword and semantic queries work | `tests/integration/search.rs`: an identifier query (`SKU-4471`) ranks its chunk first. A query whose words appear nowhere in the corpus still finds the chunk through the vector branch. The eval smoke test in CI keeps exact-keyword Recall@10 at 1.0 |
| Recall@10 and latency are measured | `docs/benchmarks/phase5-search.md`: Recall@5/10, MRR and nDCG@10 from the eval harness with a **real** embedding provider, plus P50/P95/P99 at 10k and 100k chunks |
| Query plans are reviewed | The same report contains `EXPLAIN (ANALYZE, BUFFERS)` for a small tenant and a large tenant, with a written reading of each |
| (design deliverable) Chunk-boundary retrieval analysis | Same report: boundary-query Recall@10 versus overall, and the decision T9 made |

Earlier properties that must not regress: every Phase 1–4 test passes unchanged, the CI grep guards pass (including `begin_system_tenant_tx` staying out of search), and no search log line contains the query text.

---

## 1. Locked decisions: do not reopen

| # | Decision | Source | What it means for Phase 5 |
|---|---|---|---|
| L1 | Modular monolith, API and worker processes, no microservices | ADR-001, design §5.1 | Search lives in the API process. No search service, no new binary |
| L2 | Rust + Axum + Tokio + SQLx | ADR-002 | No new framework or ORM |
| L3 | Shared PostgreSQL schema, tenant isolation by RLS **plus** an explicit tenant filter | ADR-003, design §7, §11.6 | Search SQL filters `tenant_id = $tenant` **and** runs under `begin_tenant_tx` so RLS applies |
| L4 | `TenantContext` built only by the tenant resolver. Concealment: a foreign tenant or document is 404, a missing permission is 403 | Phase 2 D7, CI grep guard | Search uses the `TenantCtx` extractor. Permission `DocumentsSearch` (every role has it) |
| L5 | No transaction is held across network I/O | design §7.3.1 | The query is embedded **before** the transaction opens |
| L6 | PostgreSQL FTS + pgvector fused by RRF. `ts_rank_cd` is not BM25. Real BM25 would be a separate architecture change | design §11.3 | No ParadeDB, no Elasticsearch, no BM25 claims anywhere in docs or the API |
| L7 | RRF `k = 60`, 30 vector + 30 lexical candidates, top 10 by default | design §11.4, §11.5 | Starting values. Tuning happens only from eval evidence (T7) |
| L8 | pgvector ≥ 0.8.0 with iterative index scans. No partitioning unless a benchmark proves it necessary | design §9.2 | One HNSW index. Partitioning is out of scope |
| L9 | `document_chunks` has `vector(1536)`, `embedding_model` and `embedding_version`. `EMBEDDING_DIM` is fixed at 1536 | ADR-007 §6, Phase 4 | No schema change to vectors. Phase 5 adds only the `tsvector` column and two indexes |
| L10 | `AppState.embedder` exists in the API for query embedding. The fake embedder has **no semantic meaning** | Phase 4 D11, `ai/fake.rs` | The same provider and model embed queries. Relevance numbers come only from a real provider |
| L11 | Chunks exist only after a successful persist, are replaced atomically on reprocess, and are deleted in the same transaction as a document delete | Phase 4 D3, D9 | The final "is this document live?" check is `documents.deleted_at IS NULL` (S4) |
| L12 | `begin_system_tenant_tx` is worker-only | Phase 4, CI guard | Search uses `begin_tenant_tx(&state.db, &ctx)` |
| L13 | Errors use the standard envelope. 422 for invalid input, 503 for unavailable dependencies | Phase 1, Phase 3 D12 | No new error machinery. One new mapping: statement timeout → 503 (T3) |
| L14 | Logs never contain document text, file names, object keys or secrets | Phase 3 D3, Phase 4 D13 | Search logs never contain the query text or snippets |
| L15 | Semantic caching is not part of the first search release | design §17 | No cache in Phase 5 |
| L16 | Per-tenant rate limits and quotas belong to Phase 7 | design §15, §23 | Search adds no limiter (risk recorded in §8) |

**Explicitly out of Phase 5:** RAG, prompts and SSE (Phase 6). Rate limits, quotas and OpenTelemetry (Phase 7). Result caching. Re-ranking models. Multi-language FTS. Tenant partitioning. Pagination of search results. Any new crate, process or Redis usage.

---

## 2. Baseline (what exists after Phase 4)

| Area | State | Phase 5 use |
|---|---|---|
| `document_chunks` | `content`, `token_count`, `embedding vector(1536)`, `embedding_model`, `embedding_version = "p4-v1"`, `metadata {page_start, page_end, heading}`, `UNIQUE (tenant_id, document_id, chunk_index)`, RLS ENABLE without FORCE (Phase 4 D2) | Searched directly. T1 adds `tsv_content` plus a GIN and an HNSW index |
| Embedding | `EmbeddingProvider` trait, `FakeEmbedder`, `OpenAiCompatibleProvider`, `validate_vectors`, `AppState.embedder` plus `with_embedder` | Query embedding: one call with one input |
| Tenant tx | `begin_tenant_tx(&pool, &ctx)` sets tenant and user ids, transaction-local | Used for the single search transaction |
| Routes | Tenant sub-routers merged under `/api/v1/tenants/{tenant_id}` in `routes.rs` | Add `handlers::search::router()` beside documents |
| Body extractor | `ApiJson<T>` gives the standard 422 envelope on bad JSON | Search request body |
| Tests | `TestApp`, `create_session`, `create_tenant`, `add_member`, `upload_file`. Phase 4 `ingestion_support` can run `ingestion_service::process` directly with any embedder | Search tests ingest real fixtures, then search |
| Performance | `tests/performance/` is empty. `k6` is installed on the dev machine | T8 adds the benchmark tooling |
| pgvector | 0.8.0 is installed, and CI asserts the version | HNSW + iterative scan |

---

## 3. Facts verified against the running database (2026-11-15)

These were checked on the Compose stack (`pgvector/pgvector:0.8.0-pg16`) and drive several decisions. Do not re-derive them.

| # | Fact | Consequence |
|---|---|---|
| F1 | The valid values of `hnsw.iterative_scan` are `off`, `strict_order` and `relaxed_order`. **The design doc's `'relaxed'` is wrong.** On a fresh connection, before the vector library loads, `SET LOCAL hnsw.iterative_scan = 'relaxed'` is accepted as a placeholder, then **silently reset to `off`** with only a WARNING once the first vector query runs | Use `relaxed_order`. A test reads the setting back **after** a vector expression in the same transaction (T3). T0 corrects the design doc |
| F2 | `relaxed_order` may return rows slightly out of distance order | The vector query wraps the index scan in a `MATERIALIZED` CTE and re-sorts by distance outside it (pgvector's documented pattern) |
| F3 | Other settings: `hnsw.ef_search` (default 40), `hnsw.max_scan_tuples` (default 20000), `hnsw.scan_mem_multiplier` (default 1). `app_runtime` may `SET LOCAL` them | Set `ef_search` per transaction. Leave the other two at their defaults |
| F4 | `ALTER TABLE ... ADD COLUMN tsv_content tsvector GENERATED ALWAYS AS (to_tsvector('english'::regconfig, content)) STORED`, a GIN index and an HNSW index (`m = 16, ef_construction = 64`) all succeed inside one transaction as `db_owner`, and backfill the existing 206 chunks | Plain sqlx migration. No `CONCURRENTLY` needed at today's size (note for production in §8) |
| F5 | With `tenant_id = $1` in the query, the planner can choose the btree `(tenant_id, document_id, chunk_index)` index and an **exact** sort instead of HNSW when the tenant is small | Small tenants get exact results. That is correct and expected, not a bug. The benchmark must test a large tenant to exercise HNSW |
| F6 | `websearch_to_tsquery('english', ...)` never raises on user syntax. It supports quotes, `OR` and `-`. A query of only stop words becomes an empty query (NOTICE, no error) | Use it for every lexical query. An empty query just means zero lexical candidates |
| F7 | `'english'` tokenizes `SKU-4471` as `sku` plus `-4471` and matches the same way at query time. `v2.1` stays one token | Identifiers and codes work with the stock parser. The eval set covers them |
| F8 | `ts_headline(..., 'StartSel="", StopSel=""')` returns plain-text fragments, strips HTML tags, and falls back to the start of the text when the query is empty | Snippets come from `ts_headline` with empty selectors |

---

## 4. New decisions for Phase 5

Each decision gives a choice, the reason, and the alternative that was rejected. ADR-008 (T0) records them once.

**S1. One endpoint, one service, one repository.** `POST /api/v1/tenants/{tenant_id}/search` → `search_service::search` → `search_repo`. *Rejected:* a separate search module or crate. Nothing outside the API needs it yet.

**S2. Embed first, then one short transaction.** The service embeds the query (timeout `SEARCH_EMBED_TIMEOUT_MS`, default 3000), then opens one `begin_tenant_tx`. That transaction runs four statements: settings, vector candidates, lexical candidates, and hydration of the final rows. Then it commits. *Rejected:* a single giant SQL statement with RRF inside. It is harder to read, test and `EXPLAIN` per branch, and §11.6 itself warns that one round trip does not guarantee speed.

**S3. If embedding fails, degrade to lexical-only.** When the provider times out or errors, search still runs the lexical branch and returns `"mode": "lexical_only"` with a `warn` log. If the database fails, the response is 503. *Rejected:* failing the whole request. Keyword search is still useful, and the client can see that the result was degraded.

**S4. Candidates come from `document_chunks` only. Liveness is checked at hydration.** Both candidate queries touch only `document_chunks` (tenant filter, optional `document_ids` filter), so each stays a single-table, index-friendly query. Hydration joins `documents` and requires `deleted_at IS NULL`. It deliberately does **not** require `status = 'ready'`. By L11 a chunk exists only for a document whose last successful run wrote it, so during a reprocess the old chunks stay searchable until they are atomically replaced. *Rejected:* filtering `status = 'ready'` inside the candidate queries. That puts a join in front of the HNSW scan, and it would also hide a document while it is being reprocessed.

**S5. `document_ids` filters inside both candidate queries.** `AND ($n::uuid[] IS NULL OR document_id = ANY($n))`. Filtering before ranking keeps recall when the filter is selective. Design §11.2's diagram filters after RRF, but this is what that diagram intends, done earlier. *Rejected:* filtering after fusion, which can leave fewer than `limit` results.

**S6. FTS uses one config, `'english'`.** It is a generated `STORED` column, ranked with `ts_rank_cd(tsv_content, q)`, and queries go through `websearch_to_tsquery` (F6). *Rejected:* `'simple'` (no stemming, worse paraphrase recall) and per-tenant language selection (no requirement yet).

**S7. Vector search uses cosine with explicit settings.** The operator is `<=>` with `vector_cosine_ops`. Inside the search transaction:

```sql
SELECT set_config('hnsw.ef_search', $1, true),          -- SEARCH_EF_SEARCH, default 100
       set_config('hnsw.iterative_scan', 'relaxed_order', true),
       set_config('statement_timeout', $2, true);       -- SEARCH_STATEMENT_TIMEOUT_MS, default 5000
```

`ef_search` must be at least the candidate count (30), and 100 gives headroom for the tenant filter. *Rejected:* inner product (`<#>`). It is only correct for normalized vectors, and cosine is correct for both providers.

**S8. RRF runs in Rust as a pure function.** `score = Σ 1/(k + rank)` over the branches the chunk appears in. Ties are broken by score descending, then best single rank ascending, then `chunk_id` ascending. That order is deterministic, which §11.6 requires. *Rejected:* RRF in SQL. It is harder to unit-test, and the inputs are at most 60 rows.

**S9. The citation key is `(document_id, chunk_index)`.** `chunk_id` is returned too, but it changes whenever a document is reprocessed, because chunks are replaced (L11). The API doc says so. Phase 6 citations use `(document_id, chunk_index)` together with `embedding_version`. *Rejected:* `chunk_id` as the citation key, which would break after any re-ingestion.

**S10. The response contains snippets, not full chunk content.** The service's internal result type keeps the full `content` for Phase 6 RAG. The HTTP DTO exposes only a `ts_headline` snippet (F8). *Rejected:* returning whole chunks, which makes larger responses and leaks more than a search result needs.

**S11. Evaluation uses our own synthetic corpus, labeled by phrase.** About 25 short documents and 50 labeled queries, all written for this project, so there is no copyrighted text. A chunk counts as relevant when it contains the labeled phrase, which keeps labels valid across re-chunking. The harness is an `#[ignore]` integration test that writes a JSON report. With a real provider it produces the published numbers. CI runs it with the fake embedder as a smoke test that asserts only on the exact-keyword and identifier queries. *Rejected:* public datasets (licensing, size) and chunk-id labels (they break on re-chunk).

**S12. Benchmarks run in a separate database with seeded data.** `tenant_saas_bench` is created on the same Postgres container and seeded by a small Rust tool in the tests crate. The report includes SQL-level plans and ANN recall (HNSW versus exact), plus HTTP latency via k6. 10k and 100k chunks are required. 1M is optional, because it depends on laptop RAM. *Rejected:* benchmarking in the dev database (polluted by tests) and pure random vectors (pathological for HNSW). Clustered vectors are used instead.

**S13. Change the chunker only on evidence.** T9 measures boundary-query recall. Only if the decision rule fires does the chunker change, with `EMBEDDING_VERSION` bumped to `p5-v1` and documents re-driven by the runbook procedure. *Rejected:* changing the chunker up front.

---

## 5. Request flow

```text
POST /api/v1/tenants/{t}/search  {query, limit?, document_ids?}
  │
  ├─ TenantCtx (L4) ─ require DocumentsSearch ─ validate request (T2)
  │
  ├─ embed query (timeout 3 s) ───────────── fail? → lexical-only (S3)      (no tx)
  │
  └─ begin_tenant_tx ─────────────────────────────────────────────────────── one short tx
       1. set_config: ef_search, iterative_scan = relaxed_order, statement_timeout
       2. vector candidates   (≤ 30)  document_chunks only, tenant + doc filter
       3. lexical candidates  (≤ 30)  document_chunks only, tenant + doc filter
       ── RRF in Rust (pure) → top `limit` chunk ids ──
       4. hydrate: join documents (deleted_at IS NULL), snippet, metadata
     commit
  │
  └─ 200 {mode, results[]}
```

---

## 6. Tasks

Order: T0 → T1 → T2 (needs no DB, can run in parallel with T1) → T3 → T4 → T5 → T6 → T7 → T9 → T8 → T10. Tests are written inside each task.

---

### T0: Groundwork: ADR-008, config, design-doc correction (P0, ~2h)

1. **Fix the design doc.** `'relaxed'` appears in five places: §9.2 (twice), §22.1, §27 and §28 (`grep -n "'relaxed'" multi_tenant_ai_saas_system_design.md`). Replace each with `'relaxed_order'`, and add one sentence to §9.2 on F1: an invalid value is silently reset to `off`. Also fix the image tag in §9.2, `pgvector/pgvector:pg16-v0.8.0`, to the tag actually used, `pgvector/pgvector:0.8.0-pg16`.
2. **`docs/adr/ADR-008-hybrid-search.md`**: context (§11), decisions S1–S13 (one short paragraph each), consequences. Reference L-items by number, without restating their reasoning.
3. **`SearchConfig`** in `common-config`, in the same style as `IngestionConfig` (defaults, `validate()`, tests):

| Key | Default | Validation |
|---|---|---|
| `SEARCH_VECTOR_CANDIDATES` | 30 | 1..=200 |
| `SEARCH_LEXICAL_CANDIDATES` | 30 | 1..=200 |
| `SEARCH_RRF_K` | 60 | 1..=1000 |
| `SEARCH_DEFAULT_LIMIT` | 10 | ≥ 1 and ≤ max limit |
| `SEARCH_MAX_LIMIT` | 50 | ≤ vector + lexical candidates (60 by default), since RRF can never return more |
| `SEARCH_EF_SEARCH` | 100 | ≥ `SEARCH_VECTOR_CANDIDATES`, ≤ 1000 |
| `SEARCH_EMBED_TIMEOUT_MS` | 3000 | 100..=30000 |
| `SEARCH_STATEMENT_TIMEOUT_MS` | 5000 | 100..=60000 |
| `SEARCH_QUERY_MAX_CHARS` | 1000 | 1..=8000 |

4. Mirror the keys in `.env.example` under a new `SEARCH` block.

**Done when** `cargo test -p common-config` passes with the new validation tests, and the ADR and the design-doc fix are in the diff.

---

### T1: Migration `006_search.sql` (P0, ~1.5h)

```sql
-- Migration: 006_search.sql
-- Purpose: full-text column and the two search indexes (Phase 5, ADR-008).
-- Runs as db_owner. Forward-only.
--
-- Index builds run inside the migration transaction. That is fine at current
-- size (F4). On a large production table build them CONCURRENTLY in a
-- separate, non-transactional step instead (see docs/runbooks/search-indexes.md).

ALTER TABLE document_chunks
    ADD COLUMN tsv_content tsvector
    GENERATED ALWAYS AS (to_tsvector('english'::regconfig, content)) STORED;

CREATE INDEX document_chunks_tsv_idx
    ON document_chunks USING gin (tsv_content);

CREATE INDEX document_chunks_embedding_hnsw_idx
    ON document_chunks USING hnsw (embedding vector_cosine_ops)
    WITH (m = 16, ef_construction = 64);
```

Not in this migration, on purpose:

- No grant changes. `app_runtime` already has `SELECT, INSERT, DELETE` on chunks, and a generated column cannot be written anyway.
- No RLS changes. The existing four policies cover the new column (L3, Phase 4 D2).
- No separate `(tenant_id, document_id)` index. The existing unique index already serves it (Phase 4 T1).
- No `btree_gin` composite `(tenant_id, tsv_content)`. Add it only if T8 shows the lexical branch scanning other tenants' matches at scale (§8).

**Tests** (`tests/security/rls_policies.rs` style or a new `tests/integration/search_schema.rs`):

| Case | Expectation |
|---|---|
| Chunk inserted by the Phase 4 pipeline | `tsv_content` populated automatically |
| `INSERT ... (tsv_content)` with an explicit value | Error: cannot insert into a generated column |
| `pg_indexes` | Both indexes exist, HNSW uses `vector_cosine_ops` |
| Migration on a DB at `005` with existing chunks | Applies. Existing chunks get `tsv_content` |
| All Phase 4 ingestion tests | Pass unchanged |

Extend the CI "RLS sanity" step to assert that both index names exist.

**Done when** `sqlx migrate run` applies on empty and on populated databases and the table passes.

---

### T2: Pure search domain: request validation and RRF (P0, ~2h, no DB)

**`apps/tenant-saas/src/domain/search.rs`** (no `sqlx`, no `axum`, and the CI pure-module guard is extended to cover this file):

```rust
pub struct SearchQuery { pub text: String, pub limit: usize, pub document_ids: Option<Vec<Uuid>> }

impl SearchQuery {
    /// Trims, rejects empty or over-long text (chars, not bytes), clamps nothing silently:
    /// limit outside 1..=max → Validation. document_ids: empty list = no filter,
    /// more than 100 → Validation, duplicates removed.
    pub fn parse(text: &str, limit: Option<usize>, document_ids: Option<Vec<Uuid>>,
                 cfg: &SearchConfig) -> Result<Self, AppError>;
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Candidate { pub chunk_id: Uuid, pub rank: usize }      // 1-based rank within its branch

#[derive(Debug, Clone, PartialEq)]
pub struct Fused { pub chunk_id: Uuid, pub score: f64,
                   pub vector_rank: Option<usize>, pub lexical_rank: Option<usize> }

/// Reciprocal Rank Fusion (S8). Deterministic: score desc, best rank asc, chunk_id asc.
pub fn rrf(vector: &[Candidate], lexical: &[Candidate], k: u32, limit: usize) -> Vec<Fused>;
```

**Tests:**

| Case | Expectation |
|---|---|
| Chunk in both lists at rank 1 | Score `2/(k+1)`, ranked first |
| Chunk only in vector at rank 1 vs only in lexical at rank 1 | Equal scores. Tie broken by `chunk_id` |
| Rank 1 in one list vs rank 2 in both | Hand-computed scores match. Both-lists chunk wins with k = 60 |
| Empty vector list (lexical-only mode) | Pure lexical order |
| Both empty | Empty result |
| `limit` smaller than the union | Truncated after sorting |
| Same input twice | Identical output (determinism) |
| Duplicate chunk ids within one list (should not happen) | Best rank kept, no double counting |
| `SearchQuery::parse`: `"  "`, 1001 chars, limit 0, limit 51, 101 document ids, `[]` document ids | 422 ×5, then `None` filter |
| Multi-byte query of exactly 1000 chars | Accepted (chars, not bytes) |

**Done when** `cargo test -p tenant-saas domain::search` passes with no database.

---

### T3: `search_repo`: the four statements (P0, ~4h)

**`apps/tenant-saas/src/repositories/search_repo.rs`**. Every function takes `&mut Tx` from `begin_tenant_tx`. Every query also binds `tenant_id` explicitly (L3).

```rust
pub async fn apply_settings(tx: &mut Tx, cfg: &SearchConfig) -> Result<(), AppError>;
pub async fn vector_candidates(tx: &mut Tx, tenant_id: Uuid, query_vec: &[f32],
                               doc_filter: Option<&[Uuid]>, n: i64) -> Result<Vec<Candidate>, AppError>;
pub async fn lexical_candidates(tx: &mut Tx, tenant_id: Uuid, query: &str,
                                doc_filter: Option<&[Uuid]>, n: i64) -> Result<Vec<Candidate>, AppError>;
pub async fn hydrate(tx: &mut Tx, tenant_id: Uuid, chunk_ids: &[Uuid], query: &str)
    -> Result<Vec<HitRow>, AppError>;
```

**1. Settings.** The SQL is the snippet in S7.

**2. Vector candidates** (F2 pattern). The vector is bound as a text literal through the existing public `chunk_repo::vector_literal` (Phase 4 D8). Call it, do not copy it.

```sql
WITH nearest AS MATERIALIZED (
    SELECT id, embedding <=> $2::vector AS distance
      FROM document_chunks
     WHERE tenant_id = $1
       AND ($3::uuid[] IS NULL OR document_id = ANY($3))
     ORDER BY embedding <=> $2::vector
     LIMIT $4
)
SELECT id FROM nearest ORDER BY distance, id
```

**3. Lexical candidates:**

```sql
SELECT c.id
  FROM document_chunks c, websearch_to_tsquery('english', $2) AS q
 WHERE c.tenant_id = $1
   AND ($3::uuid[] IS NULL OR c.document_id = ANY($3))
   AND c.tsv_content @@ q
 ORDER BY ts_rank_cd(c.tsv_content, q) DESC, c.id
 LIMIT $4
```

Ranks are the 1-based row positions. An empty tsquery (F6) returns no rows.

**4. Hydrate.** Liveness check here (S4), snippet (F8):

```sql
SELECT c.id, c.document_id, c.chunk_index, c.content, c.metadata,
       c.embedding_version, d.title,
       ts_headline('english', c.content, websearch_to_tsquery('english', $3),
                   'StartSel="", StopSel="", MaxFragments=2, MinWords=10, MaxWords=35, FragmentDelimiter=" … "') AS snippet
  FROM document_chunks c
  JOIN documents d ON d.tenant_id = c.tenant_id AND d.id = c.document_id
 WHERE c.tenant_id = $1 AND c.id = ANY($2) AND d.deleted_at IS NULL
```

The service puts rows back into RRF order. A chunk id missing from the hydration result (a concurrent delete) is simply dropped. It is never an error.

**Error mapping.** SQLSTATE `57014` (statement timeout or cancel) becomes `ExternalService("database")`, which is 503. Add it to `classify_sqlx_error` with a unit test (L13).

**Tests** (`tests/integration/search_repo.rs`). Each test ingests real fixtures through the Phase 4 `ingestion_service::process` with the fake embedder, then calls the repo:

| Case | Expectation |
|---|---|
| **F1 guard**: run a vector query, then `SHOW hnsw.iterative_scan` in the same tx after `apply_settings` | `relaxed_order` |
| **F1 regression**: `set_config('hnsw.iterative_scan','relaxed', true)`, then a vector query, then `SHOW` | Either an error (the pooled connection already loaded the library) or the value `off` (fresh connection). **Never** `relaxed`. This documents why the plan uses `relaxed_order` |
| Vector search with a chunk's own stored vector | That chunk is rank 1 |
| Lexical search for a word only in chunk X | X is rank 1. A word nowhere gives 0 rows |
| Stop-word-only query | 0 lexical rows, no error |
| User syntax: `"annual leave" -sick`, unbalanced quote, `OR` | No error |
| `doc_filter` with one document id | Only that document's chunks, in both branches |
| `doc_filter` with another tenant's document id | 0 rows (no error, no leak) |
| Hydrate a chunk of a document soft-deleted after search started (delete between steps 3 and 4) | Row dropped |
| Hydrate result | `snippet` is plain text, no `object_key` column selected |
| Statement timeout forced (`statement_timeout = 1ms` + `pg_sleep`) | `AppError::ExternalService` |

**Done when** the table passes and `cargo clippy` is clean.

---

### T4: `search_service` (P0, ~2h)

**`apps/tenant-saas/src/services/search_service.rs`:**

```rust
pub enum SearchMode { Hybrid, LexicalOnly }

pub struct SearchHit {                  // internal: Phase 6 RAG reuses this
    pub chunk_id: Uuid, pub document_id: Uuid, pub chunk_index: i32,
    pub document_title: String, pub content: String, pub snippet: String,
    pub page_start: Option<u32>, pub page_end: Option<u32>, pub heading: Option<String>,
    pub embedding_version: String,
    pub score: f64, pub vector_rank: Option<usize>, pub lexical_rank: Option<usize>,
}

pub struct SearchOutcome { pub mode: SearchMode, pub hits: Vec<SearchHit> }

pub async fn search(state: &AppState, ctx: &TenantContext, q: SearchQuery)
    -> Result<SearchOutcome, AppError>;
```

Steps, matching §5:

1. `ctx.require(Permission::DocumentsSearch)?`
2. Embed with `tokio::time::timeout(SEARCH_EMBED_TIMEOUT_MS, state.embedder.embed(&[q.text]))`, then `validate_vectors` (Phase 4). On timeout, provider error or invalid vector, set `mode = LexicalOnly` and log `warn` with `reason` (an error class only, never the query).
3. `begin_tenant_tx`, then `apply_settings`, `vector_candidates` (skipped in lexical-only mode), `lexical_candidates`, `rrf`, `hydrate`, `commit`.
4. One `info` log line with `target = "search"`: `tenant_id`, `mode`, `query_chars`, `vector_candidates`, `lexical_candidates`, `results`, `embed_ms`, `db_ms`, `total_ms`. **Never the query text** (L14).

**Tests** (`tests/integration/search.rs`, service level, using `with_embedder`):

| Case | Expectation |
|---|---|
| Provider that sleeps beyond the timeout | `LexicalOnly`, results present for a keyword query, total time < timeout + 1 s |
| Provider returning an error | `LexicalOnly` |
| Provider returning the wrong dimension | `LexicalOnly` (never sent to SQL) |
| Normal provider | `Hybrid`, ranks from both branches visible |
| Viewer role | Allowed (`DocumentsSearch`) |
| `captured_logs()` after a search with a unique marker word | Marker word absent |
| Database pool closed | `AppError::ExternalService` → 503 |

**Done when** the table passes.

---

### T5: HTTP endpoint and API doc (P0, ~2h)

**`apps/tenant-saas/src/handlers/search.rs`**, merged in `routes.rs` beside `documents::router`:

```rust
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchRequest { query: String, limit: Option<usize>, document_ids: Option<Vec<Uuid>> }
```

The request shape matches design §13.5 exactly. Unknown fields return 422.

**Response** `200 OK`:

```json
{
  "mode": "hybrid",
  "results": [
    {
      "document_id": "uuid",
      "chunk_index": 3,
      "chunk_id": "uuid",
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

No `content`, no `object_key`, no `embedding` (S10). An empty `results` array is a valid 200.

| Case | Status | Code |
|---|---|---|
| Valid request | 200 | — |
| No token | 401 | `UNAUTHORIZED` |
| Not a member, unknown or malformed tenant | 404 | `NOT_FOUND` (L4) |
| Suspended membership or tenant | 403 | `FORBIDDEN` |
| Empty or over-long query, bad `limit`, more than 100 `document_ids`, unknown field, bad JSON | 422 | `VALIDATION_ERROR` |
| Database unavailable or statement timeout | 503 | `SERVICE_UNAVAILABLE` |

**Docs.** Add a new file `docs/api/search.md` with the endpoint, the request/response contract, the error table, the meaning of `mode`, `score` (relative within one response only, not comparable across queries) and the ranks, and the citation-key rule (S9). Add a link from `docs/api/documents.md`.

**Tests:** route tests in `routes.rs` (401 without a DB, as Phase 3 did), plus one HTTP test per row of the table above in `tests/integration/search.rs`. The integration tests also assert the JSON field set exactly, so nothing extra leaks.

**Done when** every row has a passing test.

---

### T6: Security tests (P0, ~2h)

**`tests/security/cross_tenant_search.rs`:**

| Case | Expectation |
|---|---|
| Tenants A and B ingest the **same** file. B searches its exact text | Only B's `document_id` is returned |
| B searches a word that only exists in A's documents | Empty results |
| B passes A's `document_id` in `document_ids` | Empty results, 200 (no existence leak) |
| B calls `/tenants/{A}/search` | 404, body identical to a random tenant id |
| Repository level: `begin_tenant_tx` with B's context, `tenant_id = A` bound explicitly | 0 rows (RLS stops it even if the explicit filter were wrong) |
| `pool_reuse.rs`: search on a 1-connection pool alternating tenants | No leak. `hnsw.*` settings do not persist after commit (they are `set_config(..., true)`) |
| Viewer, Member, Admin, Owner | All 200 (`escalation.rs` matrix row) |
| Soft-deleted document | Never returned, even though deletion races are handled at hydration |

**Done when** all pass in CI.

---

### T7: Relevance evaluation dataset and harness (P0, ~5h)

**Dataset** in `tests/eval/search/`, versioned as `v1`:

- `corpus/`: about 25 short TXT and MD files written for this project. Fictional company handbook sections, product specs with codes (`SKU-4471`, `API v2.1`, `ERR-503X`), a FAQ, two near-duplicate policy versions, and several documents long enough to produce at least 4 chunks each. No copied text.
- `queries.jsonl`: about 50 lines:
  ```json
  {"id": "q017", "kind": "paraphrase", "query": "how many vacation days roll over",
   "relevant": [{"doc": "handbook_leave.md", "phrase": "carries over up to ten days"}]}
  ```
  `kind` is one of `exact_keyword`, `identifier`, `acronym`, `paraphrase`, `multi_chunk`, `ambiguous`, `similar_docs` (design §11.7) or `boundary` (T9). Aim for at least 5 queries per kind and 8 `boundary` queries. A `boundary` query is one whose `phrase` is written to straddle two adjacent paragraphs.
- `README.md`: how to add a query, and the rule that a chunk is relevant if its `content` contains `phrase` after whitespace normalization (S11). Graded relevance is optional: `"grade": 2|1`.

**Harness** `tests/integration/search_eval.rs`. It is an `#[ignore]` test, so it never runs in normal `cargo test`:

1. Create a tenant, ingest every corpus file through `ingestion_service::process` with `state.embedder` (real or fake, depending on config).
2. Run every query through `search_service::search` at `limit = 10`.
3. Compute per query and per `kind`: Recall@5, Recall@10, MRR, nDCG@10 (graded where given, binary otherwise), and latency. The metric functions are pure and unit-tested in the same file (hand-computed examples).
4. Write `target/search-eval/report-{timestamp}.json` and print a summary table.
5. Assert thresholds only in smoke mode (`EVAL_MODE=smoke`): `exact_keyword` and `identifier` Recall@10 = 1.0.

Run commands (documented in the README):

```bash
EVAL_MODE=smoke cargo test --test integration search_eval -- --ignored --nocapture
```

```bash
EMBEDDING_PROVIDER=openai EMBEDDING_API_KEY=... cargo test --test integration search_eval -- --ignored --nocapture
```

Embedding the whole corpus costs a fraction of a cent with `text-embedding-3-small` (about 25 documents).

**Ablation in the same run.** The harness also reports lexical-only (vector branch off) and vector-only (lexical branch off) for comparison. That is the evidence that hybrid is worth it (§11.1), without a second tool.

**Done when** the smoke mode passes, a real-provider run has produced a report, and the summary numbers are in the benchmark doc (T8).

---

### T8: Benchmark and query-plan review (P0, ~6h)

**Setup:** a separate database, so dev and test data stay clean (S12):

```bash
docker compose -f infra/docker-compose.yml exec postgres psql -U postgres -c "CREATE DATABASE tenant_saas_bench OWNER db_owner;"
```

```bash
docker compose -f infra/docker-compose.yml exec postgres psql -U postgres -d tenant_saas_bench -f /docker-entrypoint-initdb.d/00_roles.sql
```

Then run `sqlx migrate run` against it as `db_owner`.

**Seeding tool** `tests/performance/seed_search.rs` (a `[[bin]]` in the tests crate, never shipped). It writes directly as `db_owner` with batched `UNNEST` inserts and reuses the Phase 4 vector-literal helper. It never calls an embedding provider. Parameters: `--chunks`, `--large-tenant-share`, `--small-tenants`, `--seed`.

- Distribution per §9.2: one large tenant with most chunks, 50 small tenants with 100 chunks each.
- Vectors: 64 random unit centroids, each chunk = centroid + Gaussian noise (σ ≈ 0.1), normalized. Text: words from a fixed vocabulary, with planted identifiers so lexical queries have known answers.
- Sizes: **10k** and **100k** (required). **1M** is optional, and is recorded as skipped if the machine cannot build the index. Note `maintenance_work_mem` and the build time in the report.

**Measurements**, all recorded in `docs/benchmarks/phase5-search.md`:

| What | How |
|---|---|
| Index build time and size | `\timing` on `REINDEX`, `pg_relation_size` |
| ANN recall: HNSW vs exact | For 100 query vectors per tenant type: top-30 with the index versus exact top-30 (`SET LOCAL enable_indexscan = off`). Report recall for the **large** tenant, and for a **small** tenant with `iterative_scan` `off` versus `relaxed_order` (§9.2 checklist) |
| Plans | `EXPLAIN (ANALYZE, BUFFERS)` of both candidate queries for a small tenant and the large tenant. Write two or three sentences per plan: which index, rows scanned, whether F5 applied |
| SQL latency | P50/P95/P99 of the full four-statement transaction, 1000 runs, single client |
| HTTP latency | `tests/performance/search.js` (k6): 10, 50 and 100 VUs for 60 s each against `POST /search` with the fake embedder, so the provider is excluded and the number is our system's cost. P50/P95/P99, throughput, error rate |
| Environment | CPU, RAM, OS, Docker resources, Postgres settings, dataset parameters, commit hash (design §19.5) |

The report states plainly that these are laptop numbers for a baseline, not production guarantees (design "Measure rather than promise").

If the lexical branch is clearly slow for the large tenant because the GIN index returns other tenants' matches before filtering, record it and propose `btree_gin (tenant_id, tsv_content)` as a follow-up. Do not add it in Phase 5 unless P95 exceeds 200 ms at 100k (§8).

**Done when** the report has every row above filled in (or marked skipped, with the reason) and the plan readings are written.

---

### T9: Chunk-boundary analysis (P0 to measure, P1 to change, ~3h + ~3h if triggered)

The design asks for this explicitly. The current chunker (`p4-v1`) carries overlap only when the trailing **paragraph** fits in 75 tokens, so documents with long paragraphs get no overlap at all.

1. **Measure.** From the T7 real-provider report, compare `boundary` Recall@10 with the overall Recall@10.
2. **Decision rule.** If `boundary` Recall@10 is **10 or more percentage points** below overall, implement step 3. Otherwise record "no change needed", with the numbers, and stop.
3. **Change, only if triggered.** In `chunker.rs`, build the overlap from the trailing **sentences** of the previous chunk (reusing the existing sentence splitter) up to `CHUNK_OVERLAP_TOKENS`, instead of whole paragraphs. Bump `EMBEDDING_VERSION` to `"p5-v1"`. Update and extend the Phase 4 chunker tests: overlap now present with long paragraphs, still ≤ 75 tokens, still deterministic, still UTF-8 safe.
4. **Re-ingest, only if triggered.** Add a "Re-chunk after a pipeline version change" section to `docs/runbooks/ingestion-recovery.md`. It is a `db_owner` SQL procedure modeled on runbook section D: for live documents whose chunks have an older `embedding_version`, insert a fresh job and outbox event. Old chunks stay searchable until replaced (S4). Re-run T7 and record before and after.

**Done when** the benchmark doc contains the comparison, and either "no change" or the before/after numbers.

**Result (2026-11-17, commit `35e7e5b`):** `boundary` Recall@10 = **1.000**, overall Recall@10 = **0.980**. Boundary is **above** overall; gap = -2 pp. Decision rule not triggered. **No chunker change needed.** Numbers recorded in `docs/benchmarks/phase5-search.md §T9`.

---

### T10: CI, docs and checklist (P1, ~2h)

**CI** (`.github/workflows/ci.yml`):

1. Extend the RLS sanity step: assert `document_chunks_tsv_idx` and `document_chunks_embedding_hnsw_idx` exist.
2. Extend the pure-module grep guard to `apps/tenant-saas/src/domain/search.rs`.
3. New guard: the strings `hnsw.iterative_scan` and `'relaxed'` must not appear together anywhere under `apps/` (F1). One `grep -rn "iterative_scan.*'relaxed'" apps/` must return nothing.
4. New step: run the eval smoke test (`EVAL_MODE=smoke ... -- --ignored search_eval`).

**Docs:**

- `README.md`: a "Search" section with a curl example, the `SEARCH_*` env table, and the eval commands.
- `docs/architecture/search.md`: the §5 flow, S4 (liveness), S7 (settings), F1 and F5 in plain words.
- `docs/runbooks/search-indexes.md`: rebuilding indexes with `CONCURRENTLY` on a large table, tuning `ef_search`, and what to check when results look wrong (empty `mode: lexical_only` storms mean the provider is down).
- `docs/benchmarks/phase5-search.md`: from T7, T8 and T9.
- ADR-008 final review: every S-decision either confirmed or amended with evidence from T7/T8.

**Done when** CI is green and every box in §9 is ticked.

---

## 7. Dependency graph

```text
T0 (ADR-008, SearchConfig, design-doc fix)
 ├─► T1 (migration 006) ─┐
 └─► T2 (pure RRF)  ─────┴─► T3 (search_repo) ─► T4 (service) ─► T5 (endpoint) ─► T6 (security)
                                                                         │
                                                                         ├─► T7 (eval) ─► T9 (boundary)
                                                                         └─► T8 (benchmark)
                                                       T6, T7, T8, T9 ─► T10 (CI, docs)
```

Good stopping points: after T3 (search works at repo level), after T6 (feature complete and secure), after T8 (measured).

---

## 8. Risks

| Risk | Mitigation |
|---|---|
| `'relaxed'` silently disables iterative scan (F1) | `relaxed_order`, a read-back test, a CI grep guard, and the design doc fixed in T0 |
| Small tenants get poor HNSW recall | F5: small tenants usually get an exact btree plan. Large tenants: iterative scan plus `ef_search = 100`. Measured in T8 |
| GIN index returns all tenants' matches before the tenant filter | Measured in T8. `btree_gin` composite only if P95 > 200 ms at 100k |
| Query embedding is slow or down | 3 s timeout plus lexical-only degrade (S3). `mode` makes it visible |
| Search cost abuse (each query calls a paid provider) | No limiter until Phase 7 (L16). Recorded in ADR-008 consequences. Phase 7 adds per-tenant limits for search |
| `english` stemming hurts codes or non-English text | Identifiers verified (F7). Eval `identifier` and `acronym` kinds. Multi-language is out of scope |
| Index build on a big production table locks writes | Migration comment plus a runbook for `CONCURRENTLY` (T1, T10) |
| Relevance numbers from the fake embedder | Smoke mode asserts only lexical kinds. Published numbers require a real provider (L10) |
| `chunk_id` used as a citation key | S9: the citation key is `(document_id, chunk_index)`, documented in the API |
| Laptop limits for 1M chunks | 1M is optional, recorded as skipped with the reason |

---

## 9. Checklist

### P0

- [x] Design doc corrected in all five places (`relaxed_order`) plus the §9.2 image tag
- [x] ADR-008 with S1–S13
- [x] `SearchConfig` plus `.env.example`
- [x] `006_search.sql`: generated `tsv_content`, GIN, HNSW
- [x] `domain/search.rs`: `SearchQuery::parse`, `rrf`, with tests
- [x] `search_repo`: settings, vector, lexical, hydrate. 57014 → 503
- [x] `search_service` with lexical-only degrade and safe logging
- [x] `POST /tenants/{tenant_id}/search` plus `docs/api/search.md`
- [ ] `cross_tenant_search.rs`, `pool_reuse` case, escalation row
- [x] Eval dataset v1, harness, smoke assertions, one real-provider report
- [ ] Benchmark report: 10k and 100k, ANN recall, plans, SQL and HTTP latency
- [x] Chunk-boundary analysis recorded (and chunker change if triggered)

### Regression guards

- [ ] fmt, clippy `-D warnings`, all Phase 1–4 tests unchanged and green
- [ ] Existing grep guards pass
- [ ] Ingestion still reaches `ready` (Phase 4 e2e smoke) with migration 006 applied

### P1

- [ ] CI: index assertions, pure-module guard, `relaxed` guard, eval smoke step
- [ ] `docs/architecture/search.md`, `docs/runbooks/search-indexes.md`, README section

### P2

- [ ] 1M-chunk benchmark
- [ ] `btree_gin` composite index experiment
- [ ] Query-embedding cache keyed by model and query hash (design §17)

---

## 10. Handoff to Phase 6 (RAG)

| Concern | State at end of Phase 5 | Phase 6 use |
|---|---|---|
| Retrieval | `search_service::search` returns `SearchHit` with full `content` internally | RAG calls it directly (no HTTP) as its retrieval step, inside its own micro-scoped flow (design §12.1) |
| Citations | Stable key `(document_id, chunk_index)` plus `embedding_version`, title, page and heading | Citation events in SSE use exactly these fields |
| Degraded mode | `SearchMode::LexicalOnly` when embedding fails | RAG can still answer from lexical context, or refuse, depending on its policy |
| Quality evidence | Eval harness and dataset v1 | Phase 6 adds answer-level checks on top of the same corpus |

---

## 11. Definition of done

1. A member can call `POST /search` and get tenant-scoped results with snippets, ranks and a citation key, as documented in `docs/api/search.md`.
2. Keyword, identifier and paraphrase queries are each shown to work by tests and by the eval report.
3. Recall@5/10, MRR and nDCG@10 (real provider) and P50/P95/P99 latency (10k, 100k) are published, with environment details.
4. Query plans for small and large tenants are captured and explained.
5. The chunk-boundary question is answered with numbers.
6. No locked decision in §1 was changed. Any change that turned out to be needed is recorded as an ADR amendment, not hidden in code.
7. CI is green, including the eval smoke step and the `relaxed` guard.
