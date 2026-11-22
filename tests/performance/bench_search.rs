//! Benchmark runner for Phase 5 search validation (ADR-008, Option B.1).
//!
//! Validates:
//!   1. ANN Recall under both Representative (in-distribution) and Uniform Random query vectors.
//!   2. Shared Table HNSW (with tenant filter) vs Dedicated Table HNSW (isolated index).
//!   3. Parameter sensitivity across ef_search and max_scan_tuples under warm cache.
//!   4. Query execution plans via EXPLAIN (ANALYZE, BUFFERS).
//!   5. End-to-end 4-statement SQL search transaction latency (P50/P95/P99).

use std::collections::HashSet;
use std::f64;
use std::fmt::Write as _;
use std::time::Instant;

use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use uuid::Uuid;

const DIM: usize = 1536;

struct Rng {
    s: [u64; 4],
}

impl Rng {
    fn new(seed: u64) -> Self {
        let mut x = seed;
        let mut next = || {
            x = x.wrapping_add(0x9e3779b97f4a7c15);
            let mut z = x;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
            z ^ (z >> 31)
        };
        Self {
            s: [next(), next(), next(), next()],
        }
    }

    fn next_u64(&mut self) -> u64 {
        let result = self.s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = self.s[1] << 17;
        self.s[2] ^= self.s[0];
        self.s[3] ^= self.s[1];
        self.s[1] ^= self.s[2];
        self.s[0] ^= self.s[3];
        self.s[2] ^= t;
        self.s[3] = self.s[3].rotate_left(45);
        result
    }

    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn next_normal(&mut self) -> f64 {
        let u1 = self.next_f64().max(1e-15);
        let u2 = self.next_f64();
        (-2.0 * u1.ln()).sqrt() * (2.0 * f64::consts::PI * u2).cos()
    }
}

fn random_unit_vec(rng: &mut Rng) -> Vec<f32> {
    let raw: Vec<f64> = (0..DIM).map(|_| rng.next_normal()).collect();
    let norm = raw.iter().map(|x| x * x).sum::<f64>().sqrt().max(1e-12);
    raw.iter().map(|x| (x / norm) as f32).collect()
}

fn parse_vector(s: &str) -> Vec<f32> {
    let trimmed = s.trim_matches(|c| c == '[' || c == ']');
    trimmed
        .split(',')
        .filter_map(|x| x.trim().parse::<f32>().ok())
        .collect()
}

fn perturbed_vec(base: &[f32], sigma: f64, rng: &mut Rng) -> Vec<f32> {
    let raw: Vec<f64> = base
        .iter()
        .map(|&c| c as f64 + sigma * rng.next_normal())
        .collect();
    let norm = raw.iter().map(|x| x * x).sum::<f64>().sqrt().max(1e-12);
    raw.iter().map(|x| (x / norm) as f32).collect()
}

fn vector_literal(v: &[f32]) -> String {
    let mut s = String::with_capacity(v.len() * 10 + 2);
    s.push('[');
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        write!(s, "{x:.5}").unwrap();
    }
    s.push(']');
    s
}

const SHARED_VECTOR_SQL: &str = "\
WITH nearest AS MATERIALIZED ( \
    SELECT id, embedding <=> $2::vector AS distance \
      FROM public.document_chunks \
     WHERE tenant_id = $1 \
     ORDER BY embedding <=> $2::vector \
     LIMIT $3 \
) \
SELECT id FROM nearest ORDER BY distance, id";

const DEDICATED_VECTOR_SQL: &str = "\
WITH nearest AS MATERIALIZED ( \
    SELECT id, embedding <=> $1::vector AS distance \
      FROM tenant_isolated.document_chunks \
     ORDER BY embedding <=> $1::vector \
     LIMIT $2 \
) \
SELECT id FROM nearest ORDER BY distance, id";

