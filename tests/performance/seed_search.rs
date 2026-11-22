//! Benchmark seeder: populates `tenant_saas_bench` with clustered synthetic chunks.
//!
//! Never ships. Never calls an embedding provider.
//! Vectors: 64 random unit centroids, each chunk = centroid + Gaussian noise (σ ≈ 0.1),
//! then L2-normalised so cosine distance is correct (S12).
//!
//! Usage:
//!   cargo run --release --bin seed_search -- --chunks 10000 --reset
//!   cargo run --release --bin seed_search -- --chunks 100000 --reset
//!
//! Required env: DATABASE_BENCH_URL (e.g. postgres://db_owner:owner_password@localhost:5433/tenant_saas_bench)
//!               Falls back to DATABASE_MIGRATION_URL if not set.

use std::f64;
use std::fmt::Write as _;

use common_auth::{TokenService, TokenSettings};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// PRNG (xoshiro256** — deterministic, no external crate)
// ---------------------------------------------------------------------------
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

    fn next_uuid(&mut self) -> Uuid {
        Uuid::from_u128((self.next_u64() as u128) << 64 | (self.next_u64() as u128))
    }
}

// ---------------------------------------------------------------------------
// Vector helpers
// ---------------------------------------------------------------------------
const DIM: usize = 1536;

fn random_unit_vec(rng: &mut Rng) -> Vec<f32> {
    let raw: Vec<f64> = (0..DIM).map(|_| rng.next_normal()).collect();
    l2_normalize_f64(&raw)
}

fn l2_normalize_f64(v: &[f64]) -> Vec<f32> {
    let norm = v.iter().map(|x| x * x).sum::<f64>().sqrt().max(1e-12);
    v.iter().map(|x| (x / norm) as f32).collect()
}

fn perturbed_vec(centroid: &[f32], sigma: f64, rng: &mut Rng) -> Vec<f32> {
    let perturbed: Vec<f64> = centroid
        .iter()
        .map(|&c| c as f64 + sigma * rng.next_normal())
        .collect();
    l2_normalize_f64(&perturbed)
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

// ---------------------------------------------------------------------------
// Vocabulary for synthetic text
// ---------------------------------------------------------------------------
const VOCAB: &[&str] = &[
    "document",
    "policy",
    "section",
    "clause",
    "paragraph",
    "appendix",
    "requirement",
    "process",
    "system",
    "service",
    "configuration",
    "deployment",
    "tenant",
    "user",
    "role",
    "permission",
    "resource",
    "endpoint",
    "token",
    "audit",
    "compliance",
    "retention",
    "incident",
    "recovery",
    "backup",
    "schedule",
    "threshold",
    "metric",
    "alert",
    "notification",
    "workflow",
    "approval",
    "request",
    "response",
    "status",
    "code",
    "version",
    "release",
    "environment",
    "infrastructure",
    "network",
    "security",
    "encryption",
    "key",
    "certificate",
    "authentication",
    "authorization",
    "session",
    "access",
];

fn synthetic_content(chunk_idx: usize, doc_idx: usize, tenant_idx: usize, rng: &mut Rng) -> String {
    let identifier = format!("SKU-{:04}", (tenant_idx * 1000 + doc_idx) % 9000 + 1000);
    let words: Vec<&str> = (0..40)
        .map(|_| VOCAB[(rng.next_u64() as usize) % VOCAB.len()])
        .collect();
    format!(
        "Chunk {} of document {} (tenant {}). Reference {}. {}.",
        chunk_idx,
        doc_idx,
        tenant_idx,
        identifier,
        words.join(" ")
    )
}

// ---------------------------------------------------------------------------
// DB operations
// ---------------------------------------------------------------------------
async fn begin_tenant_tx(
    pool: &PgPool,
    tenant_id: Uuid,
    user_id: Uuid,
) -> anyhow::Result<Transaction<'static, Postgres>> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "SELECT set_config('app.current_tenant_id', $1, true),
                set_config('app.current_user_id',   $2, true)",
    )
    .bind(tenant_id.to_string())
    .bind(user_id.to_string())
    .execute(&mut *tx)
    .await?;
    Ok(tx)
}

async fn insert_user(pool: &PgPool, user_id: Uuid, email: &str) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO users (id, email, password_hash, status)
         VALUES ($1, $2, 'dummy_hash', 'active')
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(user_id)
    .bind(email)
    .execute(pool)
    .await?;
    Ok(())
}

