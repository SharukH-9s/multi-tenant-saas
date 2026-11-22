//! Search queries over `document_chunks` (Phase 5, ADR-008).
//!
//! Every function runs on a transaction from `begin_tenant_tx`, so RLS applies,
//! and also binds `tenant_id` explicitly (L3). The four statements are:
//! settings, vector candidates, lexical candidates, hydrate.
//!
//! # Dynamic table routing (Task 11)
//!
//! All queries that touch `document_chunks` accept a `chunk_table: &str`
//! argument — the fully-qualified identifier produced by `ChunkTarget::table()`:
//! either `"public.document_chunks"` (shared tenants, the default) or
//! `"tenant_<id>.document_chunks"` (promoted tenants).
//!
//! The value is validated by `ValidatedSchema` at `ChunkTarget` construction time
//! (a regex check + DB-column origin), so it is safe to interpolate into SQL
//! query strings here without further escaping (see §3 of the redesign analysis).
//!
//! `hydrate` always joins `public.documents` explicitly, because the `documents`
//! table is never moved to a dedicated schema — the FK in
//! `tenant_<id>.document_chunks` points back to `public.documents`.

use common_config::SearchConfig;
use common_database::Tx;
use common_errors::AppError;
use uuid::Uuid;

use super::{chunk_repo::vector_literal, db_error::map_db_error};
use crate::domain::search::Candidate;

/// A hydrated search result row. `content` is the full chunk text, kept for
/// Phase 6; the HTTP layer exposes only `snippet` (S10).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct HitRow {
    pub id: Uuid,
    pub document_id: Uuid,
    pub chunk_index: i32,
    pub content: String,
    pub metadata: serde_json::Value,
    pub embedding_version: String,
    pub title: String,
    pub snippet: String,
}

/// Sets `hnsw.ef_search`, `hnsw.iterative_scan` and `statement_timeout` for
/// this transaction only (S7). The valid iterative-scan values are `off`,
/// `strict_order` and `relaxed_order`; `relaxed` is silently ignored (F1).
pub async fn apply_settings(tx: &mut Tx, cfg: &SearchConfig) -> Result<(), AppError> {
    sqlx::query(
        "SELECT set_config('hnsw.ef_search', $1, true), \
                set_config('hnsw.iterative_scan', 'relaxed_order', true), \
                set_config('statement_timeout', $2, true)",
    )
    .bind(cfg.ef_search.to_string())
    .bind(cfg.statement_timeout_ms.to_string())
    .execute(&mut **tx)
    .await
    .map_err(map_db_error)?;
    Ok(())
}

/// Nearest chunks by cosine distance. The CTE is MATERIALIZED and re-sorted so
/// the order is deterministic when distances tie (F2).
///
/// `chunk_table` must be the value of `ChunkTarget::table()` — validated at
/// construction, safe to interpolate into SQL.
pub async fn vector_candidates(
    tx: &mut Tx,
    chunk_table: &str,
    tenant_id: Uuid,
    query_vec: &[f32],
    doc_filter: Option<&[Uuid]>,
    n: i64,
) -> Result<Vec<Candidate>, AppError> {
    let sql = format!(
        "WITH nearest AS MATERIALIZED ( \
             SELECT id, embedding <=> $2::vector AS distance \
               FROM {chunk_table} \
              WHERE tenant_id = $1 \
                AND ($3::uuid[] IS NULL OR document_id = ANY($3)) \
              ORDER BY embedding <=> $2::vector \
              LIMIT $4 \
         ) \
         SELECT id FROM nearest ORDER BY distance, id"
    );
    let rows: Vec<(Uuid,)> = sqlx::query_as(sqlx::AssertSqlSafe(sql.as_str()))
        .bind(tenant_id)
        .bind(vector_literal(query_vec))
        .bind(doc_filter)
        .bind(n)
        .fetch_all(&mut **tx)
        .await
        .map_err(map_db_error)?;
    Ok(to_candidates(rows))
}

/// Full-text matches ranked by `ts_rank_cd` (not BM25, L6). A query that
/// reduces to nothing, such as only stop words, returns no rows (F6).
///
/// `chunk_table` must be the value of `ChunkTarget::table()` — validated at
/// construction, safe to interpolate into SQL.
pub async fn lexical_candidates(
    tx: &mut Tx,
    chunk_table: &str,
    tenant_id: Uuid,
    query: &str,
    doc_filter: Option<&[Uuid]>,
    n: i64,
) -> Result<Vec<Candidate>, AppError> {
    let sql = format!(
        "SELECT c.id \
           FROM {chunk_table} c, websearch_to_tsquery('english', $2) AS q \
          WHERE c.tenant_id = $1 \
            AND ($3::uuid[] IS NULL OR c.document_id = ANY($3)) \
            AND c.tsv_content @@ q \
          ORDER BY ts_rank_cd(c.tsv_content, q) DESC, c.id \
          LIMIT $4"
    );
    let rows: Vec<(Uuid,)> = sqlx::query_as(sqlx::AssertSqlSafe(sql.as_str()))
        .bind(tenant_id)
        .bind(query)
        .bind(doc_filter)
        .bind(n)
        .fetch_all(&mut **tx)
        .await
        .map_err(map_db_error)?;
    Ok(to_candidates(rows))
}

/// Loads the rows for the fused chunk ids. Chunks of soft-deleted documents
/// are dropped here (S4), as are chunks deleted since the candidate queries.
/// Row order is not significant: the caller restores RRF order.
///
/// `chunk_table` is the fully-qualified chunk table identifier from
/// `ChunkTarget::table()`. `documents` is **always** qualified as
/// `public.documents` — the `documents` table is never promoted to a dedicated
/// schema; the FK in `tenant_<id>.document_chunks` still points there.
pub async fn hydrate(
    tx: &mut Tx,
    chunk_table: &str,
    tenant_id: Uuid,
    chunk_ids: &[Uuid],
    query: &str,
) -> Result<Vec<HitRow>, AppError> {
    let sql = format!(
        "SELECT c.id, c.document_id, c.chunk_index, c.content, c.metadata, \
                c.embedding_version, d.title, \
                ts_headline('english', c.content, websearch_to_tsquery('english', $3), \
                    'StartSel=\"\", StopSel=\"\", MaxFragments=2, MinWords=10, MaxWords=35, \
                     FragmentDelimiter=\" \u{2026} \"') AS snippet \
           FROM {chunk_table} c \
           JOIN public.documents d ON d.tenant_id = c.tenant_id AND d.id = c.document_id \
          WHERE c.tenant_id = $1 AND c.id = ANY($2) AND d.deleted_at IS NULL"
    );
    sqlx::query_as::<_, HitRow>(sqlx::AssertSqlSafe(sql.as_str()))
        .bind(tenant_id)
        .bind(chunk_ids)
        .bind(query)
        .fetch_all(&mut **tx)
        .await
        .map_err(map_db_error)
}

fn to_candidates(rows: Vec<(Uuid,)>) -> Vec<Candidate> {
    rows.into_iter()
        .enumerate()
        .map(|(i, (chunk_id,))| Candidate {
            chunk_id,
            rank: i + 1,
        })
        .collect()
}
