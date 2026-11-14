//! Security tests for the Phase 4 ingestion tables (migration `004_ingestion.sql`).
//!
//! Covers `ingestion_jobs`, `document_chunks` and `outbox_events`:
//! 1. Tenant isolation: reads see nothing across tenants, writes are refused or touch 0 rows.
//! 2. Fail-closed behaviour with no tenant context at all.
//! 3. Composite foreign keys, the one-active-job index and the lease CHECK.
//! 4. Least-privilege grants for `app_runtime`.
//! 5. The vector column dimension.
//! 6. The Phase 3 backfill statement.
//! 7. ENABLE-without-FORCE RLS (plan decision D2), which the sweeper depends on.
//!
//! Documents are seeded straight through the owner pool instead of the upload API, so
//! these tests stay valid once the upload transaction starts creating jobs itself.

use serde_json::json;
use sqlx::{PgConnection, Postgres, Transaction};
use test_support::TestApp;
use uuid::Uuid;

const MIGRATION_004: &str = include_str!("../../apps/tenant-saas/migrations/004_ingestion.sql");

const INSERT_CHUNK: &str = "INSERT INTO document_chunks \
        (tenant_id, document_id, chunk_index, content, token_count, \
         embedding, embedding_model, embedding_version) \
     VALUES ($1, $2, $3, 'chunk text', 10, $4::vector, 'fake-model', 'v1')";

const INSERT_JOB: &str = "INSERT INTO ingestion_jobs (tenant_id, document_id, max_attempts) \
     VALUES ($1, $2, 5)";

async fn spawn() -> Option<TestApp> {
    TestApp::spawn().await
}

fn vector_literal(dims: usize) -> String {
    format!("[{}]", vec!["0.0"; dims].join(","))
}

/// A tenant with one `pending` document, inserted directly (no job, no outbox row).
async fn seed_tenant_with_document(app: &TestApp) -> (Uuid, Uuid) {
    let tenant_id = app.create_tenant().await;
    let doc_id = seed_document(app, tenant_id).await;
    (tenant_id, doc_id)
}

async fn seed_document(app: &TestApp, tenant_id: Uuid) -> Uuid {
    let doc_id = Uuid::new_v4();
    let hash = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let mut tx = app.owner_scoped_tx(tenant_id, Uuid::new_v4()).await;
    sqlx::query(
        "INSERT INTO documents \
            (id, tenant_id, title, original_filename, content_hash, object_key, mime_type, size_bytes) \
         VALUES ($1, $2, 'fixture', 'fixture.txt', $3, $4, 'text/plain', 10)",
    )
    .bind(doc_id)
    .bind(tenant_id)
    .bind(hash)
    .bind(format!("tests/{tenant_id}/{doc_id}"))
    .execute(&mut *tx)
    .await
    .expect("seed document");
    tx.commit().await.expect("commit document");
    doc_id
}

/// `app_runtime` transaction scoped to `tenant_id`, exactly like `begin_tenant_tx`.
async fn runtime_tx(app: &TestApp, tenant_id: Uuid) -> Transaction<'static, Postgres> {
    let mut tx = app.runtime_pool.begin().await.expect("begin");
    sqlx::query(
        "SELECT set_config('app.current_tenant_id', $1, true), \
                set_config('app.current_user_id', $2, true)",
    )
    .bind(tenant_id.to_string())
    .bind(Uuid::new_v4().to_string())
    .execute(&mut *tx)
    .await
    .expect("set scope");
    tx
}

async fn seed_job(app: &TestApp, tenant_id: Uuid, doc_id: Uuid) -> Uuid {
    let mut tx = runtime_tx(app, tenant_id).await;
    let id: (Uuid,) = sqlx::query_as(
        "INSERT INTO ingestion_jobs (tenant_id, document_id, max_attempts) \
         VALUES ($1, $2, 5) RETURNING id",
    )
    .bind(tenant_id)
    .bind(doc_id)
    .fetch_one(&mut *tx)
    .await
    .expect("seed job");
    tx.commit().await.expect("commit job");
    id.0
}

async fn seed_chunk(app: &TestApp, tenant_id: Uuid, doc_id: Uuid, index: i32) {
    let mut tx = runtime_tx(app, tenant_id).await;
    insert_chunk(&mut tx, tenant_id, doc_id, index, 1536)
        .await
        .expect("seed chunk");
    tx.commit().await.expect("commit chunk");
}

