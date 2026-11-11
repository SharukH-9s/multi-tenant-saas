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
    AppError::from(error)
}
