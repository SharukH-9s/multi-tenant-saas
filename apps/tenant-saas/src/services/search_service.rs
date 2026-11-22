//! Hybrid search: embed the query, then one short tenant transaction that runs
//! the vector and lexical branches, fuses them with RRF and hydrates the rows
//! (ADR-008 S2, S3). Logs never contain the query text (L14).

use std::{collections::HashMap, time::Duration, time::Instant};

use common_auth::Permission;
use common_database::begin_tenant_tx;
use common_errors::AppError;
use uuid::Uuid;

use crate::{
    ai::validate_vectors,
    app_state::AppState,
    auth::request_context::RequestContext,
    domain::search::{rrf, SearchQuery},
    repositories::search_repo,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchMode {
    Hybrid,
    LexicalOnly,
}

impl SearchMode {
    pub fn as_str(self) -> &'static str {
        match self {
            SearchMode::Hybrid => "hybrid",
            SearchMode::LexicalOnly => "lexical_only",
        }
    }
}

/// Internal result. Phase 6 RAG reuses it, so it keeps the full `content`;
/// the HTTP layer exposes only `snippet` (S10).
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub chunk_id: Uuid,
    pub document_id: Uuid,
    pub chunk_index: i32,
    pub document_title: String,
    pub content: String,
    pub snippet: String,
    pub page_start: Option<u32>,
    pub page_end: Option<u32>,
    pub heading: Option<String>,
    pub embedding_version: String,
    pub score: f64,
    pub vector_rank: Option<usize>,
    pub lexical_rank: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct SearchOutcome {
    pub mode: SearchMode,
    pub hits: Vec<SearchHit>,
}

/// Runs the hybrid search pipeline for the given request context.
///
/// `ctx` is a `RequestContext` (not a bare `TenantContext`) so that
/// `ctx.chunk_target` is available for routing queries to the correct
/// `document_chunks` table (shared or dedicated, see Task 11).
///
/// `RequestContext: Deref<Target = TenantContext>`, so permission checks,
/// `ctx.tenant_id()`, and all `TenantContext` methods work directly.
pub async fn search(
    state: &AppState,
    ctx: &RequestContext,
    q: SearchQuery,
) -> Result<SearchOutcome, AppError> {
    ctx.require(Permission::DocumentsSearch)?;
    let cfg = &state.config.search;
    let started = Instant::now();

    // 1. Embed outside any transaction. Any failure degrades to lexical-only (S3).
    let embed_started = Instant::now();
    let query_vec = embed_query(state, &q.text, Duration::from_millis(cfg.embed_timeout_ms)).await;
    let embed_ms = embed_started.elapsed().as_millis() as u64;
    let mode = if query_vec.is_some() {
        SearchMode::Hybrid
    } else {
        SearchMode::LexicalOnly
    };

    // 2. One short transaction.
    let db_started = Instant::now();
    let tenant_id = ctx.tenant_id();
    let filter = q.document_ids.as_deref();
    // Resolve the fully-qualified chunk table once per request; validated at
    // RequestContext construction (ChunkTarget::from_db_opt → ValidatedSchema).
    let chunk_table = ctx.chunk_target.table();
    let mut tx = begin_tenant_tx(&state.db, ctx).await?;
    search_repo::apply_settings(&mut tx, cfg).await?;
    let vector = match &query_vec {
        Some(v) => {
            search_repo::vector_candidates(
                &mut tx,
                &chunk_table,
                tenant_id,
                v,
                filter,
                cfg.vector_candidates as i64,
            )
            .await?
        }
        None => Vec::new(),
    };
    let lexical = search_repo::lexical_candidates(
        &mut tx,
        &chunk_table,
        tenant_id,
        &q.text,
        filter,
        cfg.lexical_candidates as i64,
    )
    .await?;

    let fused = rrf(&vector, &lexical, cfg.rrf_k, q.limit);
    let ids: Vec<Uuid> = fused.iter().map(|f| f.chunk_id).collect();
    let rows = search_repo::hydrate(&mut tx, &chunk_table, tenant_id, &ids, &q.text).await?;
    tx.commit().await?;
    let db_ms = db_started.elapsed().as_millis() as u64;

    // 3. Restore RRF order. A chunk missing from `rows` was deleted since the
    //    candidate queries: drop it silently.
    let mut by_id: HashMap<Uuid, search_repo::HitRow> =
        rows.into_iter().map(|r| (r.id, r)).collect();
    let hits: Vec<SearchHit> = fused
        .into_iter()
        .filter_map(|f| {
            by_id.remove(&f.chunk_id).map(|r| SearchHit {
                chunk_id: r.id,
                document_id: r.document_id,
                chunk_index: r.chunk_index,
                document_title: r.title,
                content: r.content,
                snippet: r.snippet,
                page_start: meta_u32(&r.metadata, "page_start"),
                page_end: meta_u32(&r.metadata, "page_end"),
                heading: r
                    .metadata
                    .get("heading")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned),
                embedding_version: r.embedding_version,
                score: f.score,
                vector_rank: f.vector_rank,
                lexical_rank: f.lexical_rank,
            })
        })
        .collect();

    tracing::info!(
        target: "search",
        tenant_id = %tenant_id,
        mode = mode.as_str(),
        query_chars = q.text.chars().count(),
        vector_candidates = vector.len(),
        lexical_candidates = lexical.len(),
        results = hits.len(),
        embed_ms,
        db_ms,
        total_ms = started.elapsed().as_millis() as u64,
        "search completed"
    );

    Ok(SearchOutcome { mode, hits })
}

/// Returns the validated query vector, or `None` after logging the failure
/// class. Never logs the query text.
async fn embed_query(state: &AppState, text: &str, timeout: Duration) -> Option<Vec<f32>> {
    let embedder = &state.embedder;
    let result = tokio::time::timeout(timeout, embedder.embed(&[text.to_string()])).await;
    let reason = match result {
        Err(_) => "timeout",
        Ok(Err(_)) => "provider_error",
        Ok(Ok(mut vectors)) => {
            if validate_vectors(1, embedder.dim(), &vectors).is_ok() {
                return vectors.pop();
            }
            "invalid_vector"
        }
    };
    tracing::warn!(target: "search", reason, "query embedding failed; using lexical-only search");
    None
}

fn meta_u32(metadata: &serde_json::Value, key: &str) -> Option<u32> {
    metadata
        .get(key)
        .and_then(|v| v.as_u64())
        .and_then(|n| u32::try_from(n).ok())
}