async fn insert_chunk(
    conn: &mut PgConnection,
    tenant_id: Uuid,
    doc_id: Uuid,
    index: i32,
    dims: usize,
) -> Result<sqlx::postgres::PgQueryResult, sqlx::Error> {
    sqlx::query(INSERT_CHUNK)
        .bind(tenant_id)
        .bind(doc_id)
        .bind(index)
        .bind(vector_literal(dims))
        .execute(conn)
        .await
}

/// The SQLSTATE of a failed statement. Panics if the statement succeeded.
fn sqlstate<T>(result: Result<T, sqlx::Error>, what: &str) -> String {
    match result {
        Ok(_) => panic!("{what}: expected an error but the statement succeeded"),
        Err(sqlx::Error::Database(db)) => db.code().map(|c| c.to_string()).unwrap_or_default(),
        Err(other) => panic!("{what}: expected a database error, got {other}"),
    }
}

fn constraint<T>(result: &Result<T, sqlx::Error>) -> Option<String> {
    match result {
        Err(sqlx::Error::Database(db)) => db.constraint().map(str::to_owned),
        _ => None,
    }
}

const RLS_VIOLATION: &str = "42501";
const FK_VIOLATION: &str = "23503";
const UNIQUE_VIOLATION: &str = "23505";
const CHECK_VIOLATION: &str = "23514";

// ---------------------------------------------------------------------------
// ingestion_jobs
// ---------------------------------------------------------------------------

#[tokio::test]
async fn jobs_are_isolated_between_tenants() {
    let Some(app) = spawn().await else { return };
    let (tenant_a, doc_a) = seed_tenant_with_document(&app).await;
    let (tenant_b, doc_b) = seed_tenant_with_document(&app).await;
    let job_a = seed_job(&app, tenant_a, doc_a).await;

    // Tenant A sees its own job.
    let mut tx = runtime_tx(&app, tenant_a).await;
    let own: Vec<(Uuid,)> = sqlx::query_as("SELECT id FROM ingestion_jobs WHERE id = $1")
        .bind(job_a)
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    assert_eq!(own.len(), 1);
    drop(tx);

    // Tenant B sees nothing.
    let mut tx = runtime_tx(&app, tenant_b).await;
    let foreign: Vec<(Uuid,)> =
        sqlx::query_as("SELECT id FROM ingestion_jobs WHERE tenant_id = $1")
            .bind(tenant_a)
            .fetch_all(&mut *tx)
            .await
            .unwrap();
    assert!(foreign.is_empty(), "tenant B must not see tenant A's jobs");

    // B updating A's job touches 0 rows.
    let updated = sqlx::query("UPDATE ingestion_jobs SET attempts = 3 WHERE id = $1")
        .bind(job_a)
        .execute(&mut *tx)
        .await
        .unwrap();
    assert_eq!(updated.rows_affected(), 0);
    tx.commit().await.unwrap();

    // B inserting a job with tenant_id = A is an RLS violation.
    let mut tx = runtime_tx(&app, tenant_b).await;
    let cross = sqlx::query(INSERT_JOB)
        .bind(tenant_a)
        .bind(doc_a)
        .execute(&mut *tx)
        .await;
    assert_eq!(sqlstate(cross, "cross-tenant job insert"), RLS_VIOLATION);
    drop(tx);

    // Owner view confirms the job was never modified and B has no row of its own.
    let (attempts,): (i32,) = sqlx::query_as("SELECT attempts FROM ingestion_jobs WHERE id = $1")
        .bind(job_a)
        .fetch_one(app.owner())
        .await
        .unwrap();
    assert_eq!(attempts, 0);
    let b_jobs: (i64,) =
        sqlx::query_as("SELECT count(*) FROM ingestion_jobs WHERE document_id = $1")
            .bind(doc_b)
            .fetch_one(app.owner())
            .await
            .unwrap();
    assert_eq!(b_jobs.0, 0);
}

