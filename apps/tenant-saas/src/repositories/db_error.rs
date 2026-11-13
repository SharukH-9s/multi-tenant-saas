use common_errors::AppError;
use sqlx::postgres::PgDatabaseError;

/// Turns a database error into an `AppError`.
///
/// Knowing which constraint means what is app knowledge, so it lives here and
/// not in `common-errors`. Anything not listed falls back to the generic
/// conversion, which never leaks SQL text to clients.
///
/// Handled cases:
/// - Unique violations mapped by constraint name.
/// - `P0001` (PL/pgSQL `RAISE EXCEPTION`) with hint `LAST_OWNER` from the
///   `memberships_protect_last_owner` trigger.
pub fn map_db_error(error: sqlx::Error) -> AppError {
    if let sqlx::Error::Database(db_error) = &error {
        // ── Unique constraint violations ─────────────────────────────────────
        if db_error.is_unique_violation() {
            let conflict = match db_error.constraint() {
                Some("users_email_key") => Some((
                    "EMAIL_ALREADY_REGISTERED",
                    "An account with this email already exists.",
                )),
                Some("tenants_slug_key") => Some(("SLUG_TAKEN", "This slug is already taken.")),
                Some("memberships_tenant_user_unique") => {
                    Some(("ALREADY_MEMBER", "This user is already a member."))
                }
                _ => None,
            };
            if let Some((code, message)) = conflict {
                return AppError::Conflict {
                    code,
                    message: message.into(),
                };
            }
        }

        // ── Last-owner trigger (P0001 + hint "LAST_OWNER") ───────────────────
        // The `memberships_protect_last_owner` trigger raises SQLSTATE P0001
        // with HINT = 'LAST_OWNER' when a demotion or removal would leave a
        // tenant with no active owner.
        //
        // `hint()` is Postgres-specific, so we downcast to `PgDatabaseError`.
        if let Some(pg_err) = db_error.try_downcast_ref::<PgDatabaseError>() {
            if pg_err.code() == "P0001" && pg_err.hint().is_some_and(|h| h.contains("LAST_OWNER")) {
                return AppError::Conflict {
                    code: "LAST_OWNER",
                    message: "The tenant must retain at least one active owner.".into(),
                };
            }
        }
    }
    classify_sqlx_error(error)
}

/// Classifies a raw sqlx error into the appropriate [`AppError`] variant.
///
/// Connection-level failures — pool timeout, I/O errors, and SQLSTATE class
/// `08` (connection exception) — become [`AppError::ExternalService`] which
/// maps to **503 Service Unavailable**, signalling a temporary infrastructure
/// problem rather than an application bug (§16.3 / D12).
///
/// All other errors (including row-not-found, constraint violations, and query
/// errors) fall back to the standard [`AppError::from`] conversion so that
/// existing behaviour is unchanged.
pub fn classify_sqlx_error(error: sqlx::Error) -> AppError {
    match &error {
        // Pool exhausted waiting for a connection — transient infrastructure issue.
        sqlx::Error::PoolTimedOut => {
            tracing::warn!("database pool timed out");
            return AppError::ExternalService("database".into());
        }
        // TCP / TLS level failure — the server is unreachable.
        sqlx::Error::Io(_) => {
            tracing::warn!(error = %error, "database I/O error");
            return AppError::ExternalService("database".into());
        }
        // SQLSTATE class 08 = "Connection Exception".
        sqlx::Error::Database(db_err) => {
            let is_connection_class = db_err.code().map(|c| c.starts_with("08")).unwrap_or(false);
            if is_connection_class {
                tracing::warn!(sqlstate = ?db_err.code(), "database connection exception");
                return AppError::ExternalService("database".into());
            }
        }
        _ => {}
    }
    AppError::from(error)
}

#[cfg(test)]
mod classify_tests {
    use super::*;
    use common_errors::AppError;

    #[test]
    fn pool_timed_out_becomes_external_service() {
        let err = classify_sqlx_error(sqlx::Error::PoolTimedOut);
        assert!(matches!(err, AppError::ExternalService(_)));
    }

    #[test]
    fn row_not_found_stays_not_found() {
        let err = classify_sqlx_error(sqlx::Error::RowNotFound);
        assert!(matches!(err, AppError::NotFound));
    }

    #[test]
    fn pool_closed_stays_database_error() {
        // PoolClosed is a logic error, not a transient infra problem.
        let err = classify_sqlx_error(sqlx::Error::PoolClosed);
        assert!(matches!(err, AppError::Database(_)));
    }
}
