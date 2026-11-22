//! Phase 5 T1 & T9/T10: search schema, indexes, and dedicated schema migration invariants.

use test_support::TestApp;
use uuid::Uuid;

use crate::ingestion_support::{fixture, SIMPLE_TXT};

#[tokio::test]
async fn tsv_content_is_generated_and_read_only() {
    let Some(f) = fixture("simple.txt", SIMPLE_TXT).await else {
        return;
    };
    let owner = f.app.owner();

    let (hit,): (bool,) = sqlx::query_as(
        "WITH c AS ( \
           INSERT INTO document_chunks \
             (tenant_id, document_id, chunk_index, content, token_count, \
              embedding, embedding_model, embedding_version) \
           VALUES ($1, $2, 0, 'Invoices are payable within thirty days', 7, array_fill(0, ARRAY[1536])::vector, 'fake', 't1') \
           RETURNING tsv_content) \
         SELECT tsv_content @@ websearch_to_tsquery('english', 'invoice') FROM c",
    )
    .bind(f.tenant_id)
    .bind(f.document_id)
    .fetch_one(owner)
    .await
    .unwrap();
    assert!(hit, "tsv_content must be populated from content");

    let err = sqlx::query(
        "INSERT INTO document_chunks \
           (tenant_id, document_id, chunk_index, content, token_count, \
            embedding, embedding_model, embedding_version, tsv_content) \
         VALUES ($1, $2, 1, 'x', 1, array_fill(0, ARRAY[1536])::vector, 'fake', 't1', to_tsvector('x'))",
    )
    .bind(f.tenant_id)
    .bind(f.document_id)
    .execute(owner)
    .await
    .unwrap_err();
    assert!(
        err.to_string().contains("non-DEFAULT value into column"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn search_indexes_exist_with_expected_definitions() {
    let Some(f) = fixture("simple.txt", SIMPLE_TXT).await else {
        return;
    };
    // Must restrict to schemaname = 'public' so parallel dedicated-schema tests
    // with identically named indexes in other schemas do not conflict.
    let defs: Vec<(String, String)> = sqlx::query_as(
        "SELECT indexname, indexdef FROM pg_indexes \
          WHERE schemaname = 'public' \
            AND tablename = 'document_chunks' \
            AND indexname IN ('document_chunks_tsv_idx', 'document_chunks_embedding_hnsw_idx')",
    )
    .fetch_all(f.app.owner())
    .await
    .unwrap();
    assert_eq!(defs.len(), 2, "{defs:?}");
    for (name, def) in defs {
        if name == "document_chunks_tsv_idx" {
            assert!(def.contains("USING gin (tsv_content)"), "{def}");
        } else {
            assert!(def.contains("USING hnsw"), "{def}");
            assert!(def.contains("vector_cosine_ops"), "{def}");
        }
    }
}

#[tokio::test]
async fn dedicated_schema_tightened_constraint_and_promotion_columns() {
    let Some(app) = TestApp::spawn().await else {
        return;
    };
    let owner = app.owner();

    // 1. Verify partial index tenants_dedicated_schema_idx exists
    let (idx_count,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM pg_indexes \
          WHERE schemaname = 'public' \
            AND tablename = 'tenants' \
            AND indexname = 'tenants_dedicated_schema_idx'",
    )
    .fetch_one(owner)
    .await
    .unwrap();
    assert_eq!(idx_count, 1, "tenants_dedicated_schema_idx must exist");

    // 2. Verify promotion_fenced defaults to false and dedicated_schema defaults to NULL
    let tenant_id = app.create_tenant().await;
    let (fenced, schema): (bool, Option<String>) = {
        let mut tx = app.owner_scoped_tx(tenant_id, Uuid::nil()).await;
        sqlx::query_as("SELECT promotion_fenced, dedicated_schema FROM tenants WHERE id = $1")
            .bind(tenant_id)
            .fetch_one(&mut *tx)
            .await
            .unwrap()
    };
    assert!(!fenced, "promotion_fenced must default to false");
    assert!(schema.is_none(), "dedicated_schema must default to NULL");

    // 3. Verify tightened CHECK constraint accepts up to 56 suffix chars (63 bytes total)
    let valid_schema = format!("tenant_{}", "a".repeat(56));
    let mut tx = app.owner_scoped_tx(tenant_id, Uuid::nil()).await;
    let res = sqlx::query("UPDATE tenants SET dedicated_schema = $2 WHERE id = $1")
        .bind(tenant_id)
        .bind(&valid_schema)
        .execute(&mut *tx)
        .await;
    assert!(res.is_ok(), "56-char suffix must be accepted: {res:?}");
    tx.rollback().await.unwrap();

    // 4. Verify tightened CHECK constraint rejects 57 suffix chars (64 bytes total)
    let invalid_schema = format!("tenant_{}", "a".repeat(57));
    let mut tx = app.owner_scoped_tx(tenant_id, Uuid::nil()).await;
    let err = sqlx::query("UPDATE tenants SET dedicated_schema = $2 WHERE id = $1")
        .bind(tenant_id)
        .bind(&invalid_schema)
        .execute(&mut *tx)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("tenants_dedicated_schema_format")
            || err.to_string().contains("violates check constraint"),
        "57-char suffix must violate CHECK constraint: {err}"
    );
    tx.rollback().await.unwrap();
}
