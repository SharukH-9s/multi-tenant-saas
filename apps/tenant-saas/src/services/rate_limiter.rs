//! Token-bucket rate limiter backed by Redis.
//!
//! Each `check()` call runs a Lua script **atomically** on Redis that:
//!  - Replenishes tokens proportionally to elapsed time (smooth refill, no burst at window boundary).
//!  - Checks the IP bucket first; if empty, returns 429 without decrementing the (IP+email) bucket
//!    (victim DoS protection: the email bucket is not consumed for the attacker's IP).
//!  - Checks the (IP+email) bucket only when an email is provided.
//!
//! When Redis is unavailable the circuit breaker trips and calls are **fail-open**
//! (returns `Ok(())` immediately with a WARN log) to prevent availability loss.
//!
//! Key format:
//!   `rl:v1:{scope}:ip:{ip}`
//!   `rl:v1:{scope}:ip_email:{ip}:{sha256_hex(lower(trim(email)))}`
//! Email is never stored raw in a key or a log.

use std::{fmt::Write, time::Duration};

use common_config::AuthConfig;
use common_errors::AppError;
use redis::{aio::ConnectionManager, aio::ConnectionManagerConfig, Script};
use sha2::{Digest, Sha256};

use super::circuit_breaker::CircuitBreaker;

/// Maximum time to wait for a Redis command before treating it as a failure.
const REDIS_TIMEOUT_MS: u64 = 150;

/// Token-bucket Lua script (runs atomically via EVALSHA).
///
/// KEYS[1] = ip_key
/// KEYS[2] = ip_email_key  (ignored when ARGV[4]=="0")
/// ARGV[1] = window_secs   (u64)
/// ARGV[2] = ip_capacity   (u32)
/// ARGV[3] = email_capacity (u32, ignored when ARGV[4]=="0")
/// ARGV[4] = has_email_key  ("1" or "0")
///
/// Returns {bucket_index, retry_after_secs}
///   bucket_index 0 = OK
///   bucket_index 1 = IP bucket depleted
///   bucket_index 2 = IP+email bucket depleted
const TOKEN_BUCKET_SCRIPT: &str = r#"
local window    = tonumber(ARGV[1])
local ip_cap    = tonumber(ARGV[2])
local em_cap    = tonumber(ARGV[3])
local has_email = (ARGV[4] == "1")

local t         = redis.call('TIME')
local now       = tonumber(t[1]) + (tonumber(t[2]) / 1000000)

local function check_bucket(key, capacity)
  local refill_rate = capacity / window
  local data        = redis.call('HMGET', key, 'tokens', 'ts')
  local tokens      = tonumber(data[1])
  local ts          = tonumber(data[2])

  if not tokens then
    -- First request: start full minus this one request.
    tokens = capacity - 1.0
    ts     = now
    redis.call('HMSET', key, 'tokens', tokens, 'ts', ts)
    redis.call('EXPIRE', key, math.ceil(window * 2))
    return 0, 0
  end

  local delta = math.max(0, now - ts)
  tokens      = math.min(capacity, tokens + delta * refill_rate)
  ts          = now

  if tokens < 1.0 then
    local retry = math.ceil((1.0 - tokens) / refill_rate)
    return -1, math.max(1, retry)
  end

  tokens = tokens - 1.0
  redis.call('HMSET', key, 'tokens', tokens, 'ts', ts)
  redis.call('EXPIRE', key, math.ceil(window * 2))
  return 0, 0
end

local ok1, r1 = check_bucket(KEYS[1], ip_cap)
if ok1 ~= 0 then
  return {1, r1}
end

if has_email then
  local ok2, r2 = check_bucket(KEYS[2], em_cap)
  if ok2 ~= 0 then
    return {2, r2}
  end
end

return {0, 0}
"#;