const LEXICAL_CANDIDATE_SQL: &str = "\
SELECT c.id \
  FROM public.document_chunks c, websearch_to_tsquery('english', $2) AS q \
 WHERE c.tenant_id = $1 \
   AND ($3::uuid[] IS NULL OR c.document_id = ANY($3)) \
   AND c.tsv_content @@ q \
 ORDER BY ts_rank_cd(c.tsv_content, q) DESC, c.id \
 LIMIT $4";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();

    let args: Vec<String> = std::env::args().collect();
    let get_arg = |flag: &str, default: &str| -> String {
        args.windows(2)
            .find(|w| w[0] == flag)
            .map(|w| w[1].clone())
            .unwrap_or_else(|| default.to_string())
    };

    let large_tenant: Uuid =
        get_arg("--large-tenant", "deadbeef-0000-0000-0000-00000000002a").parse()?;
    let _small_tenant: Uuid =
        get_arg("--small-tenant", "cafebabe-0000-0001-0000-00000000002a").parse()?;
    let seed: u64 = get_arg("--seed", "12345").parse()?;
    let sql_runs: usize = get_arg("--sql-runs", "50").parse()?;
    let num_queries: usize = get_arg("--num-queries", "50").parse()?;
    let ef_search: String = get_arg("--ef-search", "300");
    let max_scan_tuples: String = get_arg("--max-scan-tuples", "50000");
    let iterative_scan: String = get_arg("--iterative-scan", "relaxed_order");

    let has_flag = |flag: &str| -> bool { args.iter().any(|a| a == flag) };
    let _skip_reindex = has_flag("--skip-reindex");
    let skip_recall = has_flag("--skip-recall");
    let skip_explain = has_flag("--skip-explain");

    let url = if let Some(pos) = args.iter().position(|a| a == "--database-url") {
        args.get(pos + 1)
            .cloned()
            .expect("--database-url requires an argument")
    } else {
        std::env::var("DATABASE_BENCH_URL")
            .or_else(|_| std::env::var("DATABASE_MIGRATION_URL"))
            .expect("DATABASE_BENCH_URL or DATABASE_MIGRATION_URL is required")
    };

    println!("Connecting to bench DB...");
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await?;

    let mut rng = Rng::new(seed);

    // -----------------------------------------------------------------------
    // Pre-warm Cache
    // -----------------------------------------------------------------------
    println!("\n=== Pre-warming Shared and Dedicated Relations ===");
    let _ = sqlx::query("SELECT pg_prewarm('public.document_chunks')").execute(&pool).await;
    let _ = sqlx::query("SELECT pg_prewarm('public.document_chunks_embedding_hnsw_idx')").execute(&pool).await;
    let _ = sqlx::query("SELECT pg_prewarm('tenant_isolated.document_chunks')").execute(&pool).await;
    let _ = sqlx::query("SELECT pg_prewarm('tenant_isolated.document_chunks_embedding_hnsw_idx')").execute(&pool).await;
    println!("Pre-warm complete.");

    // -----------------------------------------------------------------------
    // 1. REINDEX and Sizes
    // -----------------------------------------------------------------------
    println!("\n=== Index Sizes ===");
    let sizes: Vec<(String, i64)> = sqlx::query_as(
        "SELECT relname::text, pg_relation_size(c.oid)
         FROM pg_class c
         JOIN pg_namespace n ON n.oid = c.relnamespace
         WHERE relname IN (
            'document_chunks', 'document_chunks_embedding_hnsw_idx', 'document_chunks_tsv_idx'
         )
         AND n.nspname IN ('public', 'tenant_isolated')",
    )
    .fetch_all(&pool)
    .await?;
    for (name, size) in &sizes {
        println!(
            "  {name}: {size} bytes ({:.2} MiB)",
            *size as f64 / 1_048_576.0
        );
    }

    // -----------------------------------------------------------------------
    // 2. Query Generation: Representative vs Uniform Random
    // -----------------------------------------------------------------------
    println!("\nGenerating {num_queries} queries for each distribution...");

    // 2.1 Uniform Random (Unrepresentative Baseline)
    let random_queries: Vec<String> = (0..num_queries)
        .map(|_| vector_literal(&random_unit_vec(&mut rng)))
        .collect();

    // 2.2 Representative (In-Distribution, sampled from large tenant corpus + perturbation)
    let raw_rows: Vec<(String,)> = sqlx::query_as(
        "SELECT embedding::text FROM public.document_chunks WHERE tenant_id = $1 LIMIT $2",
    )
    .bind(large_tenant)
    .bind(num_queries as i64)
    .fetch_all(&pool)
    .await?;

    let representative_queries: Vec<String> = raw_rows
        .into_iter()
        .map(|(s,)| {
            let base = parse_vector(&s);
            let perturbed = perturbed_vec(&base, 0.05, &mut rng);
            vector_literal(&perturbed)
        })
        .collect();
    println!("Generated {} random and {} representative queries.", random_queries.len(), representative_queries.len());

    // -----------------------------------------------------------------------
    // 3. ANN Recall: Shared vs Dedicated on Both Query Distributions
    // -----------------------------------------------------------------------
    if !skip_recall {
        println!("\n=== ANN Recall Study: Shared Table vs Isolated Dedicated Table ===");

        for (dist_name, q_list) in [("Uniform Random", &random_queries), ("Representative (In-Distribution)", &representative_queries)] {
            println!("\n>>> Evaluating Query Distribution: [{dist_name}] ({} queries, top-30) <<<", q_list.len());

            // Compute Ground Truth (Exact Brute-Force)
            println!("  Computing exact ground truth (enable_indexscan = off)...");
            let mut ground_truth_sets = Vec::with_capacity(q_list.len());
            for q_vec in q_list {
                let mut tx = pool.begin().await?;
                sqlx::query("SET LOCAL enable_indexscan = off").execute(&mut *tx).await?;
                let rows: Vec<(Uuid,)> = sqlx::query_as(SHARED_VECTOR_SQL)
                    .bind(large_tenant)
                    .bind(q_vec)
                    .bind(30i64)
                    .fetch_all(&mut *tx)
                    .await?;
                tx.rollback().await?;
                let set: HashSet<Uuid> = rows.into_iter().map(|(id,)| id).collect();
                ground_truth_sets.push(set);
            }

            // Benchmark Configurations to Test
            let configs = vec![
                ("100", "20000", "relaxed_order", "Baseline (ef=100, scan=20k)"),
                ("300", "50000", "relaxed_order", "Tuned (ef=300, scan=50k)"),
            ];

            // A. Shared Table
            for (ef, max_scan, iter_mode, label) in &configs {
                let mut recalls = Vec::with_capacity(q_list.len());
                let mut latencies_ms = Vec::with_capacity(q_list.len());

                for (q_vec, gt) in q_list.iter().zip(&ground_truth_sets) {
                    let mut tx = pool.begin().await?;
                    sqlx::query("SET LOCAL enable_seqscan = off").execute(&mut *tx).await?;
                    sqlx::query("SET LOCAL enable_indexscan = on").execute(&mut *tx).await?;
                    sqlx::query(
                        "SELECT set_config('hnsw.ef_search', $1, true), \
                                set_config('hnsw.iterative_scan', $2, true), \
                                set_config('hnsw.max_scan_tuples', $3, true)",
                    )
                    .bind(ef)
                    .bind(iter_mode)
                    .bind(max_scan)
                    .execute(&mut *tx)
                    .await?;

                    let t0 = Instant::now();
                    let rows: Vec<(Uuid,)> = sqlx::query_as(SHARED_VECTOR_SQL)
                        .bind(large_tenant)
                        .bind(q_vec)
                        .bind(30i64)
                        .fetch_all(&mut *tx)
                        .await?;
                    let elapsed = t0.elapsed().as_secs_f64() * 1000.0;
                    latencies_ms.push(elapsed);
                    tx.rollback().await?;

                    let result_set: HashSet<Uuid> = rows.into_iter().map(|(id,)| id).collect();
                    let matches = gt.intersection(&result_set).count();
                    recalls.push(matches as f64 / gt.len().max(1) as f64);
                }

                latencies_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
                let avg_recall: f64 = recalls.iter().sum::<f64>() / recalls.len() as f64;
                let p50 = latencies_ms[latencies_ms.len() * 50 / 100];
                let p95 = latencies_ms[latencies_ms.len() * 95 / 100];
                let avg_lat = latencies_ms.iter().sum::<f64>() / latencies_ms.len() as f64;

                println!(
                    "  [SHARED TABLE]    {label:<28} => Recall@30 = {avg_recall:.4} | P50 = {p50:6.2} ms | P95 = {p95:6.2} ms | Avg = {avg_lat:6.2} ms"
                );
            }

            // B. Dedicated Table
            for (ef, max_scan, iter_mode, label) in &configs {
                let mut recalls = Vec::with_capacity(q_list.len());
                let mut latencies_ms = Vec::with_capacity(q_list.len());

                for (q_vec, gt) in q_list.iter().zip(&ground_truth_sets) {
                    let mut tx = pool.begin().await?;
                    sqlx::query("SET LOCAL enable_seqscan = off").execute(&mut *tx).await?;
                    sqlx::query("SET LOCAL enable_indexscan = on").execute(&mut *tx).await?;
                    sqlx::query(
                        "SELECT set_config('hnsw.ef_search', $1, true), \
                                set_config('hnsw.iterative_scan', $2, true), \
                                set_config('hnsw.max_scan_tuples', $3, true)",
                    )
                    .bind(ef)
                    .bind(iter_mode)
                    .bind(max_scan)
                    .execute(&mut *tx)
                    .await?;

                    let t0 = Instant::now();
                    let rows: Vec<(Uuid,)> = sqlx::query_as(DEDICATED_VECTOR_SQL)
                        .bind(q_vec)
                        .bind(30i64)
                        .fetch_all(&mut *tx)
                        .await?;
                    let elapsed = t0.elapsed().as_secs_f64() * 1000.0;
                    latencies_ms.push(elapsed);
                    tx.rollback().await?;

                    let result_set: HashSet<Uuid> = rows.into_iter().map(|(id,)| id).collect();
                    let matches = gt.intersection(&result_set).count();
                    recalls.push(matches as f64 / gt.len().max(1) as f64);
                }

                latencies_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
                let avg_recall: f64 = recalls.iter().sum::<f64>() / recalls.len() as f64;
                let p50 = latencies_ms[latencies_ms.len() * 50 / 100];
                let p95 = latencies_ms[latencies_ms.len() * 95 / 100];
                let avg_lat = latencies_ms.iter().sum::<f64>() / latencies_ms.len() as f64;

                println!(
                    "  [DEDICATED TABLE] {label:<28} => Recall@30 = {avg_recall:.4} | P50 = {p50:6.2} ms | P95 = {p95:6.2} ms | Avg = {avg_lat:6.2} ms"
                );
            }
        }
    }

    // -----------------------------------------------------------------------
    // 4. Query Plans: EXPLAIN (ANALYZE, BUFFERS) with Representative Vector
    // -----------------------------------------------------------------------
    if !skip_explain {
        println!("\n=== Query Plans (Representative Query Vector) ===");
        let sample_vec = &representative_queries[0];

        async fn explain(pool: &PgPool, name: &str, query: String) -> anyhow::Result<String> {
            let rows: Vec<(String,)> = sqlx::query_as(sqlx::AssertSqlSafe(query))
                .fetch_all(pool)
                .await?;
            let plan = rows
                .into_iter()
                .map(|(s,)| s)
                .collect::<Vec<_>>()
                .join("\n");
            println!("\n--- {name} ---\n{plan}");
            Ok(plan)
        }

        let q_shared = format!(
            "EXPLAIN (ANALYZE, BUFFERS) \
             WITH nearest AS MATERIALIZED ( \
                 SELECT id, embedding <=> '{sample_vec}'::vector AS distance \
                   FROM public.document_chunks \
                  WHERE tenant_id = '{large_tenant}' \
                  ORDER BY embedding <=> '{sample_vec}'::vector \
                  LIMIT 30 \
             ) \
             SELECT id FROM nearest ORDER BY distance, id"
        );
        explain(&pool, "Shared Table Vector Candidate Plan (Representative Vector)", q_shared).await?;

        let q_dedicated = format!(
            "EXPLAIN (ANALYZE, BUFFERS) \
             WITH nearest AS MATERIALIZED ( \
                 SELECT id, embedding <=> '{sample_vec}'::vector AS distance \
                   FROM tenant_isolated.document_chunks \
                  ORDER BY embedding <=> '{sample_vec}'::vector \
                  LIMIT 30 \
             ) \
             SELECT id FROM nearest ORDER BY distance, id"
        );
        explain(&pool, "Dedicated Table Vector Candidate Plan (Representative Vector)", q_dedicated).await?;
    }

    // -----------------------------------------------------------------------
    // 5. SQL Latency: End-to-End 4-Statement Search Transaction
    // -----------------------------------------------------------------------
    println!("\n=== SQL Latency: {sql_runs} runs on Large Tenant (Warm Cache) ===");
    let mut latencies_us = Vec::with_capacity(sql_runs);

    let sample_queries = [
        "policy compliance retention",
        "authentication token session",
        "backup recovery incident schedule",
        "deployment configuration environment",
        "encryption certificate security access",
        "approval workflow request response",
        "threshold metric alert notification",
        "SKU-1000",
    ];

    for i in 0..sql_runs {
        let q_vec = &representative_queries[i % representative_queries.len()];
        let kw = sample_queries[i % sample_queries.len()];

        let t_start = Instant::now();
        let mut tx = pool.begin().await?;

        // 1. Settings
        sqlx::query(
            "SELECT set_config('hnsw.ef_search', $1, true), \
                    set_config('hnsw.iterative_scan', $2, true), \
                    set_config('hnsw.max_scan_tuples', $3, true), \
                    set_config('statement_timeout', '60000', true)",
        )
        .bind(&ef_search)
        .bind(&iterative_scan)
        .bind(&max_scan_tuples)
        .execute(&mut *tx)
        .await?;

        // 2. Vector candidates
        let vec_rows: Vec<(Uuid,)> = sqlx::query_as(SHARED_VECTOR_SQL)
            .bind(large_tenant)
            .bind(q_vec)
            .bind(30i64)
            .fetch_all(&mut *tx)
            .await?;

        // 3. Lexical candidates
        let lex_rows: Vec<(Uuid,)> = sqlx::query_as(LEXICAL_CANDIDATE_SQL)
            .bind(large_tenant)
            .bind(kw)
            .bind(None::<&[Uuid]>)
            .bind(30i64)
            .fetch_all(&mut *tx)
            .await?;

        // In-memory RRF (top 10 IDs)
        let mut combined_ids: Vec<Uuid> = vec_rows.into_iter().map(|(id,)| id).collect();
        for (id,) in lex_rows {
            if !combined_ids.contains(&id) {
                combined_ids.push(id);
            }
        }
        combined_ids.truncate(10);

        // 4. Hydrate
        let _hydrated: Vec<(Uuid,)> = sqlx::query_as(
            "SELECT c.id FROM public.document_chunks c \
             JOIN public.documents d ON d.tenant_id = c.tenant_id AND d.id = c.document_id \
             WHERE c.tenant_id = $1 AND c.id = ANY($2) AND d.deleted_at IS NULL",
        )
        .bind(large_tenant)
        .bind(&combined_ids)
        .fetch_all(&mut *tx)
        .await?;

        tx.commit().await?;

        let elapsed = t_start.elapsed().as_micros() as u64;
        latencies_us.push(elapsed);

        if (i + 1) % 10 == 0 || i + 1 == sql_runs {
            println!(
                "  completed {}/{} runs (last: {:.2} ms)",
                i + 1,
                sql_runs,
                elapsed as f64 / 1000.0
            );
        }
    }

    latencies_us.sort_unstable();
    let p50 = latencies_us[sql_runs * 50 / 100] as f64 / 1000.0;
    let p95 = latencies_us[sql_runs * 95 / 100] as f64 / 1000.0;
    let p99 = latencies_us[sql_runs * 99 / 100] as f64 / 1000.0;
    println!("SQL Latency Results ({sql_runs} runs, Representative Queries):");
    println!("  P50: {p50:.2} ms");
    println!("  P95: {p95:.2} ms");
    println!("  P99: {p99:.2} ms");

    println!("\nValidation benchmark complete.");
    Ok(())
}