#[tokio::test]
async fn jobs_fail_closed_without_tenant_context() {
    let Some(app) = spawn().await else { return };
    let (tenant_a, doc_a) = seed_tenant_with_document(&app).await;
    let job_a = seed_job(&app, tenant_a, doc_a).await;

    let rows: Vec<(Uuid,)> = sqlx::query_as("SELECT id FROM ingestion_jobs WHERE id = $1")
        .bind(job_a)
        .fetch_all(&app.runtime_pool)
        .await
        .unwrap();
    assert!(rows.is_empty(), "no context must read 0 rows");

    let update = sqlx::query("UPDATE ingestion_jobs SET attempts = 3 WHERE id = $1")
        .bind(job_a)
        .execute(&app.runtime_pool)
        .await
        .unwrap();
    assert_eq!(update.rows_affected(), 0);

    let insert = sqlx::query(INSERT_JOB)
        .bind(tenant_a)
        .bind(doc_a)
        .execute(&app.runtime_pool)
        .await;
    assert_eq!(
        sqlstate(insert, "job insert without context"),
        RLS_VIOLATION
    );
}

#[tokio::test]
async fn job_with_unknown_tenant_document_pair_violates_composite_fk() {
    let Some(app) = spawn().await else { return };
    let (tenant_a, _doc_a) = seed_tenant_with_document(&app).await;
    let (_tenant_b, doc_b) = seed_tenant_with_document(&app).await;

    // Passes RLS (tenant_id = A) but (A, doc_b) is not a documents row.
    let mut tx = runtime_tx(&app, tenant_a).await;
    let result = sqlx::query(INSERT_JOB)
        .bind(tenant_a)
        .bind(doc_b)
        .execute(&mut *tx)
        .await;
    assert_eq!(
        constraint(&result).as_deref(),
        Some("ingestion_jobs_document_fk")
    );
    assert_eq!(sqlstate(result, "mismatched job"), FK_VIOLATION);
}

#[tokio::test]
async fn second_active_job_for_a_document_is_rejected() {
    let Some(app) = spawn().await else { return };
    let (tenant, doc) = seed_tenant_with_document(&app).await;
    let first = seed_job(&app, tenant, doc).await;

    // Another pending job: unique violation on the partial index.
    let mut tx = runtime_tx(&app, tenant).await;
    let dup = sqlx::query(INSERT_JOB)
        .bind(tenant)
        .bind(doc)
        .execute(&mut *tx)
        .await;
    assert_eq!(
        constraint(&dup).as_deref(),
        Some("ingestion_jobs_one_active")
    );
    assert_eq!(sqlstate(dup, "duplicate pending job"), UNIQUE_VIOLATION);
    drop(tx);

    // retry_scheduled still counts as active.
    let mut tx = runtime_tx(&app, tenant).await;
    sqlx::query("UPDATE ingestion_jobs SET status = 'retry_scheduled' WHERE id = $1")
        .bind(first)
        .execute(&mut *tx)
        .await
        .unwrap();
    let dup = sqlx::query(INSERT_JOB)
        .bind(tenant)
        .bind(doc)
        .execute(&mut *tx)
        .await;
    assert_eq!(
        sqlstate(dup, "job beside retry_scheduled"),
        UNIQUE_VIOLATION
    );
    drop(tx);

    // Once the first job is terminal, a new active job is allowed (manual retry).
    let mut tx = runtime_tx(&app, tenant).await;
    sqlx::query("UPDATE ingestion_jobs SET status = 'failed', completed_at = now() WHERE id = $1")
        .bind(first)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(INSERT_JOB)
        .bind(tenant)
        .bind(doc)
        .execute(&mut *tx)
        .await
        .expect("new job after a terminal one");
    tx.commit().await.unwrap();

    // The same document in another tenant is unaffected: the index is per (tenant, document).
    let (tenant_b, doc_b) = seed_tenant_with_document(&app).await;
    seed_job(&app, tenant_b, doc_b).await;
}

