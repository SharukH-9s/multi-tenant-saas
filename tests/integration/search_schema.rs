//! Phase 5 T1: the full-text column and the two search indexes (migration 006).

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
    let defs: Vec<(String, String)> = sqlx::query_as(
        "SELECT indexname, indexdef FROM pg_indexes \
          WHERE tablename = 'document_chunks' \
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
