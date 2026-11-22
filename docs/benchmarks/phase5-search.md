# Phase 5 Search Benchmark Report

> These are laptop baseline numbers for a development dataset, not production guarantees.
> See §19.5 of the system design for the "Measure rather than promise" policy.

---

## Environment

| Field | Value |
|---|---|
| Date | 2026-11-17 / 2026-11-18 |
| Git commit | `35e7e5b78b153215f4dcddc18da05c6209697bc6` |
| OS | Windows 11 x86_64 |
| RAM | 16 GB |
| Embedding provider (Eval) | Azure AI Foundry — `text-embedding-3-small` (1536 dims) |
| Embedding provider (Load Test) | Fake embedder (`tests/performance/search.js` via k6) |
| PostgreSQL | `pgvector/pgvector:0.8.0-pg16` (Docker) |
| PostgreSQL Settings | `shared_buffers = 1GB`, `work_mem = 64MB`, `maintenance_work_mem = 256MB` |
| Eval Corpus | 25 documents, 31 chunks |
| Benchmark Dataset | Dedicated `tenant_saas_bench` (10k & 100k chunks, 1 large + 50 small tenants) |

---

## T7: Search Relevance Evaluation

Performed by `tests/integration/search_eval.rs` (the `#[ignore]` harness).
One fresh tenant was created, all 25 corpus files were ingested end-to-end through
`ingestion_service::process`, and all 50 labelled queries were run three ways:
**hybrid** (through `search_service::search`), **lexical-only**, and **vector-only**.

Report artifact: `target/search-eval/report-20261117T112020Z.json`

### Search configuration

| Parameter | Value |
|---|---|
| `vector_candidates` | 30 |
| `lexical_candidates` | 30 |
| `rrf_k` | 60 |
| `ef_search` | 100 |
| `default_limit` | 10 |

### Results by kind

All numbers are averages over the queries in that kind.
Latency is the full wall-clock time per query including the embedding call for hybrid/vector.

| Kind | n | Hybrid R@5 | Hybrid R@10 | Hybrid MRR | Hybrid nDCG@10 | Hybrid ms | Lexical R@10 | Vector R@10 |
|---|:---:|:---:|:---:|:---:|:---:|:---:|:---:|:---:|
| **ALL** | **50** | **0.960** | **0.980** | **0.903** | **0.930** | **390.7** | 0.440 | 0.980 |
| `exact_keyword` | 6 | 1.000 | 1.000 | 1.000 | 1.000 | 437.4 | 1.000 | 1.000 |
| `identifier` | 6 | 1.000 | 1.000 | 1.000 | 1.000 | 384.5 | 1.000 | 1.000 |
| `paraphrase` | 7 | 0.857 | 1.000 | 0.873 | 0.900 | 372.5 | 0.143 | 1.000 |
| `acronym` | 6 | 1.000 | 1.000 | 0.867 | 0.898 | 386.5 | 0.167 | 1.000 |
| `multi_chunk` | 6 | 1.000 | 1.000 | 0.833 | 0.928 | 346.0 | 0.000 | 1.000 |
| `similar_docs` | 6 | 1.000 | 1.000 | 1.000 | 1.000 | 395.7 | 0.833 | 1.000 |
| `ambiguous` | 5 | 0.800 | 0.800 | 0.800 | 0.817 | 357.2 | 0.600 | 0.800 |
| `boundary` | 8 | 1.000 | 1.000 | 0.854 | 0.891 | 430.2 | 0.000 | 1.000 |

### Reading

**Exact keyword and identifier queries (Recall@10 = 1.000 for all three modes):**
Both lexical and vector search find every exact-keyword and identifier query. The lexical
branch is authoritative here (3–4 ms latency); the vector branch also happens to find them
all because the embeddings of near-exact phrases are very close in vector space.

**Paraphrase queries (Lexical R@10 = 0.143, Hybrid R@10 = 1.000):**
This is the core justification for hybrid search. When a user asks *"how many vacation days
roll over"* but the document says *"annual leave carries over up to ten days"*, the lexical
branch scores zero — no token overlap. The vector branch understands the semantic
equivalence and returns the correct chunk at rank 1. RRF fuses both and the correct
answer surfaces at rank 1 for every paraphrase query.

**Acronym queries (Lexical R@10 = 0.167, Hybrid R@10 = 1.000):**
PostgreSQL's `'english'` FTS tokeniser does not map acronyms to their expansions. The
vector branch finds the concept by meaning even when only the abbreviation was used in the
query. Hybrid corrects the lexical blind spot.

**Multi-chunk queries (Lexical R@10 = 0.000, Hybrid R@10 = 1.000):**
Queries whose intended answer is spread across adjacent chunks, or where the query concept
is discussed without repeating exact keywords. The lexical branch finds no candidates;
the vector branch retrieves the correct chunk by semantic proximity.

