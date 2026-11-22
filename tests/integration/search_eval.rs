//! Phase 5 T7: search relevance evaluation (ADR-008 S11).
//!
//! `#[ignore]`, so it never runs in a normal `cargo test`. It ingests the
//! corpus in `tests/eval/search/corpus` into a fresh tenant, runs every query
//! in `queries.jsonl` three ways (hybrid through `search_service`, lexical-only
//! and vector-only straight from `search_repo`) and writes a JSON report to
//! `target/search-eval/`. See `tests/eval/search/README.md`.
//!
//! The metric functions are pure and unit-tested below.

use std::{collections::BTreeMap, path::PathBuf, time::Instant};

use common_auth::{TenantContext, TenantRole};
use common_database::begin_tenant_tx;
use common_messaging::IngestionEvent;
use serde_json::{json, Value};
use tenant_saas::{
    domain::search::SearchQuery,
    repositories::search_repo,
    services::{
        ingestion_service::{self, IngestionDeps, ProcessOutcome},
        search_service,
    },
};
use test_support::TestApp;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Pure metrics
// ---------------------------------------------------------------------------

/// One labelled relevant item: where it first appears in the ranking (0-based)
/// and its grade.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Found {
    position: Option<usize>,
    grade: f64,
}

fn recall_at(found: &[Found], k: usize) -> f64 {
    if found.is_empty() {
        return 0.0;
    }
    let hits = found
        .iter()
        .filter(|f| f.position.is_some_and(|p| p < k))
        .count();
    hits as f64 / found.len() as f64
}

/// Reciprocal rank of the first relevant hit; 0 when nothing is found.
fn reciprocal_rank(found: &[Found]) -> f64 {
    found
        .iter()
        .filter_map(|f| f.position)
        .min()
        .map_or(0.0, |p| 1.0 / (p as f64 + 1.0))
}

fn ndcg_at(found: &[Found], k: usize) -> f64 {
    let dcg: f64 = found
        .iter()
        .filter_map(|f| f.position.filter(|p| *p < k).map(|p| (p, f.grade)))
        .map(|(p, g)| g / (p as f64 + 2.0).log2())
        .sum();
    let mut grades: Vec<f64> = found.iter().map(|f| f.grade).collect();
    grades.sort_by(|a, b| b.total_cmp(a));
    let ideal: f64 = grades
        .iter()
        .take(k)
        .enumerate()
        .map(|(i, g)| g / (i as f64 + 2.0).log2())
        .sum();
    if ideal == 0.0 {
        0.0
    } else {
        dcg / ideal
    }
}

fn norm(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

// ---------------------------------------------------------------------------
// Dataset
// ---------------------------------------------------------------------------

struct Label {
    doc: String,
    phrase: String,
    grade: f64,
}

struct EvalQuery {
    id: String,
    kind: String,
    text: String,
    relevant: Vec<Label>,
}

fn eval_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("eval/search")
}

fn load_queries() -> Vec<EvalQuery> {
    let raw = std::fs::read_to_string(eval_dir().join("queries.jsonl")).unwrap();
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let v: Value = serde_json::from_str(l).unwrap();
            EvalQuery {
                id: v["id"].as_str().unwrap().into(),
                kind: v["kind"].as_str().unwrap().into(),
                text: v["query"].as_str().unwrap().into(),
                relevant: v["relevant"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|r| Label {
                        doc: r["doc"].as_str().unwrap().into(),
                        phrase: norm(r["phrase"].as_str().unwrap()),
                        grade: r["grade"].as_f64().unwrap_or(1.0),
                    })
                    .collect(),
            }
        })
        .collect()
}

/// A ranked result reduced to what relevance needs.
struct Ranked {
    title: String,
    content: String, // normalised
}