/// Thread-safe token-bucket rate limiter.
///
/// Cheap to clone: the `ConnectionManager` and `CircuitBreaker` are Arc-backed.
#[derive(Clone)]
pub struct RateLimiter {
    manager: Option<ConnectionManager>,
    breaker: CircuitBreaker,
    window_secs: u64,
    per_ip: u32,
    per_email: u32,
}

impl RateLimiter {
    /// Creates a limiter that connects lazily to `redis_url` with a 150 ms timeout.
    ///
    /// Because the connection is lazy, this function returns synchronously and never blocks.
    /// If Redis is not currently running, commands will attempt to connect, fail/timeout
    /// in 150 ms, and fail open without taking down the server.
    pub fn new(redis_url: &str, cfg: &AuthConfig) -> Self {
        let manager = match redis::Client::open(redis_url) {
            Ok(client) => {
                let config = ConnectionManagerConfig::new()
                    .set_connection_timeout(Some(Duration::from_millis(REDIS_TIMEOUT_MS)))
                    .set_response_timeout(Some(Duration::from_millis(REDIS_TIMEOUT_MS)))
                    .set_number_of_retries(0);
                match ConnectionManager::new_lazy_with_config(client, config) {
                    Ok(m) => Some(m),
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "RateLimiter: could not initialize lazy Redis connection; starting fail-open"
                        );
                        None
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "RateLimiter: invalid Redis URL; starting fail-open");
                None
            }
        };

        Self {
            manager,
            breaker: CircuitBreaker::new(),
            window_secs: cfg.rate_limit_window_secs,
            per_ip: cfg.rate_limit_per_ip,
            per_email: cfg.rate_limit_per_email,
        }
    }

    /// Creates a limiter with no Redis connection (always fail-open).
    pub fn disconnected(cfg: &AuthConfig) -> Self {
        Self {
            manager: None,
            breaker: CircuitBreaker::new(),
            window_secs: cfg.rate_limit_window_secs,
            per_ip: cfg.rate_limit_per_ip,
            per_email: cfg.rate_limit_per_email,
        }
    }

    /// Checks the rate limit for an incoming request.
    ///
    /// - `scope`  — one of `"login"`, `"register"`, `"refresh"`. Each scope has independent counters.
    /// - `ip`     — normalized client IP string from `ClientIp`. Never empty.
    /// - `email`  — optional email (hashed internally, never stored raw).
    ///
    /// Returns `Ok(())` to allow the request, or `Err(AppError::TooManyRequests)` to block it.
    ///
    /// **Fail-open**: when Redis is unavailable or the circuit breaker is open, logs a WARN
    /// and returns `Ok(())` so legitimate users are never blocked by a Redis outage.
    pub async fn check(&self, scope: &str, ip: &str, email: Option<&str>) -> Result<(), AppError> {
        // Fast path: circuit is open.
        if self.breaker.is_open() {
            tracing::warn!(
                scope,
                ip,
                event = "auth.rate_limit_skipped",
                reason = "redis_circuit_open",
            );
            return Ok(());
        }

        let Some(_) = self.manager.as_ref() else {
            tracing::warn!(
                scope,
                ip,
                event = "auth.rate_limit_skipped",
                reason = "redis_unavailable",
            );
            return Ok(());
        };

        let ip_key = format!("rl:v1:{scope}:ip:{ip}");
        let email_key = email
            .map(|e| format!("rl:v1:{scope}:ip_email:{ip}:{}", hash_email(e)))
            .unwrap_or_default();
        let has_email = email.is_some();

        let result = tokio::time::timeout(
            Duration::from_millis(REDIS_TIMEOUT_MS),
            self.run_script(&ip_key, &email_key, has_email),
        )
        .await;

        match result {
            Ok(Ok((bucket, retry_after))) => {
                self.breaker.record_success();
                if bucket == 0 {
                    Ok(())
                } else {
                    Err(AppError::TooManyRequests {
                        retry_after_secs: retry_after,
                    })
                }
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, scope, ip, event = "auth.rate_limit_redis_error");
                self.breaker.record_failure();
                Ok(()) // fail open
            }
            Err(_timeout) => {
                tracing::warn!(scope, ip, event = "auth.rate_limit_redis_timeout");
                self.breaker.record_failure();
                Ok(()) // fail open
            }
        }
    }

    async fn run_script(
        &self,
        ip_key: &str,
        email_key: &str,
        has_email: bool,
    ) -> Result<(i64, u64), redis::RedisError> {
        let mut conn = self.manager.as_ref().unwrap().clone();
        let result: (i64, i64) = Script::new(TOKEN_BUCKET_SCRIPT)
            .key(ip_key)
            .key(email_key)
            .arg(self.window_secs)
            .arg(self.per_ip)
            .arg(self.per_email)
            .arg(if has_email { "1" } else { "0" })
            .invoke_async(&mut conn)
            .await?;
        Ok((result.0, result.1.max(0) as u64))
    }

    /// Returns whether the circuit breaker is currently open.
    pub fn is_circuit_open(&self) -> bool {
        self.breaker.is_open()
    }

    /// Access the underlying circuit breaker.
    pub fn circuit_breaker(&self) -> &CircuitBreaker {
        &self.breaker
    }
}

