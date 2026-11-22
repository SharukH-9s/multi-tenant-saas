# ADR-008: Hybrid Search Architecture and Reciprocal Rank Fusion

**Status**: Accepted  
**Date**: 2026-11-15  
**Deciders**: Platform Engineering  
**Phase**: 5 — Hybrid Search  

---

## Context

Phase 5 implements hybrid search over ingested document chunks (`multi_tenant_ai_saas_system_design.md` §11, §9.1, §9.2, §13.5, §17, §19.5, §23). A member of an organization submits a query and receives the most relevant chunks from that tenant's documents. Each result carries a plain-text snippet and a stable citation key.

Retrieval combines:
1. **Semantic vector ranking** via pgvector HNSW index scans with iterative search (`<=>` cosine distance).
2. **Lexical keyword ranking** via PostgreSQL Full-Text Search (FTS) using `ts_rank_cd` and `websearch_to_tsquery`.
3. **Reciprocal Rank Fusion (RRF)** merging candidate rankings deterministically.

This architecture builds on locked decisions L1–L16 established in Phases 1–4:
- Modular monolith running in the API process with Rust, Axum, Tokio, and SQLx (L1, L2).
- Shared PostgreSQL schema with strict tenant isolation enforced by Row Level Security (RLS) and explicit `tenant_id` query filters under `begin_tenant_tx` (L3, L4, L12).
- Micro-scoped transactions: no database transaction is held open across external network I/O (L5).
- Candidate limits: starting defaults of 30 vector + 30 lexical candidates, RRF $k = 60$, and top 10 limit (L7).
- Fixed 1536-dimensional embeddings with `vector(1536)` (L9).
- Application state manages the embedder; fake embedder has no semantic meaning (L10).
- Atomic chunk replacement on reprocess; liveness determined by `documents.deleted_at IS NULL` (L11).
- Standard API error envelope with 503 on dependency failures (L13).
- Zero sensitive data in logs: query text and snippets are never logged (L14).
- Semantic caching deferred to future phases (L15), and per-tenant rate limits/quotas deferred to Phase 7 (L16).

---

## Decisions

### S1: Monolithic Component Layout (One Endpoint, One Service, One Repository)

Search is integrated directly into the existing `tenant-saas` API application rather than as a separate service or crate (L1). The feature consists of a single endpoint `POST /api/v1/tenants/{tenant_id}/search`, a domain validation and fusion module (`domain::search`), an application service (`services::search_service`), and a data access repository (`repositories::search_repo`).  
*Rejected:* A dedicated search microservice or standalone search crate, which would add operational and build overhead without necessity.

### S2: Two-Phase Execution: Embed First, Then Micro-Scoped Transaction

The query text is embedded via `state.embedder` before any database connection is acquired (L5). Query embedding is bounded by a timeout (`SEARCH_EMBED_TIMEOUT_MS`, default 3000 ms). Once the vector is ready, a single short transaction (`begin_tenant_tx`) executes four sequential statements: applying session search settings, querying vector candidates, querying lexical candidates, and hydrating the fused results.  
*Rejected:* Embedding inside the database transaction (risks connection pool exhaustion), or consolidating all candidate and RRF logic into a single monolithic SQL statement (hinders testing, debugging, and `EXPLAIN` visibility per branch).

### S3: Graceful Degradation to Lexical-Only on Embedder Failure

If the external embedding provider times out, returns an error, or produces a vector with invalid dimensions, search does not fail the entire request. The service falls back to lexical-only candidate retrieval, returns HTTP 200 with `"mode": "lexical_only"`, and logs a `warn` event containing the error class (never the query text, L14). Database failures continue to return HTTP 503.  
*Rejected:* Failing the entire search request on provider downtime; keyword search remains functional and provides partial service.

### S4: Candidate Queries Isolated to `document_chunks`; Liveness Checked at Hydration

Both vector and lexical candidate queries target only the `document_chunks` table, filtered by `tenant_id` and optional `document_ids`. Neither query joins `documents` or filters by `documents.status = 'ready'`. Document liveness is enforced during final row hydration using `JOIN documents d ON d.tenant_id = c.tenant_id AND d.id = c.document_id WHERE d.deleted_at IS NULL`. By L11, chunks exist only after successful ingestion, so during document reprocessing previous chunks remain searchable until atomically replaced.  
*Rejected:* Filtering `status = 'ready'` or joining in candidate queries, which impedes HNSW index traversal and hides documents undergoing background reprocessing.

### S5: Document Scope Filter Applied Pre-Ranking

When callers provide an optional `document_ids` filter, the condition `AND ($n::uuid[] IS NULL OR document_id = ANY($n))` is executed inside both candidate queries before candidate selection and RRF fusion.  
*Rejected:* Filtering post-fusion, which discards ranked results and can return fewer than `limit` matches even when matching chunks exist in the corpus.

### S6: Lexical Retrieval via PostgreSQL FTS with `'english'` Configuration

Lexical search queries a generated stored `tsvector` column (`tsv_content = to_tsvector('english'::regconfig, content)`) indexed with a GIN index. Ranking uses `ts_rank_cd(tsv_content, q)`. Queries are parsed using `websearch_to_tsquery('english', query)`, which safely supports phrases, `OR`, and `-` negation without raising syntax errors on arbitrary user input (F6, F7).  
*Rejected:* The `'simple'` FTS configuration (lacks stemming and phrase matching) and multi-language dictionary switching (out of scope for Phase 5).