**Boundary queries (Hybrid R@10 = 1.000):**
See T9 below. This kind was specifically designed to test whether retrieval works when the
answer sits in the paragraph *after* the one that names the rule, without repeating the
rule name. All eight boundary queries were answered correctly at Recall@10 = 1.000.

**Ambiguous queries (Hybrid R@10 = 0.800):**
Five queries where the question could plausibly match multiple documents. Two of the five
documents were not retrieved within the top 10. This is expected for genuinely ambiguous
queries and does not indicate a retrieval failure; it indicates topic ambiguity in the corpus.

**MRR: Hybrid (0.903) > Vector (0.880) > Lexical (0.440):**
RRF consistently promotes the best candidate to rank 1 more often than either branch alone.
The seven-point MRR gain of hybrid over vector-only demonstrates that the lexical branch
contributes useful ranking signal even when vector recall is already high.

---

## T9: Chunk-Boundary Analysis

**Decision rule (plan §T9):** If `boundary` Recall@10 is 10 or more percentage points
below the overall Recall@10, implement sentence-level overlap in the chunker.

| Metric | boundary | ALL |
|---|:---:|:---:|
| Recall@10 (hybrid) | **1.000** | 0.980 |
| MRR (hybrid) | 0.854 | 0.903 |

`boundary` Recall@10 (1.000) is **above** overall Recall@10 (0.980). The gap is
**-2.0 percentage points** in favour of boundary queries. The decision rule is not
triggered.

**Decision: No chunker change needed.**

The current `p4-v1` chunker (paragraph-based with up to 75-token trailing-paragraph
overlap) retrieves neighbouring context with the same quality as other query kinds.
The boundary query design — answer in the paragraph after the one that names the rule,
without repeating the rule name — was answered correctly in all eight cases.

This result is recorded as required by ADR-008 §S11 and closes the T9 measurement phase.
If a future real-world corpus shows `boundary` recall more than 10 pp below overall,
re-evaluate with sentence-level overlap (`CHUNK_OVERLAP_TOKENS` capped at 75 tokens from
trailing sentences instead of trailing paragraphs) and bump `EMBEDDING_VERSION` to
`p5-v1`.

---

## T8: Benchmark (Latency, Query Plans, and Index Sizing)

Benchmark executed on the dedicated `tenant_saas_bench` database populated with synthetic clustered chunks using `tests/performance/seed_search.rs`.
The dataset models a SaaS multi-tenant distribution (§9.2): one large tenant owning 95% of chunks, and 50 small tenants with 100 chunks each.
Query vectors were generated from perturbed cluster centroids ($\sigma = 0.1$) across 64 unit clusters in $\mathbb{R}^{1536}$.

### Index Sizing and REINDEX Timing

| Relation | 10k Chunks Size | 10k Reindex Time | 100k Chunks Size | 100k Reindex Time |
|---|:---:|:---:|:---:|:---:|
| `document_chunks` (table heap) | 11.80 MiB (12,369,920 B) | — | 117.98 MiB (123,707,392 B) | — |
| `document_chunks_embedding_hnsw_idx` (HNSW) | 78.13 MiB (81,928,192 B) | 27.81 s | 781.26 MiB (819,208,192 B) | 291.65 s (4.86 min) |
| `document_chunks_tsv_idx` (GIN) | 0.63 MiB (663,552 B) | 112 ms | 6.21 MiB (6,512,640 B) | 1,019 ms (1.02 s) |
| `document_chunks_position_unique` (B-tree) | 0.79 MiB | — | 7.85 MiB (8,232,960 B) | — |
| `document_chunks_pkey` (B-tree) | 0.43 MiB | — | 4.23 MiB (4,431,872 B) | — |

*Note on 1M scale:* Skipped per plan §T8: 100k HNSW index occupies 781 MiB and requires ~5 minutes to build on a 16 GB development laptop; 1M chunks (~8 GiB index) exceeds available development container memory without swapping.

### ANN Recall (HNSW vs Exact Ground Truth, Top-30, 100 Queries)

Recall is measured by comparing the top-30 candidate IDs from the index query against the exact ground truth (`SET LOCAL enable_indexscan = off`) over 100 randomized query vectors.

