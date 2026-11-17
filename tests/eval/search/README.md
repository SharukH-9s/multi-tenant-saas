# Search evaluation dataset (v1)

Synthetic corpus and labelled queries for measuring search relevance (ADR-008 S11). Everything is written for this project, so there is no copyrighted text.

- `corpus/`: 25 short TXT/MD files (fictional company handbook, product specs with codes such as `SKU-4471`, an error catalog, FAQs, a glossary, two near-duplicate retention policies, and two long generated guides that produce several chunks each).
- `queries.jsonl`: 50 queries, one JSON object per line.

## Query format

```json
{"id": "q019", "kind": "paraphrase", "query": "how many vacation days roll over",
 "relevant": [{"doc": "handbook_leave.md", "phrase": "carries over up to ten days"}]}
```

- `kind` is one of `exact_keyword`, `identifier`, `acronym`, `paraphrase`, `multi_chunk`, `ambiguous`, `similar_docs`, `boundary`.
- `relevant` lists labels. A chunk satisfies a label when it belongs to the document `doc` and its `content` contains `phrase` after lower-casing and collapsing whitespace. Labels are phrases, not chunk ids, so they stay valid when the chunker changes.
- `"grade": 2` or `1` is optional graded relevance (default 1). It only affects nDCG.
- A label that no chunk contains is reported as `unmatched label` and left out of the metrics.
- **`boundary` queries:** the answer sits in the paragraph that follows the one naming the rule, and does not repeat the rule name. The phrase is inside one paragraph, so it is always findable; the query measures whether retrieval still works when the context lives in the neighbouring chunk (Phase 5 T9).

## Adding a query

1. Add the document to `corpus/` if needed (keep files below a few KB).
2. Append a line to `queries.jsonl` with the next free id.
3. Copy the phrase from the document text, not from memory, and run the harness: it lists any label that matches no chunk.

## Running

CI-style smoke run with the offline fake embedder. It asserts only that `exact_keyword` and `identifier` hybrid Recall@10 are 1.0, because fake vectors carry no meaning:

```bash
EVAL_MODE=smoke cargo test -p integration-tests --test integration search_eval -- --ignored --nocapture
```

Real numbers need a real embedding provider. Embedding the corpus and 150 queries costs a fraction of a cent with `text-embedding-3-small`:

```bash
EMBEDDING_PROVIDER=openai EMBEDDING_API_KEY=... cargo test -p integration-tests --test integration search_eval -- --ignored --nocapture
```

The run prints a table and writes `target/search-eval/report-<timestamp>.json`. It reports hybrid (through `search_service`), lexical-only and vector-only side by side, per kind and overall: Recall@5, Recall@10, MRR, nDCG@10 and latency. Relevance numbers measured with the fake embedder are meaningless; only the real-provider run may be quoted.
