//! Ingestion job lifecycle, failure codes and retry backoff. Pure: no I/O.
//!
//! The job state machine is drawn in the Phase 4 plan, section 3.

use std::{str::FromStr, time::Duration};

use common_errors::AppError;

// ---------------------------------------------------------------------------
// JobStatus
// ---------------------------------------------------------------------------

/// `ingestion_jobs.status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JobStatus {
    Pending,
    Processing,
    RetryScheduled,
    Succeeded,
    Failed,
    Cancelled,
}

impl JobStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Processing => "processing",
            Self::RetryScheduled => "retry_scheduled",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// Every value, in the order of the SQL CHECK constraint. A unit test diffs
    /// this against `004_ingestion.sql`.
    pub fn all_str() -> &'static [&'static str] {
        &[
            "pending",
            "processing",
            "retry_scheduled",
            "succeeded",
            "failed",
            "cancelled",
        ]
    }

    /// Statuses a job can still leave. These are the ones the partial unique
    /// index `ingestion_jobs_one_active` covers.
    pub fn is_active(self) -> bool {
        matches!(
            self,
            Self::Pending | Self::Processing | Self::RetryScheduled
        )
    }

    /// The edges of the job state machine. A status never "transitions" to
    /// itself. Terminal statuses (`succeeded`, `failed`, `cancelled`) have no
    /// outgoing edges: a manual retry creates a new job instead.
    pub fn can_transition_to(self, next: JobStatus) -> bool {
        use JobStatus::*;
        matches!(
            (self, next),
            // claim, or the document was deleted before a worker got to it
            (Pending, Processing)
                | (Pending, Cancelled)
                // success, permanent or exhausted failure, transient failure
                // with attempts left, lease expired (sweeper), document deleted
                | (Processing, Succeeded)
                | (Processing, Failed)
                | (Processing, RetryScheduled)
                | (Processing, Pending)
                | (Processing, Cancelled)
                // backoff elapsed (sweeper), document deleted
                | (RetryScheduled, Pending)
                | (RetryScheduled, Cancelled)
        )
    }
}

impl FromStr for JobStatus {
    type Err = AppError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(Self::Pending),
            "processing" => Ok(Self::Processing),
            "retry_scheduled" => Ok(Self::RetryScheduled),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            other => Err(AppError::Internal(format!(
                "unknown ingestion job status '{other}' in database"
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// Failures
// ---------------------------------------------------------------------------

/// Whether trying again can help.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// Retrying will give the same result. The job fails at once.
    Permanent,
    /// May succeed later. The job is retried while attempts remain.
    Transient,
}

/// The closed set of failure codes (plan D13). Stored in
/// `documents.failure_code` and `ingestion_jobs.last_error_code`, and shown to
/// users, so the strings are an API: never build one from provider or parser
/// output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureCode {
    ParseFailed,
    DocumentEmpty,
    DocumentTooLarge,
    UnsupportedFormat,
    ObjectMissing,
    ObjectCorrupt,
    EmbeddingRejected,
    EmbeddingInvalid,
    StorageUnavailable,
    EmbeddingUnavailable,
    DatabaseUnavailable,
    ParseTimeout,
    AttemptsExhausted,
    Internal,
}

impl FailureCode {
    pub const ALL: [FailureCode; 14] = [
        Self::ParseFailed,
        Self::DocumentEmpty,
        Self::DocumentTooLarge,
        Self::UnsupportedFormat,
        Self::ObjectMissing,
        Self::ObjectCorrupt,
        Self::EmbeddingRejected,
        Self::EmbeddingInvalid,
        Self::StorageUnavailable,
        Self::EmbeddingUnavailable,
        Self::DatabaseUnavailable,
        Self::ParseTimeout,
        Self::AttemptsExhausted,
        Self::Internal,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ParseFailed => "PARSE_FAILED",
            Self::DocumentEmpty => "DOCUMENT_EMPTY",
            Self::DocumentTooLarge => "DOCUMENT_TOO_LARGE",
            Self::UnsupportedFormat => "UNSUPPORTED_FORMAT",
            Self::ObjectMissing => "OBJECT_MISSING",
            Self::ObjectCorrupt => "OBJECT_CORRUPT",
            Self::EmbeddingRejected => "EMBEDDING_REJECTED",
            Self::EmbeddingInvalid => "EMBEDDING_INVALID_RESPONSE",
            Self::StorageUnavailable => "STORAGE_UNAVAILABLE",
            Self::EmbeddingUnavailable => "EMBEDDING_UNAVAILABLE",
            Self::DatabaseUnavailable => "DATABASE_UNAVAILABLE",
            Self::ParseTimeout => "PARSE_TIMEOUT",
            Self::AttemptsExhausted => "ATTEMPTS_EXHAUSTED",
            Self::Internal => "INTERNAL_ERROR",
        }
    }

    pub fn kind(self) -> FailureKind {
        match self {
            Self::StorageUnavailable
            | Self::EmbeddingUnavailable
            | Self::DatabaseUnavailable
            | Self::ParseTimeout
            | Self::Internal => FailureKind::Transient,
            Self::ParseFailed
            | Self::DocumentEmpty
            | Self::DocumentTooLarge
            | Self::UnsupportedFormat
            | Self::ObjectMissing
            | Self::ObjectCorrupt
            | Self::EmbeddingRejected
            | Self::EmbeddingInvalid
            | Self::AttemptsExhausted => FailureKind::Permanent,
        }
    }
}

// ---------------------------------------------------------------------------
// Backoff
// ---------------------------------------------------------------------------

/// Exponential backoff with "equal jitter": half fixed, half random.
///
/// `attempt` 1 gives `base`, 2 gives `2 * base`, and so on, capped at `max`.
/// The result is in `[d / 2, d]` for that `d`. `rand01` is injected (0.0..=1.0,
/// clamped) so tests are deterministic. Attempt 0 is treated as 1.
pub fn retry_delay(attempt: u32, base: Duration, max: Duration, rand01: f64) -> Duration {
    let exponent = attempt.saturating_sub(1).min(32) as i32;
    let ceiling = (base.as_secs_f64() * 2f64.powi(exponent)).min(max.as_secs_f64());
    let rand01 = if rand01.is_nan() {
        0.0
    } else {
        rand01.clamp(0.0, 1.0)
    };
    Duration::from_secs_f64(ceiling / 2.0 + (ceiling / 2.0) * rand01)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_STATUSES: [JobStatus; 6] = [
        JobStatus::Pending,
        JobStatus::Processing,
        JobStatus::RetryScheduled,
        JobStatus::Succeeded,
        JobStatus::Failed,
        JobStatus::Cancelled,
    ];

    #[test]
    fn job_status_round_trips() {
        for &s in JobStatus::all_str() {
            assert_eq!(s.parse::<JobStatus>().unwrap().as_str(), s);
        }
        assert!("bogus".parse::<JobStatus>().is_err());
        assert_eq!(ALL_STATUSES.len(), JobStatus::all_str().len());
    }

    #[test]
    fn job_status_matches_sql_check_constraint() {
        let sql = include_str!("../../migrations/004_ingestion.sql");
        let table = sql
            .split("CREATE TABLE ingestion_jobs")
            .nth(1)
            .expect("ingestion_jobs table");
        let check = table
            .split("CHECK (status IN (")
            .nth(1)
            .expect("status CHECK")
            .split("))")
            .next()
            .unwrap();
        let values: Vec<&str> = check
            .split('\'')
            .enumerate()
            .filter(|(i, _)| i % 2 == 1)
            .map(|(_, v)| v)
            .collect();
        assert_eq!(
            JobStatus::all_str(),
            values.as_slice(),
            "JobStatus does not match the SQL CHECK constraint, update both together"
        );
    }

    #[test]
    fn job_status_transition_table_matches_the_state_machine() {
        use JobStatus::*;
        let allowed = [
            (Pending, Processing),
            (Pending, Cancelled),
            (Processing, Succeeded),
            (Processing, Failed),
            (Processing, RetryScheduled),
            (Processing, Pending),
            (Processing, Cancelled),
            (RetryScheduled, Pending),
            (RetryScheduled, Cancelled),
        ];
        for from in ALL_STATUSES {
            for to in ALL_STATUSES {
                assert_eq!(
                    from.can_transition_to(to),
                    allowed.contains(&(from, to)),
                    "{from:?} -> {to:?}"
                );
            }
        }
    }

    #[test]
    fn terminal_statuses_are_not_active_and_have_no_exits() {
        for s in ALL_STATUSES {
            let has_exit = ALL_STATUSES.iter().any(|&t| s.can_transition_to(t));
            assert_eq!(s.is_active(), has_exit, "{s:?}");
        }
    }

    #[test]
    fn failure_codes_match_the_column_regex_and_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for code in FailureCode::ALL {
            let s = code.as_str();
            assert!((3..=64).contains(&s.len()), "{s}");
            let mut chars = s.chars();
            assert!(chars.next().unwrap().is_ascii_uppercase(), "{s}");
            assert!(
                chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'),
                "{s}"
            );
            assert!(seen.insert(s), "duplicate {s}");
        }
    }