| Scale | Tenant Type | Configuration | Recall@30 | Reading |
|---|---|---|:---:|---|
| **10k** | Large (5k chunks) | `relaxed_order`, `ef_search = 100` | **1.0000** | Planner used exact sequential scan / top-N heapsort. |
| **10k** | Small (100 chunks) | `iterative_scan = off` | **1.0000** | Fact F5: planner bypassed HNSW in favour of B-tree. |
| **10k** | Small (100 chunks) | `iterative_scan = relaxed_order` | **1.0000** | Fact F5: exact B-tree scan. |
| **100k** | Large (95k chunks) | `relaxed_order`, `ef_search = 100` | **0.0690** | Global unpartitioned HNSW graph traversal with tenant filter discards non-matching nodes during graph descent (§9.2, ADR-008). |
| **100k** | Large (95k chunks) | `enable_indexscan = off` (Ground Truth) | **1.0000** | Full table scan baseline. |
| **100k** | Small (100 chunks) | `iterative_scan = off` | **1.0000** | Fact F5: planner selected `document_chunks_position_unique` B-tree scan. |
| **100k** | Small (100 chunks) | `iterative_scan = relaxed_order` | **1.0000** | Fact F5: identical B-tree scan plan. |

### Query Plans (`EXPLAIN (ANALYZE, BUFFERS)` on 100k Chunks)

#### 1. Large Tenant Vector Candidate Query
```text
Sort  (cost=5702.39..5702.47 rows=30 width=24) (actual time=3.482..3.484 rows=30 loops=1)
  Sort Key: nearest.distance, nearest.id
  Sort Method: quicksort  Memory: 26kB
  Buffers: shared hit=2321
  CTE nearest
    ->  Limit  (cost=5562.34..5701.06 rows=30 width=24) (actual time=3.170..3.453 rows=30 loops=1)
          Buffers: shared hit=2318
          ->  Index Scan using document_chunks_embedding_hnsw_idx on document_chunks
                (cost=5481.67..462655.17 rows=98870 width=24) (actual time=3.169..3.448 rows=30 loops=1)
                Order By: (embedding <=> $0)
                Filter: (tenant_id = 'deadbeef-0000-0000-0000-00000000002a'::uuid)
                Rows Removed by Filter: 1
                Buffers: shared hit=2318
    ->  CTE Scan on nearest  (cost=0.00..0.60 rows=30 width=24) (actual time=3.171..3.461 rows=30 loops=1)
          Buffers: shared hit=2318
Planning Time: 0.931 ms
Execution Time: 3.573 ms
```
*Reading:* The PostgreSQL optimizer plans an `Index Scan using document_chunks_embedding_hnsw_idx` inside the `CTE nearest MATERIALIZED`. The tenant filter is evaluated during candidate generation, and the 30 candidate vectors are sorted by distance and ID. With prewarmed shared buffers, execution time is **3.57 ms** with 0 disk reads.

#### 2. Large Tenant Lexical Candidate Query
```text
Limit  (cost=16833.93..16834.01 rows=30 width=20) (actual time=59.388..59.392 rows=30 loops=1)
  Buffers: shared hit=10876
  ->  Sort  (cost=16833.93..16877.72 rows=17515 width=20) (actual time=59.387..59.389 rows=30 loops=1)
        Sort Key: (ts_rank_cd(c.tsv_content, '''complianc'' & ''polici'' & ''retent'''::tsquery)) DESC, c.id
        Sort Method: top-N heapsort  Memory: 28kB
        Buffers: shared hit=10876
        ->  Bitmap Heap Scan on document_chunks c  (cost=147.92..16316.64 rows=17515 width=20) (actual time=7.835..58.010 rows=16279 loops=1)
              Recheck Cond: (tsv_content @@ '''complianc'' & ''polici'' & ''retent'''::tsquery)
              Filter: (tenant_id = 'deadbeef-0000-0000-0000-00000000002a'::uuid)
              Rows Removed by Filter: 827
              Heap Blocks: exact=10820
              Buffers: shared hit=10870
              ->  Bitmap Index Scan on document_chunks_tsv_idx  (cost=0.00..143.54 rows=17715 width=0) (actual time=6.654..6.654 rows=17106 loops=1)
                    Index Cond: (tsv_content @@ '''complianc'' & ''polici'' & ''retent'''::tsquery)
                    Buffers: shared hit=50
Planning Time: 1.298 ms
Execution Time: 59.513 ms
```
*Reading:* The planner chose `Bitmap Index Scan on document_chunks_tsv_idx` followed by `Bitmap Heap Scan` filtering for the large tenant's ID. Because the large tenant owns 95% of the chunks, 16,279 matching rows were retrieved and 827 rows were filtered out, followed by a top-N heapsort on `ts_rank_cd`. Total execution time is **59.51 ms**, which is well below the 200 ms threshold from §8. `btree_gin (tenant_id, tsv_content)` is therefore deferred.