fn locate(labels: &[&Label], ranking: &[Ranked]) -> Vec<Found> {
    labels
        .iter()
        .map(|l| Found {
            position: ranking
                .iter()
                .position(|r| r.title == l.doc && r.content.contains(&l.phrase)),
            grade: l.grade,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Running the three modes
// ---------------------------------------------------------------------------

#[derive(Default, Clone)]
struct Scores {
    n: usize,
    recall5: f64,
    recall10: f64,
    mrr: f64,
    ndcg10: f64,
    latency_ms: f64,
}

impl Scores {
    fn add(&mut self, found: &[Found], latency_ms: f64) {
        self.n += 1;
        self.recall5 += recall_at(found, 5);
        self.recall10 += recall_at(found, 10);
        self.mrr += reciprocal_rank(found);
        self.ndcg10 += ndcg_at(found, 10);
        self.latency_ms += latency_ms;
    }

    fn mean(&self) -> Value {
        let n = self.n.max(1) as f64;
        json!({
            "queries": self.n,
            "recall_at_5": round(self.recall5 / n),
            "recall_at_10": round(self.recall10 / n),
            "mrr": round(self.mrr / n),
            "ndcg_at_10": round(self.ndcg10 / n),
            "avg_latency_ms": round(self.latency_ms / n),
        })
    }
}

fn round(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

async fn ingest_corpus(app: &TestApp) -> (Uuid, Uuid, String) {
    let session = app.create_session().await;
    let tenant_id = app
        .seed_membership(session.user_id, "owner", "active")
        .await;

    let mut files: Vec<PathBuf> = std::fs::read_dir(eval_dir().join("corpus"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    files.sort();
    for path in &files {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let bytes = std::fs::read(path).unwrap();
        let (status, body) = app
            .upload_file(tenant_id, &session.access_token, &name, &bytes, Some(&name))
            .await;
        assert_eq!(status, axum::http::StatusCode::ACCEPTED, "{name}: {body}");
    }

    let jobs: Vec<(Uuid, Uuid)> =
        sqlx::query_as("SELECT id, document_id FROM ingestion_jobs WHERE tenant_id = $1")
            .bind(tenant_id)
            .fetch_all(app.owner())
            .await
            .unwrap();
    let deps = IngestionDeps::new(app.state.clone(), format!("eval-{}", Uuid::new_v4()));
    for (job_id, document_id) in jobs {
        let out =
            ingestion_service::process(&deps, &IngestionEvent::new(job_id, tenant_id, document_id))
                .await;
        assert!(matches!(out, ProcessOutcome::Succeeded { .. }), "{out:?}");
    }
    (tenant_id, session.user_id, session.access_token)
}

async fn run_hybrid(
    app: &TestApp,
    ctx: &tenant_saas::auth::RequestContext,
    text: &str,
) -> (Vec<Ranked>, f64) {
    let q = SearchQuery::parse(text, Some(10), None, &app.state.config.search).unwrap();
    let started = Instant::now();
    let out = search_service::search(&app.state, ctx, q).await.unwrap();
    let ms = started.elapsed().as_secs_f64() * 1000.0;
    let ranked = out
        .hits
        .into_iter()
        .map(|h| Ranked {
            title: h.document_title,
            content: norm(&h.content),
        })
        .collect();
    (ranked, ms)
}

/// Lexical-only or vector-only from the repository, top 10.
async fn run_branch(
    app: &TestApp,
    ctx: &TenantContext,
    text: &str,
    vector: bool,
) -> (Vec<Ranked>, f64) {
    let cfg = &app.state.config.search;
    let started = Instant::now();
    let query_vec = if vector {
        Some(
            app.state
                .embedder
                .embed(&[text.to_string()])
                .await
                .unwrap()
                .remove(0),
        )
    } else {
        None
    };
    let mut tx = begin_tenant_tx(&app.state.db, ctx).await.unwrap();
    search_repo::apply_settings(&mut tx, cfg).await.unwrap();
    let target = tenant_saas::domain::chunk::ChunkTarget::Shared;
    let cands = match &query_vec {
        Some(v) => search_repo::vector_candidates(&mut tx, &target, ctx.tenant_id(), v, None, 30)
            .await
            .unwrap(),
        None => search_repo::lexical_candidates(&mut tx, &target, ctx.tenant_id(), text, None, 30)
            .await
            .unwrap(),
    };
    let ids: Vec<Uuid> = cands.iter().take(10).map(|c| c.chunk_id).collect();
    let rows = search_repo::hydrate(&mut tx, &target, ctx.tenant_id(), &ids, text)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let ms = started.elapsed().as_secs_f64() * 1000.0;
    let ranked = ids
        .iter()
        .filter_map(|id| rows.iter().find(|r| r.id == *id))
        .map(|r| Ranked {
            title: r.title.clone(),
            content: norm(&r.content),
        })
        .collect();
    (ranked, ms)
}

#[tokio::test]
#[ignore = "evaluation run: needs the database; real numbers need EMBEDDING_PROVIDER=openai"]
async fn search_eval() {
    let Some(app) = TestApp::spawn().await else {
        return;
    };
    let provider = format!(
        "{} ({})",
        app.state.config.ingestion.embedding_provider,
        app.state.embedder.model()
    );
    let (tenant_id, user_id, _token) = ingest_corpus(&app).await;
    let tc = TenantContext::from_verified_membership(tenant_id, user_id, TenantRole::Owner);
    let ctx = tenant_saas::auth::RequestContext::for_shared(tc);

    // Every chunk of the corpus, to find which labels can be matched at all.
    let chunks: Vec<(String, String)> = sqlx::query_as(
        "SELECT d.title, c.content FROM document_chunks c \
           JOIN documents d ON d.id = c.document_id AND d.tenant_id = c.tenant_id \
          WHERE c.tenant_id = $1",
    )
    .bind(tenant_id)
    .fetch_all(app.owner())
    .await
    .unwrap();
    let all: Vec<Ranked> = chunks
        .into_iter()
        .map(|(title, content)| Ranked {
            title,
            content: norm(&content),
        })
        .collect();

    let queries = load_queries();
    let modes = ["hybrid", "lexical", "vector"];
    let mut by_mode: BTreeMap<&str, BTreeMap<String, Scores>> = BTreeMap::new();
    let mut unmatched: Vec<String> = Vec::new();
    let mut per_query: Vec<Value> = Vec::new();

    for q in &queries {
        // Labels no chunk contains cannot be retrieved by anyone: report them
        // and leave them out of the denominators.
        let achievable: Vec<&Label> = q
            .relevant
            .iter()
            .filter(|l| locate(&[l], &all)[0].position.is_some())
            .collect();
        if achievable.len() != q.relevant.len() {
            unmatched.push(format!(
                "{} ({}): {} of {} labels match no chunk",
                q.id,
                q.kind,
                q.relevant.len() - achievable.len(),
                q.relevant.len()
            ));
        }
        if achievable.is_empty() {
            continue;
        }

        let expected_labels: Vec<Value> = achievable
            .iter()
            .map(|l| json!({ "doc": l.doc, "phrase": l.phrase, "grade": l.grade }))
            .collect();
        let mut row = json!({
            "id": q.id,
            "kind": q.kind,
            "query": q.text,
            "expected": expected_labels,
        });
        for mode in modes {
            let (ranked, ms) = match mode {
                "hybrid" => run_hybrid(&app, &ctx, &q.text).await,
                "lexical" => run_branch(&app, &ctx, &q.text, false).await,
                _ => run_branch(&app, &ctx, &q.text, true).await,
            };
            let found = locate(&achievable, &ranked);
            let m = by_mode.entry(mode).or_default();
            m.entry(q.kind.clone()).or_default().add(&found, ms);
            m.entry("ALL".into()).or_default().add(&found, ms);

            let matched_positions: Vec<Option<usize>> = found.iter().map(|f| f.position).collect();
            let retrieved_top_docs: Vec<String> =
                ranked.iter().take(5).map(|r| r.title.clone()).collect();

            row[mode] = json!({
                "recall_at_5": round(recall_at(&found, 5)),
                "recall_at_10": round(recall_at(&found, 10)),
                "mrr": round(reciprocal_rank(&found)),
                "ndcg_at_10": round(ndcg_at(&found, 10)),
                "latency_ms": round(ms),
                "matched_positions": matched_positions,
                "retrieved_top_docs": retrieved_top_docs,
            });
        }
        per_query.push(row);
    }

    // ----- report -----
    let summary: Value = by_mode
        .iter()
        .map(|(mode, kinds)| {
            let k: serde_json::Map<String, Value> =
                kinds.iter().map(|(k, s)| (k.clone(), s.mean())).collect();
            ((*mode).to_string(), Value::Object(k))
        })
        .collect::<serde_json::Map<_, _>>()
        .into();

    let git_commit = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".into());

    let search_config = json!({
        "vector_candidates": app.state.config.search.vector_candidates,
        "lexical_candidates": app.state.config.search.lexical_candidates,
        "rrf_k": app.state.config.search.rrf_k,
        "ef_search": app.state.config.search.ef_search,
        "default_limit": app.state.config.search.default_limit,
    });

    let report = json!({
        "dataset": "v1",
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "git_commit": git_commit,
        "environment": {
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
        },
        "search_config": search_config,
        "embedding_model": provider,
        "chunks_in_corpus": all.len(),
        "queries_total": queries.len(),
        "unmatched_labels": unmatched,
        "summary": summary,
        "per_query": per_query,
    });
    let out_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/search-eval");
    std::fs::create_dir_all(&out_dir).unwrap();
    let file = out_dir.join(format!(
        "report-{}.json",
        chrono::Utc::now().format("%Y%m%dT%H%M%SZ")
    ));
    std::fs::write(&file, serde_json::to_string_pretty(&report).unwrap()).unwrap();

    println!("\nembedding model: {provider}   chunks: {}", all.len());
    println!(
        "{:<8} {:<14} {:>3} {:>8} {:>9} {:>6} {:>8} {:>8}",
        "mode", "kind", "n", "R@5", "R@10", "MRR", "nDCG@10", "ms"
    );
    for (mode, kinds) in &by_mode {
        for (kind, s) in kinds {
            let n = s.n.max(1) as f64;
            println!(
                "{:<8} {:<14} {:>3} {:>8.3} {:>9.3} {:>6.3} {:>8.3} {:>8.1}",
                mode,
                kind,
                s.n,
                s.recall5 / n,
                s.recall10 / n,
                s.mrr / n,
                s.ndcg10 / n,
                s.latency_ms / n
            );
        }
    }
    for u in &unmatched {
        println!("unmatched label: {u}");
    }
    println!("report: {}", file.display());

    // Smoke mode: the fake embedder has no semantics, so only keyword-shaped
    // queries are asserted.
    if std::env::var("EVAL_MODE").as_deref() == Ok("smoke") {
        for kind in ["exact_keyword", "identifier"] {
            let s = &by_mode["hybrid"][kind];
            assert_eq!(
                s.recall10 / s.n as f64,
                1.0,
                "hybrid Recall@10 for {kind} must be 1.0"
            );
        }
    }
}

#[cfg(test)]
mod metric_tests {
    use super::*;

    fn f(position: Option<usize>, grade: f64) -> Found {
        Found { position, grade }
    }

    #[test]
    fn recall_counts_labels_inside_the_cutoff() {
        let found = [
            f(Some(0), 1.0),
            f(Some(7), 1.0),
            f(None, 1.0),
            f(Some(4), 1.0),
        ];
        assert_eq!(recall_at(&found, 5), 0.5);
        assert_eq!(recall_at(&found, 10), 0.75);
        assert_eq!(recall_at(&[], 10), 0.0);
    }

    #[test]
    fn reciprocal_rank_uses_the_first_hit() {
        assert_eq!(
            reciprocal_rank(&[f(Some(2), 1.0), f(Some(5), 1.0)]),
            1.0 / 3.0
        );
        assert_eq!(reciprocal_rank(&[f(None, 1.0)]), 0.0);
    }

    #[test]
    fn ndcg_is_one_for_a_perfect_ranking_and_lower_when_misordered() {
        let perfect = [f(Some(0), 2.0), f(Some(1), 1.0)];
        assert!((ndcg_at(&perfect, 10) - 1.0).abs() < 1e-12);

        // grade 1 first, grade 2 second:
        // dcg = 1/log2(2) + 2/log2(3) = 1 + 1.261860; ideal = 2/1 + 1/log2(3) = 2.630930
        let swapped = [f(Some(1), 2.0), f(Some(0), 1.0)];
        let expected = (1.0 + 2.0 / 3f64.log2()) / (2.0 + 1.0 / 3f64.log2());
        assert!((ndcg_at(&swapped, 10) - expected).abs() < 1e-12);

        assert_eq!(ndcg_at(&[f(None, 1.0)], 10), 0.0);
    }

    #[test]
    fn norm_collapses_whitespace_and_case() {
        assert_eq!(norm("  Hello\n\n  WORLD\t!"), "hello world !");
    }
}