#[tokio::test]
async fn lease_fields_must_match_processing_status() {
    let Some(app) = spawn().await else { return };
    let (tenant, doc) = seed_tenant_with_document(&app).await;

    // processing with no lease
    let mut tx = runtime_tx(&app, tenant).await;
    let result = sqlx::query(
        "INSERT INTO ingestion_jobs (tenant_id, document_id, status, max_attempts) \
         VALUES ($1, $2, 'processing', 5)",
    )
    .bind(tenant)
    .bind(doc)
    .execute(&mut *tx)
    .await;
    assert_eq!(
        constraint(&result).as_deref(),
        Some("ingestion_jobs_lease_consistency")
    );
    assert_eq!(
        sqlstate(result, "processing without lease"),
        CHECK_VIOLATION
    );
    drop(tx);

    // pending with a lease
    let mut tx = runtime_tx(&app, tenant).await;
    let result = sqlx::query(
        "INSERT INTO ingestion_jobs \
            (tenant_id, document_id, status, max_attempts, lease_token, locked_until) \
         VALUES ($1, $2, 'pending', 5, gen_random_uuid(), now() + interval '1 minute')",
    )
    .bind(tenant)
    .bind(doc)
    .execute(&mut *tx)
    .await;
    assert_eq!(sqlstate(result, "pending with lease"), CHECK_VIOLATION);
    drop(tx);

    // processing with a lease is accepted
    let mut tx = runtime_tx(&app, tenant).await;
    sqlx::query(
        "INSERT INTO ingestion_jobs \
            (tenant_id, document_id, status, max_attempts, lease_token, locked_by, locked_until) \
         VALUES ($1, $2, 'processing', 5, gen_random_uuid(), 'worker-1', now() + interval '1 minute')",
    )
    .bind(tenant)
    .bind(doc)
    .execute(&mut *tx)
    .await
    .expect("processing job with a lease");
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn job_value_ranges_are_checked() {
    let Some(app) = spawn().await else { return };
    let (tenant, doc) = seed_tenant_with_document(&app).await;

    for (label, max_attempts, error_code) in [
        ("max_attempts 0", 0, None),
        ("max_attempts 101", 101, None),
        ("bad error code", 5, Some("lowercase")),
    ] {
        let mut tx = runtime_tx(&app, tenant).await;
        let result = sqlx::query(
            "INSERT INTO ingestion_jobs (tenant_id, document_id, max_attempts, last_error_code) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(tenant)
        .bind(doc)
        .bind(max_attempts)
        .bind(error_code)
        .execute(&mut *tx)
        .await;
        assert_eq!(sqlstate(result, label), CHECK_VIOLATION, "{label}");
    }
}

#[tokio::test]
async fn app_runtime_cannot_delete_jobs_or_update_chunks() {
    let Some(app) = spawn().await else { return };
    let (tenant, doc) = seed_tenant_with_document(&app).await;
    seed_job(&app, tenant, doc).await;
    seed_chunk(&app, tenant, doc, 0).await;

    let mut tx = runtime_tx(&app, tenant).await;
    let result = sqlx::query("DELETE FROM ingestion_jobs WHERE tenant_id = $1")
        .bind(tenant)
        .execute(&mut *tx)
        .await;
    assert_eq!(sqlstate(result, "DELETE ingestion_jobs"), RLS_VIOLATION);
    drop(tx);

    let mut tx = runtime_tx(&app, tenant).await;
    let result = sqlx::query("UPDATE document_chunks SET content = 'x' WHERE tenant_id = $1")
        .bind(tenant)
        .execute(&mut *tx)
        .await;
    assert_eq!(sqlstate(result, "UPDATE document_chunks"), RLS_VIOLATION);
}

// ---------------------------------------------------------------------------
// document_chunks
// ---------------------------------------------------------------------------

#[tokio::test]
async fn chunks_are_isolated_between_tenants() {
    let Some(app) = spawn().await else { return };
    let (tenant_a, doc_a) = seed_tenant_with_document(&app).await;
    let (tenant_b, _doc_b) = seed_tenant_with_document(&app).await;
    seed_chunk(&app, tenant_a, doc_a, 0).await;

    // A sees its chunk; B sees nothing.
    let mut tx = runtime_tx(&app, tenant_a).await;
    let own: (i64,) = sqlx::query_as("SELECT count(*) FROM document_chunks WHERE document_id = $1")
        .bind(doc_a)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(own.0, 1);
    drop(tx);

    let mut tx = runtime_tx(&app, tenant_b).await;
    let foreign: (i64,) =
        sqlx::query_as("SELECT count(*) FROM document_chunks WHERE document_id = $1")
            .bind(doc_a)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    assert_eq!(foreign.0, 0, "tenant B must not see tenant A's chunks");

    // B deleting A's chunks touches 0 rows.
    let deleted = sqlx::query("DELETE FROM document_chunks WHERE document_id = $1")
        .bind(doc_a)
        .execute(&mut *tx)
        .await
        .unwrap();
    assert_eq!(deleted.rows_affected(), 0);
    tx.commit().await.unwrap();

    // B inserting a chunk with tenant_id = A is an RLS violation.
    let mut tx = runtime_tx(&app, tenant_b).await;
    let cross = insert_chunk(&mut tx, tenant_a, doc_a, 1, 1536).await;
    assert_eq!(sqlstate(cross, "cross-tenant chunk insert"), RLS_VIOLATION);
    drop(tx);

    // A's chunk survived all of that.
    let (count,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM document_chunks WHERE document_id = $1")
            .bind(doc_a)
            .fetch_one(app.owner())
            .await
            .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn chunks_fail_closed_without_tenant_context() {
    let Some(app) = spawn().await else { return };
    let (tenant, doc) = seed_tenant_with_document(&app).await;
    seed_chunk(&app, tenant, doc, 0).await;

    let rows: (i64,) =
        sqlx::query_as("SELECT count(*) FROM document_chunks WHERE document_id = $1")
            .bind(doc)
            .fetch_one(&app.runtime_pool)
            .await
            .unwrap();
    assert_eq!(rows.0, 0);

    let deleted = sqlx::query("DELETE FROM document_chunks WHERE document_id = $1")
        .bind(doc)
        .execute(&app.runtime_pool)
        .await
        .unwrap();
    assert_eq!(deleted.rows_affected(), 0);

    let mut conn = app.runtime_pool.acquire().await.unwrap();
    let insert = insert_chunk(&mut conn, tenant, doc, 1, 1536).await;
    assert_eq!(
        sqlstate(insert, "chunk insert without context"),
        RLS_VIOLATION
    );
}

#[tokio::test]
async fn chunk_with_unknown_tenant_document_pair_violates_composite_fk() {
    let Some(app) = spawn().await else { return };
    let (tenant_a, _doc_a) = seed_tenant_with_document(&app).await;
    let (_tenant_b, doc_b) = seed_tenant_with_document(&app).await;

    let mut tx = runtime_tx(&app, tenant_a).await;
    let result = insert_chunk(&mut tx, tenant_a, doc_b, 0, 1536).await;
    assert_eq!(
        constraint(&result).as_deref(),
        Some("document_chunks_document_fk")
    );
    assert_eq!(sqlstate(result, "mismatched chunk"), FK_VIOLATION);
}

#[tokio::test]
async fn chunk_position_is_unique_per_document() {
    let Some(app) = spawn().await else { return };
    let (tenant, doc) = seed_tenant_with_document(&app).await;
    seed_chunk(&app, tenant, doc, 0).await;

    let mut tx = runtime_tx(&app, tenant).await;
    let dup = insert_chunk(&mut tx, tenant, doc, 0, 1536).await;
    assert_eq!(
        constraint(&dup).as_deref(),
        Some("document_chunks_position_unique")
    );
    assert_eq!(sqlstate(dup, "duplicate chunk_index"), UNIQUE_VIOLATION);
    drop(tx);

    // Delete + insert in one transaction is the replace path and must work.
    let mut tx = runtime_tx(&app, tenant).await;
    sqlx::query("DELETE FROM document_chunks WHERE document_id = $1")
        .bind(doc)
        .execute(&mut *tx)
        .await
        .unwrap();
    insert_chunk(&mut tx, tenant, doc, 0, 1536)
        .await
        .expect("re-insert index 0");
    insert_chunk(&mut tx, tenant, doc, 1, 1536)
        .await
        .expect("insert index 1");
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn chunk_embedding_must_have_1536_dimensions() {
    let Some(app) = spawn().await else { return };
    let (tenant, doc) = seed_tenant_with_document(&app).await;

    let mut tx = runtime_tx(&app, tenant).await;
    let result = insert_chunk(&mut tx, tenant, doc, 0, 768).await;
    match result {
        Err(sqlx::Error::Database(db)) => assert!(
            db.message().contains("expected 1536 dimensions"),
            "unexpected message: {}",
            db.message()
        ),
        other => panic!("expected a dimension error, got {other:?}"),
    }
    drop(tx);

    // Bulk-insert style: parallel arrays through UNNEST, as the worker will do (plan D8).
    let mut tx = runtime_tx(&app, tenant).await;
    let result = sqlx::query(
        "INSERT INTO document_chunks \
            (tenant_id, document_id, chunk_index, content, token_count, \
             embedding, embedding_model, embedding_version, metadata) \
         SELECT $1, $2, t.idx, t.content, t.tokens, t.emb::vector, 'fake-model', 'v1', t.meta \
           FROM UNNEST($3::int[], $4::text[], $5::int[], $6::text[], $7::jsonb[]) \
                AS t(idx, content, tokens, emb, meta)",
    )
    .bind(tenant)
    .bind(doc)
    .bind(vec![0, 1])
    .bind(vec!["a".to_string(), "b".to_string()])
    .bind(vec![3, 4])
    .bind(vec![vector_literal(1536), vector_literal(1536)])
    .bind(vec![json!({}), json!({"page": 2})])
    .execute(&mut *tx)
    .await
    .expect("UNNEST bulk insert");
    assert_eq!(result.rows_affected(), 2);
    let (dims,): (i32,) = sqlx::query_as(
        "SELECT vector_dims(embedding) FROM document_chunks WHERE document_id = $1 LIMIT 1",
    )
    .bind(doc)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert_eq!(dims, 1536);
}

#[tokio::test]
async fn deleting_a_document_row_cascades_to_jobs_and_chunks() {
    let Some(app) = spawn().await else { return };
    let (tenant, doc) = seed_tenant_with_document(&app).await;
    seed_job(&app, tenant, doc).await;
    seed_chunk(&app, tenant, doc, 0).await;

    sqlx::query("DELETE FROM documents WHERE id = $1")
        .bind(doc)
        .execute(app.owner())
        .await
        .expect("owner hard delete");

    let (jobs,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM ingestion_jobs WHERE document_id = $1")
            .bind(doc)
            .fetch_one(app.owner())
            .await
            .unwrap();
    assert_eq!(jobs, 0, "jobs must cascade");
    let (chunks,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM document_chunks WHERE document_id = $1")
            .bind(doc)
            .fetch_one(app.owner())
            .await
            .unwrap();
    assert_eq!(chunks, 0, "chunks must cascade");
}

// ---------------------------------------------------------------------------
// outbox_events
// ---------------------------------------------------------------------------

#[tokio::test]
async fn outbox_is_unscoped_and_purgeable_by_app_runtime() {
    let Some(app) = spawn().await else { return };
    let (tenant_a, _) = seed_tenant_with_document(&app).await;
    let (tenant_b, _) = seed_tenant_with_document(&app).await;
    let (event_a, event_b) = (Uuid::new_v4(), Uuid::new_v4());

    // Written while scoped to A, for both tenants: there is no RLS barrier.
    let mut tx = runtime_tx(&app, tenant_a).await;
    for (id, tenant) in [(event_a, tenant_a), (event_b, tenant_b)] {
        sqlx::query(
            "INSERT INTO outbox_events (id, tenant_id, aggregate_type, aggregate_id, event_type, payload) \
             VALUES ($1, $2, 'ingestion_job', $3, 'ingestion.requested', $4)",
        )
        .bind(id)
        .bind(tenant)
        .bind(Uuid::new_v4())
        .bind(json!({"v": 1}))
        .execute(&mut *tx)
        .await
        .expect("insert outbox event");
    }
    tx.commit().await.unwrap();

    // The publisher has no tenant context and still sees and claims both rows.
    let claimed = sqlx::query(
        "UPDATE outbox_events SET claimed_until = now() + interval '30 seconds', \
                publish_attempts = publish_attempts + 1 \
          WHERE id IN ($1, $2) AND published_at IS NULL",
    )
    .bind(event_a)
    .bind(event_b)
    .execute(&app.runtime_pool)
    .await
    .unwrap();
    assert_eq!(claimed.rows_affected(), 2);

    // Retention purge: DELETE is granted.
    let purged = sqlx::query("DELETE FROM outbox_events WHERE id IN ($1, $2)")
        .bind(event_a)
        .bind(event_b)
        .execute(&app.runtime_pool)
        .await
        .unwrap();
    assert_eq!(purged.rows_affected(), 2);
}

#[tokio::test]
async fn outbox_payload_must_be_a_json_object() {
    let Some(app) = spawn().await else { return };
    let result = sqlx::query(
        "INSERT INTO outbox_events (aggregate_type, aggregate_id, event_type, payload) \
         VALUES ('ingestion_job', $1, 'ingestion.requested', '[1]'::jsonb)",
    )
    .bind(Uuid::new_v4())
    .execute(&app.runtime_pool)
    .await;
    assert_eq!(sqlstate(result, "array payload"), CHECK_VIOLATION);
}

// ---------------------------------------------------------------------------
// Row security configuration and the owner exemption (plan D2)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rls_is_enabled_but_not_forced_on_tenant_tables() {
    let Some(app) = spawn().await else { return };
    let rows: Vec<(String, bool, bool)> = sqlx::query_as(
        "SELECT relname::text, relrowsecurity, relforcerowsecurity FROM pg_class \
          WHERE relname IN ('ingestion_jobs', 'document_chunks', 'outbox_events') \
            AND relnamespace = 'public'::regnamespace ORDER BY relname",
    )
    .fetch_all(app.owner())
    .await
    .unwrap();
    assert_eq!(
        rows,
        vec![
            ("document_chunks".to_string(), true, false),
            ("ingestion_jobs".to_string(), true, false),
            ("outbox_events".to_string(), false, false),
        ],
        "(name, rls enabled, rls forced); FORCE would blind the sweeper (D2)"
    );
}

#[tokio::test]
async fn owner_sees_every_tenants_jobs_without_context() {
    let Some(app) = spawn().await else { return };
    let (tenant_a, doc_a) = seed_tenant_with_document(&app).await;
    let (tenant_b, doc_b) = seed_tenant_with_document(&app).await;
    let job_a = seed_job(&app, tenant_a, doc_a).await;
    let job_b = seed_job(&app, tenant_b, doc_b).await;

    let (count,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM ingestion_jobs WHERE id IN ($1, $2)")
            .bind(job_a)
            .bind(job_b)
            .fetch_one(app.owner())
            .await
            .unwrap();
    assert_eq!(
        count, 2,
        "the table owner is exempt, which the sweeper relies on"
    );
}

// ---------------------------------------------------------------------------
// Backfill
// ---------------------------------------------------------------------------

#[tokio::test]
async fn backfill_gives_a_pending_document_one_job_and_one_event() {
    let Some(app) = spawn().await else { return };
    let (tenant, doc) = seed_tenant_with_document(&app).await;
    let deleted_doc = seed_document(&app, tenant).await;
    sqlx::query("UPDATE documents SET status = 'deleted', deleted_at = now() WHERE id = $1")
        .bind(deleted_doc)
        .execute(app.owner())
        .await
        .unwrap();

    // Run the real statement from the migration, narrowed to this tenant so that
    // parallel tests' documents are left alone.
    let start = MIGRATION_004
        .find("WITH new_jobs AS")
        .expect("backfill statement in 004");
    let filter = "WHERE d.status = 'pending' AND d.deleted_at IS NULL";
    let backfill =
        MIGRATION_004[start..].replace(filter, &format!("{filter} AND d.tenant_id = '{tenant}'"));
    assert!(
        backfill.contains(&tenant.to_string()),
        "filter was not applied"
    );

    // Test-only: the SQL comes from a compiled-in migration plus a UUID.
    sqlx::query(sqlx::AssertSqlSafe(backfill))
        .execute(app.owner())
        .await
        .expect("backfill statement");

    let jobs: Vec<(Uuid, String, i32, Uuid)> = sqlx::query_as(
        "SELECT id, status, max_attempts, document_id FROM ingestion_jobs WHERE tenant_id = $1",
    )
    .bind(tenant)
    .fetch_all(app.owner())
    .await
    .unwrap();
    assert_eq!(jobs.len(), 1, "deleted documents get no job");
    let (job_id, status, max_attempts, job_doc) = &jobs[0];
    assert_eq!(
        (status.as_str(), *max_attempts, *job_doc),
        ("pending", 5, doc)
    );

    let events: Vec<(String, String, serde_json::Value)> = sqlx::query_as(
        "SELECT aggregate_type, event_type, payload FROM outbox_events WHERE aggregate_id = $1",
    )
    .bind(job_id)
    .fetch_all(app.owner())
    .await
    .unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].0, "ingestion_job");
    assert_eq!(events[0].1, "ingestion.requested");
    assert_eq!(
        events[0].2,
        json!({
            "v": 1,
            "job_id": job_id.to_string(),
            "tenant_id": tenant.to_string(),
            "document_id": doc.to_string()
        })
    );

    // Clean up the event so parallel publisher tests are not handed an extra row.
    sqlx::query("DELETE FROM outbox_events WHERE aggregate_id = $1")
        .bind(job_id)
        .execute(app.owner())
        .await
        .unwrap();
}