    #[test]
    fn failure_kinds_follow_the_plan() {
        assert_eq!(FailureCode::ParseFailed.kind(), FailureKind::Permanent);
        assert_eq!(FailureCode::ObjectCorrupt.kind(), FailureKind::Permanent);
        assert_eq!(
            FailureCode::AttemptsExhausted.kind(),
            FailureKind::Permanent
        );
        assert_eq!(FailureCode::ParseTimeout.kind(), FailureKind::Transient);
        assert_eq!(
            FailureCode::EmbeddingUnavailable.kind(),
            FailureKind::Transient
        );
        assert_eq!(FailureCode::Internal.kind(), FailureKind::Transient);
    }

    #[test]
    fn retry_delay_is_within_jitter_band_monotonic_and_capped() {
        let base = Duration::from_secs(5);
        let max = Duration::from_secs(600);
        let mut previous = [Duration::ZERO; 3];
        for attempt in 1..=10u32 {
            let ceiling = (base * 2u32.pow(attempt - 1)).min(max);
            for (slot, rand01) in [0.0, 0.5, 1.0].into_iter().enumerate() {
                let d = retry_delay(attempt, base, max, rand01);
                assert!(d >= ceiling / 2, "attempt {attempt} rand {rand01}: {d:?}");
                assert!(d <= ceiling, "attempt {attempt} rand {rand01}: {d:?}");
                assert!(d <= max);
                assert!(d >= previous[slot], "not monotonic at attempt {attempt}");
                previous[slot] = d;
            }
        }
    }

    #[test]
    fn retry_delay_first_attempt_is_base_at_full_jitter() {
        let base = Duration::from_secs(5);
        assert_eq!(retry_delay(1, base, Duration::from_secs(600), 1.0), base);
    }

    #[test]
    fn retry_delay_survives_huge_attempts_and_bad_randomness() {
        let max = Duration::from_secs(600);
        let base = Duration::from_secs(5);
        assert!(retry_delay(u32::MAX, base, max, 1.0) <= max);
        assert_eq!(retry_delay(0, base, max, 1.0), base);
        assert!(retry_delay(3, base, max, f64::NAN) >= base * 2);
        assert!(retry_delay(3, base, max, 7.0) <= base * 4);
    }
}