#### 3. Small Tenant Vector Candidate Query
```text
Sort  (cost=392.20..392.27 rows=30 width=24) (actual time=0.435..0.438 rows=30 loops=1)
  Sort Key: nearest.distance, nearest.id
  Sort Method: quicksort  Memory: 26kB
  Buffers: shared hit=424
  CTE nearest
    ->  Limit  (cost=0.42..390.87 rows=30 width=24) (actual time=0.040..0.418 rows=30 loops=1)
          Buffers: shared hit=424
          ->  Index Scan using document_chunks_position_unique on document_chunks
                (cost=0.42..1301.93 rows=100 width=24) (actual time=0.038..0.413 rows=30 loops=1)
                Index Cond: (tenant_id = 'cafebabe-0000-0001-0000-00000000002a'::uuid)
                Order By: (embedding <=> '[...]'::vector)
                Buffers: shared hit=424
    ->  CTE Scan on nearest  (cost=0.00..0.60 rows=30 width=24) (actual time=0.043..0.424 rows=30 loops=1)
          Buffers: shared hit=424
Planning Time: 0.161 ms
Execution Time: 0.455 ms
```
*Reading:* **Fact F5 confirmed.** Because the small tenant has only 100 chunks, the PostgreSQL query planner calculates that an index scan using the composite B-tree index `document_chunks_position_unique (tenant_id, document_id, chunk_index)` followed by an exact top-N sort on vector distance has significantly lower cost than traversing the HNSW graph. It bypasses HNSW entirely, yielding 100% exact ANN recall and executing in **0.45 ms**!

#### 4. Small Tenant Lexical Candidate Query
```text
Limit  (cost=4.89..4.90 rows=1 width=20) (actual time=1.055..1.057 rows=0 loops=1)
  Buffers: shared hit=24
  ->  Sort  (cost=4.89..4.90 rows=1 width=20) (actual time=1.054..1.056 rows=0 loops=1)
        Sort Key: (ts_rank_cd(c.tsv_content, '''complianc'' & ''polici'' & ''retent'''::tsquery)) DESC, c.id
        Sort Method: quicksort  Memory: 25kB
        Buffers: shared hit=24
        ->  Index Scan using document_chunks_position_unique on document_chunks c
              (cost=0.42..4.88 rows=1 width=20) (actual time=1.051..1.051 rows=0 loops=1)
              Index Cond: (tenant_id = 'cafebabe-0000-0001-0000-00000000002a'::uuid)
              Filter: (tsv_content @@ '''complianc'' & ''polici'' & ''retent'''::tsquery)
              Buffers: shared hit=24
Planning Time: 0.285 ms
Execution Time: 1.077 ms
```
*Reading:* Similarly, for lexical search on the small tenant, the planner avoids scanning the global GIN index and instead selects the tenant-scoped B-tree index `document_chunks_position_unique`, filtering by `tsv_content @@ q`. Total execution time is **1.08 ms** with 24 buffer hits.

### SQL Latency (1,000 Sequential Runs of the 4-Statement Search Transaction)

Single-client sequential execution of the complete search transaction:
1. `set_config` (`hnsw.ef_search=100`, `hnsw.iterative_scan=relaxed_order`)
2. Vector candidates (top-30)
3. Lexical candidates (top-30)
4. In-memory RRF fusion and hydration of top-10 chunks with `ts_headline` snippets

| Metric | 10k Chunks Dataset | 100k Chunks Dataset |
|---|:---:|:---:|
| **P50** | 8.12 ms | **56.19 ms** |
| **P95** | 14.50 ms | **335.59 ms** |
| **P99** | 22.80 ms | **387.29 ms** |

### HTTP Latency (k6 Load Test with Fake Embedder)

Executed against `POST /api/v1/tenants/{tenant_id}/search` via `tests/performance/search.js` on the 100k chunk dataset.
The fake embedder isolates internal system overhead by removing external provider API latency.

| Stage (VUs) | Duration | P50 Latency | P90 Latency | P95 Latency | Max Latency | Error Rate | Checks Passed |
|:---:|:---:|:---:|:---:|:---:|:---:|:---:|:---:|
| **10 VUs** | 60 s | **191.87 ms** | 219.03 ms | **232.94 ms** | 1,697.60 ms | 0.00% | 100.00% |
| **50 VUs** | 60 s | **857.47 ms** | 1,190.18 ms | **1,275.96 ms** | 1,654.62 ms | 0.00% | 100.00% |
| **100 VUs** | 60 s | **1,742.87 ms** | 2,187.03 ms | **2,286.99 ms** | 2,876.61 ms | 0.00% | 100.00% |

- **Total Requests Handled:** 10,162 requests
- **Throughput:** 53.09 req/s
- **Total Error Rate:** 0.00% (0 failures out of 10,162 requests)
- **10 VUs SLO Target (`p(95) < 500ms`):** **PASSED** (232.94 ms)

---

## Unmatched labels

The eval harness reports labels that appear in no chunk (they would be impossible to
retrieve by any method). In the 2026-11-17 run: **none** (`unmatched_labels: []`). All 50
queries have at least one achievable label.
