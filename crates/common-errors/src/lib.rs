use axum::{
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

    #[error("conflict: {0}")]
    Conflict(String),

    #[error("validation error: {0}")]
    Validation(String),

    #[error("database error")]
    Database(#[source] sqlx::Error),

    #[error("external service unavailable: {0}")]
    ExternalService(String),

    #[error("internal error")]
    Internal(String),
}

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
            AppError::Conflict(_) => "CONFLICT",
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
            AppError::Conflict(_) => StatusCode::CONFLICT,
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
            AppError::Conflict(msg) => msg.clone(),
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

        (status, Json(body)).into_response()
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
