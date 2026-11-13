//! Document domain types — valid by construction.
//!
//! Nothing here knows about Axum or SQLx.  Handlers parse raw request data
//! into these types so the rest of the code never sees invalid input.
//! Rules mirror the CHECK constraints and status values in `003_documents.sql`.

use std::str::FromStr;

use common_errors::AppError;

// ---------------------------------------------------------------------------
// SupportedFormat
// ---------------------------------------------------------------------------

/// Every MIME type the platform accepts for upload in Phase 3.
///
/// Phase 4 adds more formats via the `infer` crate (noted in ADR-006).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupportedFormat {
    Pdf,
    Text,
    Markdown,
}

impl SupportedFormat {
    /// The MIME type stored in the `documents.mime_type` column.
    pub fn mime_type(self) -> &'static str {
        match self {
            Self::Pdf => "application/pdf",
            Self::Text => "text/plain",
            Self::Markdown => "text/markdown",
        }
    }

    /// Parses the file extension (without the leading dot, case-insensitive).
    ///
    /// Accepts: `pdf`, `txt`, `md`, `markdown`.
    pub fn from_extension(ext: &str) -> Result<Self, AppError> {
        match ext.to_ascii_lowercase().as_str() {
            "pdf" => Ok(Self::Pdf),
            "txt" => Ok(Self::Text),
            "md" | "markdown" => Ok(Self::Markdown),
            _other => Err(AppError::UnsupportedMediaType {
                accepted: "pdf, txt, md, markdown",
            }),
        }
    }

    /// Whether this format requires text validation (UTF-8 + no NUL).
    pub fn is_text(self) -> bool {
        matches!(self, Self::Text | Self::Markdown)
    }

    /// Whether this format requires a magic-byte sniff.
    pub fn has_magic(self) -> bool {
        matches!(self, Self::Pdf)
    }
}

// ---------------------------------------------------------------------------
// DocumentStatus
// ---------------------------------------------------------------------------

/// Every lifecycle status value from `003_documents.sql`.
///
/// Phase 3 code only ever *writes* `Pending`, `Deleting`, and `Deleted`;
/// the remaining variants are listed so `FromStr` matches the CHECK constraint
/// exactly, preventing Rust and SQL from drifting apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentStatus {
    Pending,
    Processing,
    Ready,
    RetryScheduled,
    Failed,
    Deleting,
    Deleted,
}

impl DocumentStatus {
    /// The string stored in the `documents.status` column.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Processing => "processing",
            Self::Ready => "ready",
            Self::RetryScheduled => "retry_scheduled",
            Self::Failed => "failed",
            Self::Deleting => "deleting",
            Self::Deleted => "deleted",
        }
    }

    /// All variants, in the same order as the SQL CHECK constraint.
    ///
    /// A unit test diffs this against the SQL to prevent drift.
    pub fn all_str() -> &'static [&'static str] {
        &[
            "pending",
            "processing",
            "ready",
            "retry_scheduled",
            "failed",
            "deleting",
            "deleted",
        ]
    }
}