### S7: Cosine Vector Search with Explicit Iterative Scan Settings

Vector retrieval uses cosine distance (`<=>` with `vector_cosine_ops`). Within the search transaction, session settings are explicitly applied: `hnsw.ef_search = 100`, `hnsw.iterative_scan = 'relaxed_order'`, and `statement_timeout = 5000ms`. Candidate queries wrap the HNSW scan in a `MATERIALIZED` CTE and explicitly order by distance in the outer query to handle possible non-strict ordering under `relaxed_order` (F1, F2).  
*Rejected:* Inner product distance (`<#>` requires unit normalization), and `'relaxed'` which is an invalid setting that silently disables iterative scans (F1).

### S8: Pure-Function Reciprocal Rank Fusion in Rust

Reciprocal Rank Fusion is executed in application code as a pure, deterministic function:
$$\text{RRF Score}(d) = \sum_{m \in M} \frac{1}{k + r_m(d)}$$
where $k = 60$ and $r_m(d)$ is the 1-based rank within candidate list $m$. Deterministic tie-breaking orders by score descending, best individual branch rank ascending, and `chunk_id` ascending.  
*Rejected:* RRF in SQL, which complicates unit testing, determinism guarantees, and candidate inspections for small candidate sets ($\le 60$ rows).

### S9: Stable Composite Citation Key `(document_id, chunk_index)`

API search results identify chunks with a stable citation key `(document_id, chunk_index)` alongside `embedding_version`. While `chunk_id` is exposed for diagnostic purposes, clients and downstream Phase 6 RAG citations rely on `(document_id, chunk_index)` because `chunk_id` changes whenever a document is reprocessed (L11).  
*Rejected:* Using `chunk_id` as the primary citation reference, which breaks citations upon document re-ingestion.

### S10: Snippets in Response via `ts_headline`, Full Content Kept Internal

Search API responses return contextual snippets generated by PostgreSQL `ts_headline` with plain-text delimiters (`StartSel="", StopSel=""`), stripping HTML markup (F8). The internal `SearchHit` struct retains full chunk text for consumption by Phase 6 RAG, but full text, object storage keys, and raw vector embeddings are excluded from the public DTO.  
*Rejected:* Returning full chunk text in public search responses, which inflates network payloads and leaks unnecessary chunk contents.

### S11: Synthetic Evaluation Corpus and Normalized Phrase-Matching Harness

Search relevance is evaluated against a synthetic corpus of 25 short documents and 50 labeled queries across 8 query types (`exact_keyword`, `identifier`, `acronym`, `paraphrase`, `multi_chunk`, `ambiguous`, `similar_docs`, `boundary`). Relevance is determined by normalized phrase containment rather than chunk IDs, allowing labels to survive re-chunking. CI runs an automated smoke test asserting Recall@10 = 1.0 for exact keyword and identifier queries with the fake embedder.  
*Rejected:* Third-party benchmark datasets (licensing/domain mismatch) and chunk-id ground-truth labels.

### S12: Dedicated Benchmark Database and Clustered Synthetic Seeding

Search benchmarks run against an isolated `tenant_saas_bench` database using a dedicated seeding tool (`seed_search`). Seeding models multi-tenant data skew (1 large tenant with hundreds of thousands of chunks, 50 small tenants with 100 chunks each) with Gaussian-clustered vectors. Benchmarks measure index build time, ANN recall (HNSW vs exact scan), execution plans via `EXPLAIN (ANALYZE, BUFFERS)`, transaction latency, and HTTP throughput via k6 at 10k and 100k chunk scales.  
*Rejected:* Benchmarking against dev/test databases (state pollution) or using uniformly random vectors (pathological for HNSW graphs).

### S13: Evidence-Driven Chunker Modification (Boundary Analysis)

The Phase 4 chunker (`p4-v1`) is preserved unless evaluation shows boundary-query Recall@10 is 10 or more percentage points below overall recall. Only if this threshold is crossed will the chunker be updated to construct overlap from trailing sentences rather than full paragraphs, bumping the version to `p5-v1`.  
*Rejected:* Modifying chunker tokenization without quantitative retrieval evidence.

---

## Consequences

### Positive
- Combines lexical precision (exact codes, SKUs, identifiers) and semantic recall (paraphrasing, conceptual matches) with deterministic RRF ranking.
- Strict multi-tenant isolation via dual defense (RLS + explicit `tenant_id` filter).
- Connection pool resilience: query embedding is performed before transaction acquisition, and transactions are micro-scoped.
- High availability: external provider timeouts or outages degrade gracefully to lexical search instead of erroring.
- Safe API surface: snippets are scoped, query text is not logged, and citations remain stable across re-indexing.

### Trade-offs and Risks
- **Cost & Abuse**: Each search request invokes an embedding API call. Per-tenant rate limits and search quotas are deferred to Phase 7 (L16).
- **GIN Index Multitenancy**: PostgreSQL GIN index scans all tenants' postings matching search terms before filtering by tenant. Monitored in T8 benchmarks; a composite `btree_gin (tenant_id, tsv_content)` can be introduced if P95 latency exceeds 200 ms at scale.
- **Statement Timeout Handling**: Search transactions enforce a 5-second `statement_timeout`; SQLSTATE `57014` is classified as 503 Service Unavailable.
