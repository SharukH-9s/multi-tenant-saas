//! Chunk routing domain types (Phase 5, Option B.1).
//!
//! `ChunkTarget` decides which table a chunk read or write hits.
//! `ValidatedSchema` is a newtype that enforces the naming contract
//! (`^tenant_[a-z0-9_]{1,56}$`) at construction so callers can interpolate
//! it directly into SQL query strings without further escaping or injection
//! risk (see §3 of `dedicated-schema-redesign-analysis.md`).
//!
//! Nothing here touches Axum or SQLx.

use std::borrow::Cow;

use common_errors::AppError;

/// Maximum allowed length of the suffix that follows `tenant_` in a schema name.
/// PostgreSQL's identifier limit (NAMEDATALEN - 1) is 63 bytes total; `tenant_`
/// is 7 bytes, leaving at most 56 bytes for the suffix.
const MAX_SCHEMA_SUFFIX_LEN: usize = 56;

// ── ValidatedSchema ──────────────────────────────────────────────────────────

/// A PostgreSQL schema name validated against `^tenant_[a-z0-9_]{1,56}$`.
///
/// The value is safe to interpolate into a SQL identifier — it was validated
/// at construction time and can only originate from the
/// `tenants.dedicated_schema` DB column, which carries the same regex as a
/// CHECK constraint (migrations `007` and `008`). Total length is guaranteed
/// <= 63 bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedSchema(String);

impl ValidatedSchema {
    /// Builds from a raw schema name read from the database.
    ///
    /// Returns `Err(AppError::Internal)` when the value violates the regex.
    /// In production this only fires if the DB CHECK constraint was bypassed
    /// (e.g. by a superuser writing directly), so it is treated as an
    /// internal error rather than a client-visible validation error.
    pub fn from_db(raw: &str) -> Result<Self, AppError> {
        if is_valid_schema(raw) {
            Ok(Self(raw.to_owned()))
        } else {
            Err(AppError::Internal(format!(
                "dedicated_schema value '{}' violates the naming invariant \
                 (^tenant_[a-z0-9_]{{1,56}}$); the DB CHECK constraint may \
                 have been bypassed",
                raw
            )))
        }
    }

    /// The raw schema name, guaranteed safe to interpolate into a SQL query.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

// ── ChunkTarget ──────────────────────────────────────────────────────────────

/// Which `document_chunks` table a chunk read or write targets.
///
/// `Shared` → `public.document_chunks` (the default for all tenants).
/// `Dedicated` → `tenant_<id>.document_chunks` (promoted tenants only).
///
/// Callers obtain a `ChunkTarget` from one of two paths:
///
/// * **HTTP path** — resolved once per request at middleware time by
///   `auth::tenant_resolver::resolve`, carried in `RequestContext`.
/// * **Worker path** — resolved at the transaction execution boundary by
///   `repositories::tenant_repo::get_dedicated_schema`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChunkTarget {
    /// Tenant uses the shared `public.document_chunks` table.
    Shared,
    /// Tenant has been promoted to its own schema.
    Dedicated(ValidatedSchema),
}

impl ChunkTarget {
    /// Builds from an optional raw schema name coming from the database.
    ///
    /// `None` → `Shared`; `Some(name)` → `Dedicated(ValidatedSchema)`.
    pub fn from_db_opt(raw: Option<&str>) -> Result<Self, AppError> {
        match raw {
            None => Ok(Self::Shared),
            Some(s) => ValidatedSchema::from_db(s).map(Self::Dedicated),
        }
    }

    /// The fully qualified table identifier, safe for SQL string interpolation.
    ///
    /// Returns a `'static` borrow for the common `Shared` case to avoid a
    /// heap allocation on every search request.
    pub fn table(&self) -> Cow<'static, str> {
        match self {
            Self::Shared => Cow::Borrowed("public.document_chunks"),
            Self::Dedicated(schema) => Cow::Owned(format!("{}.document_chunks", schema.as_str())),
        }
    }

    /// `true` iff this tenant uses the shared public table.
    pub fn is_shared(&self) -> bool {
        matches!(self, Self::Shared)
    }
}

// ── Validation ───────────────────────────────────────────────────────────────

fn is_valid_schema(s: &str) -> bool {
    // Pattern: ^tenant_[a-z0-9_]{1,56}$
    // Must start with "tenant_", followed by 1–56 chars of [a-z0-9_].
    let Some(suffix) = s.strip_prefix("tenant_") else {
        return false;
    };
    let len = suffix.len();
    if len == 0 || len > MAX_SCHEMA_SUFFIX_LEN {
        return false;
    }
    suffix
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── ValidatedSchema ──

    #[test]
    fn valid_schema_names_are_accepted() {
        for name in [
            "tenant_abc",
            "tenant_018f1a2b3c4d",
            "tenant_a_b_c",
            "tenant_123",
            "tenant_018f1a2b_3c4d",
            &format!("tenant_{}", "a".repeat(56)),
        ] {
            assert!(ValidatedSchema::from_db(name).is_ok(), "rejected: {name}");
        }
    }

    #[test]
    fn invalid_schema_names_are_rejected() {
        for name in [
            "",
            "tenant_", // empty suffix
            "public",  // wrong prefix
            "document_chunks",
            "TENANT_ABC",                          // uppercase
            "tenant_abc!",                         // special char
            "tenant_abc xyz",                      // space
            "tenant_abc-xyz",                      // hyphen not allowed
            &format!("tenant_{}", "a".repeat(57)), // suffix too long (> 56 chars, total > 63 bytes)
        ] {
            assert!(ValidatedSchema::from_db(name).is_err(), "accepted: {name}");
        }
    }

    #[test]
    fn as_str_round_trips() {
        let s = ValidatedSchema::from_db("tenant_abc123").unwrap();
        assert_eq!(s.as_str(), "tenant_abc123");
    }

    // ── ChunkTarget ──

    #[test]
    fn shared_target_returns_public_table() {
        assert_eq!(ChunkTarget::Shared.table(), "public.document_chunks");
        assert!(ChunkTarget::Shared.is_shared());
    }

    #[test]
    fn dedicated_target_returns_schema_qualified_table() {
        let schema = ValidatedSchema::from_db("tenant_abc").unwrap();
        let target = ChunkTarget::Dedicated(schema);
        assert_eq!(target.table(), "tenant_abc.document_chunks");
        assert!(!target.is_shared());
    }

    #[test]
    fn from_db_opt_none_gives_shared() {
        assert_eq!(ChunkTarget::from_db_opt(None).unwrap(), ChunkTarget::Shared);
    }

    #[test]
    fn from_db_opt_some_valid_gives_dedicated() {
        let t = ChunkTarget::from_db_opt(Some("tenant_xyz")).unwrap();
        assert!(matches!(t, ChunkTarget::Dedicated(_)));
        assert_eq!(t.table(), "tenant_xyz.document_chunks");
    }

    #[test]
    fn from_db_opt_some_invalid_is_internal_error() {
        let err = ChunkTarget::from_db_opt(Some("'; DROP TABLE tenants; --")).unwrap_err();
        assert!(matches!(err, AppError::Internal(_)));
    }
}
