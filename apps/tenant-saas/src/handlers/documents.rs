//! `/documents` — upload, list, get, status, and delete documents.
//!
//! Merged into the existing `{tenant_id}` sub-router in `routes.rs` so the
//! `TenantCtx` extractor resolves the membership from the path parameter.

use axum::{
    extract::{DefaultBodyLimit, FromRequest, Multipart, Path, Query, Request, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use common_errors::AppError;
use serde::{Deserialize, Serialize};
use tower::ServiceBuilder;
use tower_http::timeout::TimeoutLayer;
use uuid::Uuid;

use crate::{
    app_state::AppState,
    auth::extractors::TenantCtx,
    domain::document::{DocumentTitle, OriginalFilename, SupportedFormat},
    repositories::document_repo::{Cursor, DocumentRow},
    services::document_service::{self, IncomingUpload, ListQuery},
};

use std::time::Duration;

/// Upload route timeout: 120s overrides the global 30s.
/// A 20 MiB upload on a slow connection legitimately takes longer than 30s.
/// The global `TimeoutLayer` is a known envelope gap — it returns a bare 408
/// without the JSON error envelope.  This route-local layer prevents that
/// for uploads.
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(120);

/// 1 MiB headroom above `max_upload_bytes` for multipart overhead (boundary,
/// headers, `title` field).
const BODY_LIMIT_HEADROOM: u64 = 1_048_576;

/// Extractor that enforces validation checks 1 and 2 before parsing the body:
///
/// 1. Content-Type must be `multipart/form-data` (415 `UNSUPPORTED_MEDIA_TYPE`).
/// 2. Declared `Content-Length` must be within `max_upload_bytes + 1 MiB` (413 `PAYLOAD_TOO_LARGE`).
pub struct MultipartUpload(pub Multipart);

impl FromRequest<AppState> for MultipartUpload {
    type Rejection = AppError;

    async fn from_request(req: Request, state: &AppState) -> Result<Self, Self::Rejection> {
        let content_type = req
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        if !content_type.starts_with("multipart/form-data") {
            return Err(AppError::UnsupportedMediaType {
                accepted: "multipart/form-data",
            });
        }

        let max_allowed = state.config.storage.max_upload_bytes + BODY_LIMIT_HEADROOM;
        if let Some(cl_header) = req.headers().get(axum::http::header::CONTENT_LENGTH) {
            if let Ok(cl_str) = cl_header.to_str() {
                if let Ok(cl) = cl_str.parse::<u64>() {
                    if cl > max_allowed {
                        return Err(AppError::PayloadTooLarge);
                    }
                }
            }
        }

        let multipart = Multipart::from_request(req, state).await.map_err(|e| {
            if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
                AppError::PayloadTooLarge
            } else {
                AppError::UnsupportedMediaType {
                    accepted: "multipart/form-data",
                }
            }
        })?;

        Ok(MultipartUpload(multipart))
    }
}

pub fn router(state: &AppState) -> Router<AppState> {
    let max_upload = state.config.storage.max_upload_bytes;

    // Route-local layers for the upload endpoint only.
    let upload_route = post(upload_document).layer(
        ServiceBuilder::new()
            .layer(DefaultBodyLimit::max(
                (max_upload + BODY_LIMIT_HEADROOM) as usize,
            ))
            .layer(TimeoutLayer::with_status_code(
                StatusCode::REQUEST_TIMEOUT,
                UPLOAD_TIMEOUT,
            )),
    );

    Router::new()
        .route("/documents", upload_route.get(list_documents))
        .route(
            "/documents/{document_id}",
            get(get_document).delete(delete_document),
        )
        .route("/documents/{document_id}/status", get(get_status))
}

// ── Response DTOs ────────────────────────────────────────────────────────────

/// Fields exposed to clients.  Deliberately omits storage key,
/// `failure_detail`, and `deleted_at`.  `content_hash` is included so a
/// client can detect a duplicate locally before uploading.
#[derive(Serialize)]
struct DocumentResponse {
    id: Uuid,
    title: String,
    original_filename: String,
    content_hash: String,
    mime_type: String,
    size_bytes: i64,
    status: String,
    chunk_count: i32,
    uploaded_by: Option<Uuid>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

/// Small body for the status polling endpoint.
#[derive(Serialize)]
struct DocumentStatusResponse {
    id: Uuid,
    status: String,
    chunk_count: i32,
    failure_code: Option<String>,
    updated_at: DateTime<Utc>,
}

// ── Query types ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ListDocumentsQuery {
    after_created_at: Option<DateTime<Utc>>,
    after_id: Option<Uuid>,
    #[serde(default = "default_limit")]
    limit: i64,
    status: Option<String>,
}

fn default_limit() -> i64 {
    50
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn doc_response(row: DocumentRow) -> DocumentResponse {
    DocumentResponse {
        id: row.id,
        title: row.title,
        original_filename: row.original_filename,
        content_hash: row.content_hash,
        mime_type: row.mime_type,
        size_bytes: row.size_bytes,
        status: row.status,
        chunk_count: row.chunk_count,
        uploaded_by: row.uploaded_by,
        created_at: row.created_at,
        updated_at: row.updated_at,
    }
}

/// Maps axum's `MultipartError` into `AppError` so the body keeps the
/// JSON error envelope.
fn multipart_err(e: axum::extract::multipart::MultipartError) -> AppError {
    // axum returns 413 for body-too-large multipart errors
    let status = e.status();
    if status == StatusCode::PAYLOAD_TOO_LARGE {
        AppError::PayloadTooLarge
    } else {
        AppError::Validation(format!("multipart error: {e}"))
    }
}

// ── Handlers ─────────────────────────────────────────────────────────────────

/// `POST /documents` — multipart upload.
///
/// Returns **202** for a new document, **200** for a duplicate.
///
/// The multipart loop:
/// 1. `title` goes through `field.text()`, bounded by the route body limit.
/// 2. `file` records the filename, parses the format, then collects chunks.
/// 3. Unknown field names are skipped.
/// 4. A second `file` field returns 422.
async fn upload_document(
    State(state): State<AppState>,
    TenantCtx(ctx): TenantCtx,
    MultipartUpload(mut multipart): MultipartUpload,
) -> Result<impl IntoResponse, AppError> {
    let mut title_text: Option<String> = None;
    let mut file_data: Option<(String, SupportedFormat, Vec<Bytes>)> = None;

    while let Some(mut field) = multipart.next_field().await.map_err(multipart_err)? {
        let field_name = field.name().unwrap_or("").to_owned();

        match field_name.as_str() {
            "title" => {
                let text = field.text().await.map_err(multipart_err)?;
                title_text = Some(text);
            }
            "file" => {
                // A second `file` field is 422.
                if file_data.is_some() {
                    return Err(AppError::Validation("only one file part is allowed".into()));
                }

                // Extract and validate the filename.
                let raw_filename = field
                    .file_name()
                    .map(|s| s.to_owned())
                    .ok_or_else(|| AppError::Validation("file part must have a filename".into()))?;

                let filename = OriginalFilename::parse(&raw_filename)?;
                let ext = filename.extension().ok_or_else(|| {
                    AppError::Validation("filename must have an extension".into())
                })?;
                let format = SupportedFormat::from_extension(ext)?;

                // Collect chunks from the multipart field.
                let mut chunks = Vec::new();
                while let Some(chunk) = field.chunk().await.map_err(multipart_err)? {
                    chunks.push(chunk);
                }

                file_data = Some((filename.as_str().to_owned(), format, chunks));
            }
            _ => {
                // Unknown field — skip without reading.
                // (Calling .text() or .chunk() would consume the body.)
            }
        }
    }

    // Require exactly one `file` part.
    let (original_filename, format, chunks) =
        file_data.ok_or_else(|| AppError::Validation("a 'file' part is required".into()))?;

    // Derive title: explicit title or fall back to filename stem.
    let filename_for_title =
        OriginalFilename::parse(&original_filename).expect("already validated");
    let title = match title_text {
        Some(ref t) => DocumentTitle::parse(t)?,
        None => DocumentTitle::from_filename(&filename_for_title),
    };

    let result = document_service::upload(
        &state,
        &ctx,
        IncomingUpload {
            title: title.into_string(),
            original_filename,
            format,
            chunks,
        },
    )
    .await?;

    let status_code = if result.deduplicated {
        StatusCode::OK
    } else {
        StatusCode::ACCEPTED
    };

    Ok((status_code, Json(doc_response(result.document))))
}

/// `GET /documents` — keyset-paginated list, newest first.
async fn list_documents(
    State(state): State<AppState>,
    TenantCtx(ctx): TenantCtx,
    Query(query): Query<ListDocumentsQuery>,
) -> Result<Json<Vec<DocumentResponse>>, AppError> {
    let limit = query.limit.clamp(1, 100);

    let cursor = match (query.after_created_at, query.after_id) {
        (Some(ts), Some(id)) => Some(Cursor { created_at: ts, id }),
        _ => None,
    };

    let rows = document_service::list(
        &state,
        &ctx,
        ListQuery {
            cursor,
            limit,
            status: query.status,
        },
    )
    .await?;

    Ok(Json(rows.into_iter().map(doc_response).collect()))
}

/// `GET /documents/{document_id}` — single document.
async fn get_document(
    State(state): State<AppState>,
    TenantCtx(ctx): TenantCtx,
    Path((_tenant_id, document_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<DocumentResponse>, AppError> {
    let row = document_service::get(&state, &ctx, document_id).await?;
    Ok(Json(doc_response(row)))
}

/// `GET /documents/{document_id}/status` — lightweight polling endpoint.
async fn get_status(
    State(state): State<AppState>,
    TenantCtx(ctx): TenantCtx,
    Path((_tenant_id, document_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<DocumentStatusResponse>, AppError> {
    let row = document_service::get(&state, &ctx, document_id).await?;
    Ok(Json(DocumentStatusResponse {
        id: row.id,
        status: row.status,
        chunk_count: row.chunk_count,
        failure_code: row.failure_code,
        updated_at: row.updated_at,
    }))
}

/// `DELETE /documents/{document_id}` — soft-delete + storage cleanup.
async fn delete_document(
    State(state): State<AppState>,
    TenantCtx(ctx): TenantCtx,
    Path((_tenant_id, document_id)): Path<(Uuid, Uuid)>,
) -> Result<impl IntoResponse, AppError> {
    document_service::delete(&state, &ctx, document_id).await?;
    Ok(StatusCode::NO_CONTENT)
}