/// SHA-256 hex of the lowercased, trimmed email (never the raw email).
fn hash_email(email: &str) -> String {
    let hash: [u8; 32] = Sha256::digest(email.trim().to_lowercase().as_bytes()).into();
    let mut out = String::with_capacity(64);
    for b in hash {
        let _ = write!(out, "{:02x}", b);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_email_is_case_insensitive_and_trimmed() {
        assert_eq!(
            hash_email("User@Example.COM"),
            hash_email("user@example.com")
        );
        assert_eq!(
            hash_email("  user@example.com  "),
            hash_email("user@example.com")
        );
    }

    #[test]
    fn hash_email_contains_no_raw_email_parts() {
        let h = hash_email("victim@example.com");
        assert!(!h.contains('@'), "hash must not contain @");
        assert!(!h.contains("victim"), "hash must not contain local part");
        assert!(!h.contains("example"), "hash must not contain domain");
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    fn test_auth_config() -> AuthConfig {
        AuthConfig {
            jwt_secret: "12345678901234567890123456789012".into(),
            jwt_issuer: "test".into(),
            jwt_audience: "test".into(),
            jwt_expiry_secs: 900,
            refresh_token_ttl_secs: 1000,
            argon2_memory_kib: 19456,
            argon2_iterations: 2,
            argon2_parallelism: 1,
            max_concurrent_hashes: 4,
            rate_limit_window_secs: 60,
            rate_limit_per_ip: 2,
            rate_limit_per_email: 2,
            trusted_proxy_hops: 0,
        }
    }

    #[tokio::test]
    async fn disconnected_limiter_always_allows() {
        let cfg = test_auth_config();
        let limiter = RateLimiter::disconnected(&cfg);
        for _ in 0..10 {
            assert!(limiter
                .check("login", "127.0.0.1", Some("user@example.com"))
                .await
                .is_ok());
        }
    }

    #[tokio::test]
    async fn dead_redis_fails_open_and_trips_circuit_breaker() {
        let cfg = test_auth_config();
        // Point to an unreachable port
        let limiter = RateLimiter::new("redis://127.0.0.1:1", &cfg);
        assert!(!limiter.is_circuit_open());

        for _ in 0..3 {
            let res = limiter
                .check("login", "127.0.0.1", Some("user@example.com"))
                .await;
            assert!(res.is_ok(), "must fail open on Redis error");
        }

        assert!(
            limiter.is_circuit_open(),
            "circuit breaker must trip after 3 failures"
        );

        // While open, subsequent checks return Ok immediately without attempting Redis
        assert!(limiter
            .check("login", "127.0.0.1", Some("user@example.com"))
            .await
            .is_ok());
    }
}