async fn insert_tenant_and_membership(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    user_id: Uuid,
    tenant_name: &str,
    slug: &str,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO tenants (id, name, slug, status)
         VALUES ($1, $2, $3, 'active')
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(tenant_id)
    .bind(tenant_name)
    .bind(slug)
    .execute(&mut **tx)
    .await?;

    sqlx::query(
        "INSERT INTO memberships (tenant_id, user_id, role, status)
         VALUES ($1, $2, 'owner', 'active')
         ON CONFLICT (tenant_id, user_id) DO NOTHING",
    )
    .bind(tenant_id)
    .bind(user_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn insert_document(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    document_id: Uuid,
    user_id: Uuid,
    doc_idx: usize,
) -> anyhow::Result<()> {
    let raw = document_id.as_bytes();
    let content_hash: String = raw
        .iter()
        .cycle()
        .take(32)
        .flat_map(|b| {
            let hi = char::from_digit((b >> 4) as u32, 16).unwrap();
            let lo = char::from_digit((b & 0xf) as u32, 16).unwrap();
            [hi, lo]
        })
        .collect();

    sqlx::query(
        "INSERT INTO documents
             (id, tenant_id, uploaded_by, title, original_filename,
              content_hash, object_key, mime_type, size_bytes, status)
         VALUES ($1, $2, $3, $4, $5, $6, $7, 'text/plain', 1024, 'ready')
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(document_id)
    .bind(tenant_id)
    .bind(user_id)
    .bind(format!("Bench Doc {doc_idx}"))
    .bind(format!("bench-doc-{doc_idx}.txt"))
    .bind(content_hash)
    .bind(format!("bench/{}", document_id))
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn insert_chunks(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    document_id: Uuid,
    chunks: &[(i32, String, String)],
) -> anyhow::Result<()> {
    for batch in chunks.chunks(500) {
        let indices: Vec<i32> = batch.iter().map(|(i, _, _)| *i).collect();
        let contents: Vec<&str> = batch.iter().map(|(_, c, _)| c.as_str()).collect();
        let embeddings: Vec<&str> = batch.iter().map(|(_, _, e)| e.as_str()).collect();
        let token_counts: Vec<i32> = batch
            .iter()
            .map(|(_, c, _)| c.split_whitespace().count() as i32)
            .collect();
        let metadatas: Vec<&str> = batch
            .iter()
            .map(|_| r#"{"page_start":1,"page_end":1}"#)
            .collect();

        sqlx::query(
            "INSERT INTO document_chunks
                 (tenant_id, document_id, chunk_index, content, token_count,
                  embedding, embedding_model, embedding_version, metadata)
             SELECT $1, $2, t.idx, t.content, t.tokens, t.emb::vector, $3, $4, t.meta::jsonb
               FROM UNNEST($5::int[], $6::text[], $7::int[], $8::text[], $9::text[])
                    AS t(idx, content, tokens, emb, meta)
             ON CONFLICT (tenant_id, document_id, chunk_index) DO NOTHING",
        )
        .bind(tenant_id)
        .bind(document_id)
        .bind("text-embedding-3-small")
        .bind("p4-v1")
        .bind(&indices)
        .bind(&contents)
        .bind(&token_counts)
        .bind(&embeddings)
        .bind(&metadatas)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

async fn seed_tenant(
    pool: &PgPool,
    tenant_id: Uuid,
    user_id: Uuid,
    tenant_idx: usize,
    num_chunks: usize,
    centroids: &[Vec<f32>],
    rng: &mut Rng,
) -> anyhow::Result<()> {
    let email = format!("bench-u{}@example.com", user_id.simple());
    insert_user(pool, user_id, &email).await?;

    let chunks_per_doc = 20usize;
    let num_docs = num_chunks.div_ceil(chunks_per_doc);
    let mut inserted = 0usize;

    let slug = format!("bench-{}", tenant_id.simple());
    let name = format!("Bench Tenant {tenant_idx}");

    let mut tx_init = begin_tenant_tx(pool, tenant_id, user_id).await?;
    insert_tenant_and_membership(&mut tx_init, tenant_id, user_id, &name, &slug).await?;
    tx_init.commit().await?;

    let docs_per_tx = 50usize;
    for doc_batch_start in (0..num_docs).step_by(docs_per_tx) {
        let doc_batch_end = (doc_batch_start + docs_per_tx).min(num_docs);
        let mut tx = begin_tenant_tx(pool, tenant_id, user_id).await?;

        for doc_idx in doc_batch_start..doc_batch_end {
            let document_id = rng.next_uuid();

            let this_doc_chunks = if doc_idx == num_docs - 1 {
                num_chunks - inserted
            } else {
                chunks_per_doc.min(num_chunks - inserted)
            };
            if this_doc_chunks == 0 {
                break;
            }

            let chunk_data: Vec<(i32, String, String)> = (0..this_doc_chunks)
                .map(|ci| {
                    let centroid_idx = rng.next_u64() as usize % centroids.len();
                    let vec = perturbed_vec(&centroids[centroid_idx], 0.1, rng);
                    let content = synthetic_content(ci, doc_idx, tenant_idx, rng);
                    (ci as i32, content, vector_literal(&vec))
                })
                .collect();

            insert_document(&mut tx, tenant_id, document_id, user_id, doc_idx).await?;
            insert_chunks(&mut tx, tenant_id, document_id, &chunk_data).await?;

            inserted += this_doc_chunks;
        }

        tx.commit().await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------
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
    let has_flag = |flag: &str| -> bool { args.iter().any(|a| a == flag) };

    let total_chunks: usize = get_arg("--chunks", "10000").parse()?;
    let small_tenants: usize = get_arg("--small-tenants", "50").parse()?;
    let chunks_per_small: usize = get_arg("--chunks-per-small", "100").parse()?;
    let seed: u64 = get_arg("--seed", "42").parse()?;
    let reset = has_flag("--reset");

    let url = std::env::var("DATABASE_BENCH_URL")
        .or_else(|_| std::env::var("DATABASE_MIGRATION_URL"))
        .expect("DATABASE_BENCH_URL or DATABASE_MIGRATION_URL is required");

    println!("Connecting to bench database...");
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await?;
    println!("Connected.");

    if reset {
        println!("Resetting tables (TRUNCATE CASCADE)...");
        sqlx::query("TRUNCATE TABLE document_chunks, documents, tenants, users CASCADE")
            .execute(&pool)
            .await?;
        println!("Reset done.");
    }

    let mut rng = Rng::new(seed);

    println!("Generating 64 centroids ({DIM}-dimensional)...");
    let centroids: Vec<Vec<f32>> = (0..64).map(|_| random_unit_vec(&mut rng)).collect();

    let small_chunk_total = small_tenants * chunks_per_small;
    let large_chunks = if total_chunks > small_chunk_total {
        total_chunks - small_chunk_total
    } else {
        total_chunks / 2
    };

    let large_tenant_id = Uuid::from_u128((0xDEAD_BEEF_u128 << 96) | (seed as u128));
    let large_user_id = Uuid::from_u128((0xDEAD_FACE_u128 << 96) | (seed as u128));

    println!(
        "Seeding {} total chunks: {} for large tenant, {} × {} for {} small tenants",
        large_chunks + small_chunk_total,
        large_chunks,
        chunks_per_small,
        small_tenants,
        small_tenants
    );

    // ---- Large tenant ----
    let t0 = std::time::Instant::now();
    seed_tenant(
        &pool,
        large_tenant_id,
        large_user_id,
        0,
        large_chunks,
        &centroids,
        &mut rng,
    )
    .await?;
    let large_secs = t0.elapsed().as_secs_f64();
    println!("Large tenant done: {large_chunks} chunks in {large_secs:.1}s");

    // ---- Small tenants ----
    let t_small = std::time::Instant::now();
    let mut first_small_tenant_id = Uuid::nil();
    let mut first_small_user_id = Uuid::nil();

    for st in 0..small_tenants {
        let small_tenant_id =
            Uuid::from_u128((0xCAFE_BABE_u128 << 96) | (((st as u128) + 1) << 48) | (seed as u128));
        let small_user_id =
            Uuid::from_u128((0xCAFE_FACE_u128 << 96) | (((st as u128) + 1) << 48) | (seed as u128));
        if st == 0 {
            first_small_tenant_id = small_tenant_id;
            first_small_user_id = small_user_id;
        }

        seed_tenant(
            &pool,
            small_tenant_id,
            small_user_id,
            st + 1,
            chunks_per_small,
            &centroids,
            &mut rng,
        )
        .await?;
        if (st + 1) % 10 == 0 || st + 1 == small_tenants {
            println!("  Small tenants: {}/{}", st + 1, small_tenants);
        }
    }
    println!(
        "Small tenants done: {} chunks in {:.1}s",
        small_chunk_total,
        t_small.elapsed().as_secs_f64()
    );

    // ---- Generate Tokens for Benchmarking ----
    let jwt_secret = std::env::var("JWT_SECRET")
        .unwrap_or_else(|_| "changeme-dev-only-must-be-at-least-32-characters-long".into());
    let token_service = TokenService::new(TokenSettings {
        secret: jwt_secret,
        issuer: "tenant-saas".into(),
        audience: "tenant-saas-api".into(),
        expiry_secs: 86400,
    });

    let large_token = token_service.issue_access(large_user_id)?;
    let small_token = token_service.issue_access(first_small_user_id)?;

    println!("\n=== Benchmark Tenant Identifiers ===");
    println!("Large Tenant ID:  {large_tenant_id}");
    println!("Large User ID:    {large_user_id}");
    println!("Large Auth Token: {}", large_token.token);
    println!("Small Tenant ID:  {first_small_tenant_id}");
    println!("Small User ID:    {first_small_user_id}");
    println!("Small Auth Token: {}", small_token.token);

    // ---- Report index sizes ----
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT indexname, pg_relation_size(indexname::regclass)
         FROM pg_indexes
         WHERE tablename = 'document_chunks'
         ORDER BY indexname",
    )
    .fetch_all(&pool)
    .await?;

    println!("\n=== Index sizes after seeding ===");
    for (name, size) in &rows {
        println!(
            "  {name}: {size} bytes ({:.2} MiB)",
            *size as f64 / 1_048_576.0
        );
    }

    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM document_chunks")
        .fetch_one(&pool)
        .await?;
    println!("\nTotal chunks in bench database: {count}");
    println!("Seed complete.");
    Ok(())
}
