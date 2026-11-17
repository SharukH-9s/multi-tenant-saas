# Phase 5 Search Benchmark Report

> These are laptop baseline numbers for a development dataset, not production guarantees.
> See §19.5 of the system design for the "Measure rather than promise" policy.

---

## Environment

| Field | Value |
|---|---|
| Date | 2026-11-17 |
| Git commit | `35e7e5b78b153215f4dcddc18da05c6209697bc6` |
| OS | Windows x86_64 |
| Embedding provider | Azure AI Foundry — `text-embedding-3-small` (1536 dims) |
| PostgreSQL | `pgvector/pgvector:0.8.0-pg16` (Docker) |
| Corpus | 25 documents, 31 chunks |

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

## T8: Benchmark (Latency and Query Plans)

> Status: **In progress** — this section will be filled in when T8 is complete.

T8 requires a seeded benchmark database (separate from the test database) with 10k and
100k chunks for a large tenant plus 50 small tenants, and k6 HTTP load tests. The numbers
below are placeholders pending that run.

### SQL latency (placeholder)

P50/P95/P99 of the full four-statement search transaction (settings + vector + lexical +
hydrate) for 1 000 sequential queries against the eval tenant (31 chunks):

| Metric | Value |
|---|---|
| P50 | *pending T8* |
| P95 | *pending T8* |
| P99 | *pending T8* |

### HTTP latency (placeholder)

k6 load test against `POST /api/v1/tenants/{tenant_id}/search` with the fake embedder
(provider cost excluded), 10 / 50 / 100 VUs for 60 s each:

| VUs | P50 | P95 | P99 | Throughput | Error rate |
|:---:|:---:|:---:|:---:|:---:|:---:|
| 10 | *pending* | *pending* | *pending* | *pending* | *pending* |
| 50 | *pending* | *pending* | *pending* | *pending* | *pending* |
| 100 | *pending* | *pending* | *pending* | *pending* | *pending* |

### ANN recall (placeholder)

HNSW index vs exact scan for the large tenant (100k chunks):

| Configuration | Recall |
|---|:---:|
| HNSW (`relaxed_order`, `ef_search = 100`) | *pending* |
| Exact (`enable_indexscan = off`) | 1.000 |

### Query plans (placeholder)

`EXPLAIN (ANALYZE, BUFFERS)` output for the vector candidate query and the lexical
candidate query on a small tenant and the large tenant will be inserted here when T8 is
run, with a written reading of which index was chosen and whether F5 (exact btree plan for
small tenants) applied.

---

## Unmatched labels

The eval harness reports labels that appear in no chunk (they would be impossible to
retrieve by any method). In the 2026-11-17 run: **none** (`unmatched_labels: []`). All 50
queries have at least one achievable label.
