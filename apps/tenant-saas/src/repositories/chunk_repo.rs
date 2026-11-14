//! `document_chunks` access. Rows are immutable: a re-ingest deletes the
//! document's chunks and inserts the new set in one transaction.

use common_database::Tx;
use common_errors::AppError;
use uuid::Uuid;

use super::db_error::map_db_error;

pub struct NewChunk<'a> {
    pub index: i32,
    pub content: &'a str,
    pub token_count: i32,
    pub embedding: &'a [f32],
    pub metadata: serde_json::Value,
}

/// Formats a float slice as a Postgres vector literal `"[0.1,0.2,...]"`.
pub fn vector_literal(v: &[f32]) -> String {
    let mut s = String::with_capacity(v.len() * 10 + 2);
    s.push('[');
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        use std::fmt::Write;
        write!(s, "{x}").unwrap();
    }
    s.push(']');
    s
}

/// Deletes every chunk of a document and returns how many were removed.
/// RLS limits the delete to the caller's tenant.
pub async fn delete_for_document(tx: &mut Tx, document_id: Uuid) -> Result<u64, AppError> {
    let result = sqlx::query("DELETE FROM document_chunks WHERE document_id = $1")
        .bind(document_id)
        .execute(&mut **tx)
        .await
        .map_err(map_db_error)?;
    Ok(result.rows_affected())
}

/// Returns the total chunk count for a document.
pub async fn count_for_document(tx: &mut Tx, document_id: Uuid) -> Result<i64, AppError> {
    let (count,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM document_chunks WHERE document_id = $1")
            .bind(document_id)
            .fetch_one(&mut **tx)
            .await
            .map_err(map_db_error)?;
    Ok(count)
}

/// Deletes all chunks of the document, then inserts `chunks` in UNNEST batches of 200 (D8).
pub async fn replace_for_document(
    tx: &mut Tx,
    tenant_id: Uuid,
    document_id: Uuid,
    model: &str,
    version: &str,
    chunks: &[NewChunk<'_>],
) -> Result<(), AppError> {
    delete_for_document(tx, document_id).await?;
    if chunks.is_empty() {
        return Ok(());
    }

    for batch in chunks.chunks(200) {
        let indices: Vec<i32> = batch.iter().map(|c| c.index).collect();
        let contents: Vec<&str> = batch.iter().map(|c| c.content).collect();
        let token_counts: Vec<i32> = batch.iter().map(|c| c.token_count).collect();
        let embeddings: Vec<String> = batch.iter().map(|c| vector_literal(c.embedding)).collect();
        let metadatas: Vec<String> = batch.iter().map(|c| c.metadata.to_string()).collect();

        sqlx::query(
            "INSERT INTO document_chunks \
                (tenant_id, document_id, chunk_index, content, token_count, \
                 embedding, embedding_model, embedding_version, metadata) \
             SELECT $1, $2, t.idx, t.content, t.tokens, t.emb::vector, $3, $4, t.meta::jsonb \
               FROM UNNEST($5::int[], $6::text[], $7::int[], $8::text[], $9::text[]) \
                    AS t(idx, content, tokens, emb, meta)",
        )
        .bind(tenant_id)
        .bind(document_id)
        .bind(model)
        .bind(version)
        .bind(&indices)
        .bind(&contents)
        .bind(&token_counts)
        .bind(&embeddings)
        .bind(&metadatas)
        .execute(&mut **tx)
        .await
        .map_err(map_db_error)?;
    }

    Ok(())
}
