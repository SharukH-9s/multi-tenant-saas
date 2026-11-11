use axum::{
    extract::{rejection::JsonRejection, FromRequest},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;
use thiserror::Error;

/// All application errors.
///
/// Rules:
/// - Never expose SQL errors, stack traces, or internal details to clients.
/// - Map all `sqlx::Error` to safe variants with generic messages.
/// - `request_id` is injected by the response layer, not stored here.
#[derive(Debug, Error)]
pub enum AppError {
    #[error("resource not found")]
    NotFound,

    #[error("authentication required")]
    Unauthorized,

    #[error("permission denied")]
    Forbidden,

    #[error("bad request: {0}")]
    BadRequest(String),

    #[error("invalid credentials")]
    InvalidCredentials,

    #[error("conflict ({code}): {message}")]
    Conflict { code: &'static str, message: String },

    #[error("rate limited; retry after {retry_after_secs}s")]
    TooManyRequests { retry_after_secs: u64 },

    #[error("validation error: {0}")]
    Validation(String),

    #[error("database error")]
    Database(#[source] sqlx::Error),

    #[error("external service unavailable: {0}")]
    ExternalService(String),

    #[error("internal error")]
    Internal(String),
}

/// Response extension marking a response as produced by an `AppError`.
///
/// The app's response layer uses it to find error envelopes and inject
/// `error.request_id`, since `AppError` itself holds no request state.
#[derive(Debug, Clone, Copy)]
pub struct AppErrorMarker;

/// Safe JSON error envelope sent to clients.
/// Never includes internal details, SQL messages, or stack traces.
#[derive(Serialize)]
struct ErrorBody {
    error: ErrorDetail,
}

#[derive(Serialize)]
struct ErrorDetail {
    code: &'static str,
    message: String,
}

impl AppError {
    /// Machine-readable error code sent in the response body.
    fn code(&self) -> &'static str {
        match self {
            AppError::NotFound => "NOT_FOUND",
            AppError::Unauthorized => "UNAUTHORIZED",
            AppError::Forbidden => "FORBIDDEN",
            AppError::BadRequest(_) => "BAD_REQUEST",
            AppError::InvalidCredentials => "INVALID_CREDENTIALS",
            AppError::Conflict { code, .. } => code,
            AppError::TooManyRequests { .. } => "RATE_LIMITED",
            AppError::Validation(_) => "VALIDATION_ERROR",
            AppError::Database(_) => "DATABASE_ERROR",
            AppError::ExternalService(_) => "SERVICE_UNAVAILABLE",
            AppError::Internal(_) => "INTERNAL_ERROR",
        }
    }

    /// HTTP status code for the error.
    fn status(&self) -> StatusCode {
        match self {
            AppError::NotFound => StatusCode::NOT_FOUND,
            AppError::Unauthorized => StatusCode::UNAUTHORIZED,
            AppError::Forbidden => StatusCode::FORBIDDEN,
            AppError::BadRequest(_) => StatusCode::BAD_REQUEST,
            AppError::InvalidCredentials => StatusCode::UNAUTHORIZED,
            AppError::Conflict { .. } => StatusCode::CONFLICT,
            AppError::TooManyRequests { .. } => StatusCode::TOO_MANY_REQUESTS,
            AppError::Validation(_) => StatusCode::UNPROCESSABLE_ENTITY,
            AppError::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
            AppError::ExternalService(_) => StatusCode::SERVICE_UNAVAILABLE,
            AppError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// Client-safe message. Never includes SQL text or internal paths.
    fn safe_message(&self) -> String {
        match self {
            // These variants have user-visible context that is safe to surface
            AppError::Conflict { message, .. } => message.clone(),
            AppError::BadRequest(msg) => msg.clone(),
            AppError::InvalidCredentials => "Invalid email or password.".into(),
            AppError::TooManyRequests { .. } => "Too many requests. Please try again later.".into(),
            AppError::Validation(msg) => msg.clone(),
            AppError::ExternalService(svc) => {
                format!("The service '{}' is temporarily unavailable.", svc)
            }
            // All others use a generic message — never leak internal details
            AppError::NotFound => "The requested resource was not found.".into(),
            AppError::Unauthorized => "Authentication is required.".into(),
            AppError::Forbidden => "You do not have permission to perform this action.".into(),
            AppError::Database(_) => "A database error occurred. Please try again.".into(),
            AppError::Internal(_) => "An unexpected error occurred. Please try again.".into(),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        // Log internal errors with full context, but never send them to clients.
        match &self {
            AppError::Database(e) => {
                tracing::error!(error = %e, "Database error");
            }
            AppError::Internal(msg) => {
                tracing::error!(message = %msg, "Internal error");
            }
            AppError::ExternalService(svc) => {
                tracing::warn!(service = %svc, "External service unavailable");
            }
            _ => {}
        }

        let status = self.status();
        let body = ErrorBody {
            error: ErrorDetail {
                code: self.code(),
                message: self.safe_message(),
            },
        };

        let mut response = (status, Json(body)).into_response();
        if let AppError::TooManyRequests { retry_after_secs } = &self {
            if let Ok(value) = axum::http::HeaderValue::from_str(&retry_after_secs.to_string()) {
                response
                    .headers_mut()
                    .insert(axum::http::header::RETRY_AFTER, value);
            }
        }
        response.extensions_mut().insert(AppErrorMarker);
        response
    }
}

/// Map sqlx errors to AppError.
/// DatabaseError rows not found → NotFound; everything else → Database.
impl From<sqlx::Error> for AppError {
    fn from(e: sqlx::Error) -> Self {
        match e {
            sqlx::Error::RowNotFound => AppError::NotFound,
            other => AppError::Database(other),
        }
    }
}

/// Maps axum's JSON rejections onto the error envelope.
/// Serde's detailed messages are deliberately not echoed to clients.
impl From<JsonRejection> for AppError {
    fn from(rejection: JsonRejection) -> Self {
        match rejection {
            JsonRejection::JsonDataError(_) => {
                AppError::Validation("The request body is invalid.".into())
            }
            JsonRejection::JsonSyntaxError(_) => {
                AppError::BadRequest("The request body is not valid JSON.".into())
            }
            JsonRejection::MissingJsonContentType(_) => {
                AppError::BadRequest("Expected `Content-Type: application/json`.".into())
            }
            _ => AppError::BadRequest("The request body could not be read.".into()),
        }
    }
}

/// Drop-in replacement for `axum::Json` as an extractor whose rejections use
/// the standard error envelope.
#[derive(FromRequest)]
#[from_request(via(axum::Json), rejection(AppError))]
pub struct ApiJson<T>(pub T);

impl<T: Serialize> IntoResponse for ApiJson<T> {
    fn into_response(self) -> Response {
        Json(self.0).into_response()
    }
}

impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        AppError::Internal(format!("I/O error: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    #[test]
    fn conflict_uses_its_own_code_and_409() {
        let err = AppError::Conflict {
            code: "SLUG_TAKEN",
            message: "Slug is taken.".into(),
        };
        assert_eq!(err.code(), "SLUG_TAKEN");
        assert_eq!(err.status(), StatusCode::CONFLICT);
    }

    #[test]
    fn invalid_credentials_maps_to_401() {
        assert_eq!(
            AppError::InvalidCredentials.status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(AppError::InvalidCredentials.code(), "INVALID_CREDENTIALS");
    }

    #[test]
    fn too_many_requests_sets_retry_after_and_marker() {
        let res = AppError::TooManyRequests {
            retry_after_secs: 7,
        }
        .into_response();
        assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(res.headers()["retry-after"], "7");
        assert!(res.extensions().get::<AppErrorMarker>().is_some());
    }

    #[test]
    fn not_found_maps_to_404() {
        assert_eq!(AppError::NotFound.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn unauthorized_maps_to_401() {
        assert_eq!(AppError::Unauthorized.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn forbidden_maps_to_403() {
        assert_eq!(AppError::Forbidden.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn database_error_does_not_leak_sql_message() {
        let err = AppError::Database(sqlx::Error::RowNotFound);
        // safe_message must not contain "row" or SQL terms
        let msg = err.safe_message();
        assert!(!msg.to_lowercase().contains("row"));
        assert!(!msg.to_lowercase().contains("sql"));
    }

    #[test]
    fn internal_error_has_safe_message() {
        let err = AppError::Internal("stack trace here".into());
        let msg = err.safe_message();
        // Must not leak the internal detail
        assert!(!msg.contains("stack trace"));
    }

    #[test]
    fn sqlx_row_not_found_converts_to_app_not_found() {
        let app_err: AppError = sqlx::Error::RowNotFound.into();
        assert!(matches!(app_err, AppError::NotFound));
    }
}
