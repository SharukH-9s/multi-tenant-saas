//! `POST /tenants/{tenant_id}/search`: hybrid search over the tenant's chunks.
//! Responses carry snippets only, never full content (ADR-008 S10).

use axum::{extract::State, routing::post, Json, Router};
use common_errors::{ApiJson, AppError};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    app_state::AppState,
    auth::extractors::TenantCtx,
    domain::search::SearchQuery,
    services::search_service::{self, SearchHit},
};

pub fn router() -> Router<AppState> {
    // Mounted under /tenants/{tenant_id} by the parent router.
    Router::new().route("/search", post(search))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchRequest {
    query: String,
    limit: Option<usize>,
    document_ids: Option<Vec<Uuid>>,
}

#[derive(Serialize)]
struct SearchResponse {
    mode: &'static str,
    results: Vec<ResultItem>,
}

#[derive(Serialize)]
struct ResultItem {
    document_id: Uuid,
    chunk_index: i32,
    chunk_id: Uuid,
    document_title: String,
    snippet: String,
    page_start: Option<u32>,
    page_end: Option<u32>,
    heading: Option<String>,
    score: f64,
    vector_rank: Option<usize>,
    lexical_rank: Option<usize>,
}

impl From<SearchHit> for ResultItem {
    fn from(h: SearchHit) -> Self {
        Self {
            document_id: h.document_id,
            chunk_index: h.chunk_index,
            chunk_id: h.chunk_id,
            document_title: h.document_title,
            snippet: h.snippet,
            page_start: h.page_start,
            page_end: h.page_end,
            heading: h.heading,
            score: h.score,
            vector_rank: h.vector_rank,
            lexical_rank: h.lexical_rank,
        }
    }
}

async fn search(
    State(state): State<AppState>,
    TenantCtx(ctx): TenantCtx,
    ApiJson(body): ApiJson<SearchRequest>,
) -> Result<Json<SearchResponse>, AppError> {
    let q = SearchQuery::parse(
        &body.query,
        body.limit,
        body.document_ids,
        &state.config.search,
    )?;
    let outcome = search_service::search(&state, &ctx, q).await?;
    Ok(Json(SearchResponse {
        mode: outcome.mode.as_str(),
        results: outcome.hits.into_iter().map(ResultItem::from).collect(),
    }))
}