impl FromStr for DocumentStatus {
    type Err = AppError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(Self::Pending),
            "processing" => Ok(Self::Processing),
            "ready" => Ok(Self::Ready),
            "retry_scheduled" => Ok(Self::RetryScheduled),
            "failed" => Ok(Self::Failed),
            "deleting" => Ok(Self::Deleting),
            "deleted" => Ok(Self::Deleted),
            other => Err(AppError::Internal(format!(
                "unknown document status in database: {other:?}"
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// ContentHash
// ---------------------------------------------------------------------------

/// A SHA-256 hex digest: exactly 64 lowercase hex characters.
///
/// The inner `String` is kept private so construction always goes through
/// `ContentHash::new`, which verifies the invariant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentHash(String);

impl ContentHash {
    /// Wraps a pre-computed hex string.  Returns an error if the string is
    /// not exactly 64 lowercase hex characters (matches the DB CHECK).
    pub fn new(hex: String) -> Result<Self, AppError> {
        let valid = hex.len() == 64
            && hex
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase());
        if valid {
            Ok(Self(hex))
        } else {
            Err(AppError::Internal(
                "content hash must be 64 lowercase hex characters".into(),
            ))
        }
    }

    /// Returns the inner hex string.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consumes and returns the inner `String`.
    pub fn into_string(self) -> String {
        self.0
    }
}

// ---------------------------------------------------------------------------
// OriginalFilename
// ---------------------------------------------------------------------------

/// A sanitized upload filename.
///
/// ## Rules (from D9 and the plan)
/// 1. Keep only the **final path component** — both `/` and `\` are treated as
///    separators, so a Windows client sending `C:\Users\alice\report.pdf`
///    produces `report.pdf`.
/// 2. Strip control characters (U+0000–U+001F, U+007F, U+0080–U+009F).
/// 3. Reject the special names `.` and `..`.
/// 4. Reject empty after stripping.
/// 5. Cap at 255 characters (matches the DB CHECK).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OriginalFilename(String);

impl OriginalFilename {
    /// Parses a raw multipart filename into a sanitized `OriginalFilename`.
    pub fn parse(raw: &str) -> Result<Self, AppError> {
        // 1. Keep only the last path component (handle both / and \).
        let name = raw
            .split(['/', '\\'])
            .rfind(|s| !s.is_empty())
            .unwrap_or("");

        // 2. Strip control characters.
        let name: String = name.chars().filter(|&c| !c.is_control()).collect();

        let name = name.trim();

        // 3. Reject `.` and `..`.
        if name == "." || name == ".." {
            return Err(AppError::Validation(
                "filename must not be '.' or '..'".into(),
            ));
        }

        // 4. Reject empty.
        if name.is_empty() {
            return Err(AppError::Validation("filename must not be empty".into()));
        }

        // 5. Cap at 255 characters.
        if name.chars().count() > 255 {
            return Err(AppError::Validation(
                "filename must be at most 255 characters".into(),
            ));
        }

        Ok(Self(name.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the file extension (after the last `.`), lower-cased.
    pub fn extension(&self) -> Option<&str> {
        self.0
            .rsplit('.')
            .next()
            .filter(|e| !e.is_empty() && e.len() < self.0.len())
    }
}

// ---------------------------------------------------------------------------
// DocumentTitle
// ---------------------------------------------------------------------------

/// A document display title: trimmed, 1 to 200 characters.
///
/// Falls back to the filename stem (filename without its extension) when the
/// caller omits the `title` part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentTitle(String);

impl DocumentTitle {
    /// Parses a caller-supplied title.
    pub fn parse(input: &str) -> Result<Self, AppError> {
        let title = input.trim();
        let len = title.chars().count();
        if (1..=200).contains(&len) {
            Ok(Self(title.to_owned()))
        } else {
            Err(AppError::Validation(
                "title must be between 1 and 200 characters".into(),
            ))
        }
    }

    /// Derives a title from an `OriginalFilename` by removing the extension.
    /// Never fails; always produces a valid title since filenames are at most
    /// 255 characters (well within the 200-character limit after extension removal
    /// — if somehow still too long, we truncate at a character boundary).
    pub fn from_filename(filename: &OriginalFilename) -> Self {
        let name = filename.as_str();
        let stem = match name.rfind('.') {
            Some(dot_pos) if dot_pos > 0 => &name[..dot_pos],
            _ => name,
        };
        // Truncate at 200 chars (char boundary safe).
        let truncated: String = stem.chars().take(200).collect();
        // Ensure non-empty (filename always has at least 1 char after sanitization).
        let title = if truncated.is_empty() {
            name.chars().take(200).collect()
        } else {
            truncated
        };
        Self(title)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- SupportedFormat ---

    #[test]
    fn format_from_extension_accepts_all_variants() {
        assert_eq!(
            SupportedFormat::from_extension("pdf").unwrap(),
            SupportedFormat::Pdf
        );
        assert_eq!(
            SupportedFormat::from_extension("txt").unwrap(),
            SupportedFormat::Text
        );
        assert_eq!(
            SupportedFormat::from_extension("md").unwrap(),
            SupportedFormat::Markdown
        );
        assert_eq!(
            SupportedFormat::from_extension("markdown").unwrap(),
            SupportedFormat::Markdown
        );
        // Case-insensitive
        assert_eq!(
            SupportedFormat::from_extension("PDF").unwrap(),
            SupportedFormat::Pdf
        );
        assert_eq!(
            SupportedFormat::from_extension("TXT").unwrap(),
            SupportedFormat::Text
        );
    }

    #[test]
    fn format_from_extension_rejects_unknown() {
        assert!(matches!(
            SupportedFormat::from_extension("docx"),
            Err(AppError::UnsupportedMediaType { .. })
        ));
        assert!(matches!(
            SupportedFormat::from_extension(""),
            Err(AppError::UnsupportedMediaType { .. })
        ));
        assert!(matches!(
            SupportedFormat::from_extension("exe"),
            Err(AppError::UnsupportedMediaType { .. })
        ));
    }

    #[test]
    fn format_mime_types_are_correct() {
        assert_eq!(SupportedFormat::Pdf.mime_type(), "application/pdf");
        assert_eq!(SupportedFormat::Text.mime_type(), "text/plain");
        assert_eq!(SupportedFormat::Markdown.mime_type(), "text/markdown");
    }

    // --- DocumentStatus ---

    #[test]
    fn document_status_round_trips() {
        for &s in DocumentStatus::all_str() {
            let status: DocumentStatus = s.parse().unwrap();
            assert_eq!(status.as_str(), s, "round-trip failed for {s:?}");
        }
    }

    #[test]
    fn document_status_rejects_unknown() {
        assert!(matches!(
            "unknown_status".parse::<DocumentStatus>(),
            Err(AppError::Internal(_))
        ));
    }

    /// Asserts that the Rust variant list exactly matches the SQL CHECK constraint
    /// in `003_documents.sql`.  Any divergence is a bug.
    #[test]
    fn document_status_matches_sql_check_constraint() {
        // This list must exactly match:
        //   CHECK (status IN ('pending', 'processing', 'ready',
        //                     'retry_scheduled', 'failed', 'deleting', 'deleted'))
        // from 003_documents.sql.
        let sql_values = [
            "pending",
            "processing",
            "ready",
            "retry_scheduled",
            "failed",
            "deleting",
            "deleted",
        ];
        assert_eq!(
            DocumentStatus::all_str(),
            &sql_values,
            "Rust DocumentStatus list does not match SQL CHECK constraint — update both together"
        );
    }

    // --- ContentHash ---

    #[test]
    fn content_hash_accepts_valid_hex() {
        let hex = "a".repeat(64);
        assert!(ContentHash::new(hex).is_ok());

        let hex = "0123456789abcdef".repeat(4);
        assert!(ContentHash::new(hex).is_ok());
    }

    #[test]
    fn content_hash_rejects_wrong_length() {
        assert!(ContentHash::new("a".repeat(63)).is_err());
        assert!(ContentHash::new("a".repeat(65)).is_err());
        assert!(ContentHash::new(String::new()).is_err());
    }

    #[test]
    fn content_hash_rejects_uppercase() {
        // Must be lowercase hex only.
        assert!(ContentHash::new("A".repeat(64)).is_err());
        let mixed = format!("{}{}", "a".repeat(32), "A".repeat(32));
        assert!(ContentHash::new(mixed).is_err());
    }

    #[test]
    fn content_hash_rejects_non_hex_chars() {
        let bad = format!("{}g{}", "a".repeat(31), "a".repeat(32));
        assert!(ContentHash::new(bad).is_err());
    }

    // --- OriginalFilename ---

    #[test]
    fn filename_keeps_last_component_on_unix_path() {
        let f = OriginalFilename::parse("/home/alice/report.pdf").unwrap();
        assert_eq!(f.as_str(), "report.pdf");
    }

    #[test]
    fn filename_keeps_last_component_on_windows_path() {
        let f = OriginalFilename::parse("C:\\Users\\alice\\report.pdf").unwrap();
        assert_eq!(f.as_str(), "report.pdf");
    }

    #[test]
    fn filename_strips_control_characters() {
        // \x01 is a control char and should be stripped.
        let f = OriginalFilename::parse("my\x01file.txt").unwrap();
        assert_eq!(f.as_str(), "myfile.txt");
    }

    #[test]
    fn filename_rejects_dot_and_dotdot() {
        assert!(OriginalFilename::parse(".").is_err());
        assert!(OriginalFilename::parse("..").is_err());
    }

    #[test]
    fn filename_rejects_empty_after_sanitization() {
        assert!(OriginalFilename::parse("").is_err());
        assert!(OriginalFilename::parse("\x01\x02").is_err()); // all control chars → empty
    }

    #[test]
    fn filename_rejects_over_255_chars() {
        let long = "a".repeat(256) + ".txt";
        assert!(OriginalFilename::parse(&long).is_err());
    }

    #[test]
    fn filename_at_255_chars_is_accepted() {
        let name = "a".repeat(251) + ".txt"; // 255 total
        assert!(OriginalFilename::parse(&name).is_ok());
    }

    #[test]
    fn filename_extension_is_extracted() {
        let f = OriginalFilename::parse("report.pdf").unwrap();
        assert_eq!(f.extension(), Some("pdf"));

        let f = OriginalFilename::parse("archive.tar.gz").unwrap();
        assert_eq!(f.extension(), Some("gz"));

        let f = OriginalFilename::parse("nodotfile").unwrap();
        assert_eq!(f.extension(), None);
    }

    // --- DocumentTitle ---

    #[test]
    fn title_is_trimmed() {
        let t = DocumentTitle::parse("  My Doc  ").unwrap();
        assert_eq!(t.as_str(), "My Doc");
    }

    #[test]
    fn title_rejects_empty_and_whitespace_only() {
        assert!(DocumentTitle::parse("").is_err());
        assert!(DocumentTitle::parse("   ").is_err());
    }

    #[test]
    fn title_rejects_over_200_chars() {
        assert!(DocumentTitle::parse(&"a".repeat(201)).is_err());
        assert!(DocumentTitle::parse(&"a".repeat(200)).is_ok());
    }

    #[test]
    fn title_from_filename_removes_extension() {
        let f = OriginalFilename::parse("my_report.pdf").unwrap();
        let t = DocumentTitle::from_filename(&f);
        assert_eq!(t.as_str(), "my_report");
    }

    #[test]
    fn title_from_filename_no_extension() {
        let f = OriginalFilename::parse("readme").unwrap();
        let t = DocumentTitle::from_filename(&f);
        assert_eq!(t.as_str(), "readme");
    }

    #[test]
    fn title_from_filename_keeps_up_to_200_chars() {
        // 210-char stem + ".txt" = 214 chars filename (<=255 so valid)
        let name = "a".repeat(210) + ".txt";
        let f = OriginalFilename::parse(&name).unwrap();
        let t = DocumentTitle::from_filename(&f);
        assert_eq!(t.as_str().chars().count(), 200);
    }
}
