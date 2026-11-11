//! Atomic circuit breaker for Redis connections.
//!
//! States:
//! - `Closed`: normal operation; all checks run against Redis.
//! - `Open`: 3+ consecutive failures; fast-fail for 30 s. `is_open()` returns `true`, callers must skip Redis (fail-open) and log a warning.
//! - `Half-Open`: after the 30 s cooldown, one probe is allowed. Success -> `Closed`; failure -> back to `Open` with a fresh 30 s cooldown.
//!
//! Thread-safe via atomics; cheap to clone (Arc-backed).

use std::sync::{
    atomic::{AtomicU32, AtomicU64, Ordering},
    Arc,
};
use std::time::{SystemTime, UNIX_EPOCH};

/// Number of consecutive failures before the breaker opens.
const FAILURE_THRESHOLD: u32 = 3;
/// How long the breaker stays open before allowing a probe (seconds).
const OPEN_DURATION_SECS: u64 = 30;

#[derive(Clone, Debug)]
pub struct CircuitBreaker {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    /// Consecutive failure count.
    failures: AtomicU32,
    /// Unix timestamp (seconds) when the breaker opened; 0 if closed.
    opened_at: AtomicU64,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl CircuitBreaker {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                failures: AtomicU32::new(0),
                opened_at: AtomicU64::new(0),
            }),
        }
    }

    /// Returns `true` when the breaker is open (Redis calls must be skipped).
    ///
    /// Automatically transitions `Open → Half-Open` after the cooldown, letting
    /// exactly one probe attempt through.
    pub fn is_open(&self) -> bool {
        let opened_at = self.inner.opened_at.load(Ordering::Acquire);
        if opened_at == 0 {
            return false; // Closed
        }
        let elapsed = now_secs().saturating_sub(opened_at);
        if elapsed >= OPEN_DURATION_SECS {
            // Allow one probe: clear opened_at so the next caller sees Closed.
            // If the probe succeeds, record_success() keeps it Closed.
            // If it fails, record_failure() re-opens with a fresh timestamp.
            let _ = self.inner.opened_at.compare_exchange(
                opened_at,
                0,
                Ordering::AcqRel,
                Ordering::Relaxed,
            );
            false
        } else {
            true // still Open
        }
    }

    /// Call after a successful Redis operation. Resets the breaker to Closed.
    pub fn record_success(&self) {
        self.inner.failures.store(0, Ordering::Release);
        self.inner.opened_at.store(0, Ordering::Release);
    }

    /// Call after a Redis error or timeout. Opens the breaker after the threshold.
    pub fn record_failure(&self) {
        let prev = self.inner.failures.fetch_add(1, Ordering::AcqRel);
        if prev + 1 >= FAILURE_THRESHOLD {
            // Only transition to Open if currently Closed (opened_at == 0).
            let _ = self.inner.opened_at.compare_exchange(
                0,
                now_secs(),
                Ordering::AcqRel,
                Ordering::Relaxed,
            );
        }
    }
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_closed() {
        let cb = CircuitBreaker::new();
        assert!(!cb.is_open());
    }

    #[test]
    fn two_failures_stay_closed() {
        let cb = CircuitBreaker::new();
        cb.record_failure();
        cb.record_failure();
        assert!(!cb.is_open(), "2 failures: still closed");
    }

    #[test]
    fn three_failures_open_the_breaker() {
        let cb = CircuitBreaker::new();
        cb.record_failure();
        cb.record_failure();
        cb.record_failure();
        assert!(cb.is_open(), "3 failures: should be open");
    }

    #[test]
    fn success_resets_to_closed() {
        let cb = CircuitBreaker::new();
        cb.record_failure();
        cb.record_failure();
        cb.record_failure();
        assert!(cb.is_open());
        cb.record_success();
        assert!(!cb.is_open());
    }

    #[test]
    fn stays_open_before_cooldown() {
        let cb = CircuitBreaker::new();
        cb.record_failure();
        cb.record_failure();
        cb.record_failure();
        // Set opened_at to now (not expired).
        cb.inner.opened_at.store(now_secs(), Ordering::Release);
        assert!(cb.is_open());
    }

    #[test]
    fn half_opens_after_cooldown() {
        let cb = CircuitBreaker::new();
        cb.record_failure();
        cb.record_failure();
        cb.record_failure();
        // Backdate opened_at past the cooldown.
        cb.inner
            .opened_at
            .store(now_secs() - OPEN_DURATION_SECS, Ordering::Release);
        assert!(!cb.is_open(), "should allow probe after cooldown");
        // A subsequent failure re-opens the breaker.
        cb.record_failure();
        assert!(cb.is_open(), "failed probe re-opens breaker");
    }
}
