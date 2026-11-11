# Task 9 — Auth abuse controls (rate limiting): working plan

Source: `phase2_implementation_plan.md` Task 9, design §14.1 ("rate-limit login attempts") and §15
("document the algorithm, test under concurrent replicas, define behavior when Redis is down").
Status: **Completed** (Token Bucket in Redis via atomic Lua + in-memory atomic Circuit Breaker with fail-open + Client IP resolution with trusted proxy hops + comprehensive unit and integration tests).

## 1. Goal

Stop password guessing and registration/refresh floods **before** they reach Argon2 or the database,
without introducing victim account-lockout Denial-of-Service and without blocking logins when Redis is down.

| Endpoint | IP Bucket | (IP + Email) Bucket |
|---|---|---|
| `POST /auth/login` | yes | yes |
| `POST /auth/register` | yes | yes |
| `POST /auth/refresh` | yes | no (no email in the request) |
| `logout`, `me` | not limited (they need a valid token; per-user limits are Phase 7) | |

Defaults in config:
- Window: 60 s (`rate_limit_window_secs = 60`)
- IP capacity: 10 (`rate_limit_per_ip = 10`, refill 10 per 60 s = 1 token / 6 s)
- (IP + Email) capacity: 5 (`rate_limit_per_email = 5`, refill 5 per 60 s = 1 token / 12 s)

Each endpoint has its **own** counters, so exhausting login does not block register.

Exceeded -> `429 RATE_LIMITED` with `Retry-After`.
Redis unavailable -> **Circuit breaker trips, fail open** (logs `WARN`, skips Redis, lets request through).

## 2. What I found in the repo (affects the plan)

| Finding | Consequence |
|---|---|
| `AuthConfig` already has `rate_limit_window_secs`, `rate_limit_per_ip`, `rate_limit_per_email` (+ validation). `REDIS_URL` exists. | No new limit settings needed, only one new one (proxy hops, section 4). |
| `AppState` has **no Redis** yet. | Add a `RateLimiter` to `AppState` without changing `AppState::new`'s signature. |
| Workspace `redis` has `default-features = false`, features `tokio-comp` + `connection-manager`. The **`script` feature is not enabled**, so `redis::Script` does not exist. | Add `script` to the workspace dependency (tiny: one extra crate, `sha1_smol`). Fallback: raw `EVAL` via `redis::cmd`, no feature needed. |
| `main.rs` serves with plain `axum::serve(listener, router)`: **no `ConnectInfo`**, so the client IP is unavailable today. | Switch to `into_make_service_with_connect_info::<SocketAddr>()`. |
| Tests call the router with `oneshot` (no socket, no IP). All 30 integration tests would share one bucket. | Test harness change (section 7). Random IP per `TestApp` + hops=1. |
| `.env.example` has no `AUTH_RATE_LIMIT_*` keys and still says `JWT_EXPIRY_SECS=3600` (code default is 900). | Add the keys. The 3600 value is a leftover from Task 1 item 6; fix it while editing that block (one line). |
| `/health/ready` checks only PostgreSQL. | Leave unchanged (Task 1 says health behaves exactly as before). Redis is not added to readiness. |
| Redis in Compose has no password; `redis:7-alpine`. | Local and CI tests work with the default URL. |

## 3. Algorithm: Atomic Token Bucket & Dual-Keying

**Token Bucket in Redis.** One Lua script, run atomically on the server.
Using Redis server time (`TIME`), the script replenishes tokens proportionally and consumes 1 token per bucket.

```lua
-- KEYS = { ip_key, [ip_email_key] }
-- ARGV[1] = now_sec, ARGV[2] = now_usec, ARGV[3] = window_secs
-- ARGV[4..] = pairs of (capacity, refill_rate_per_sec) for each key
local now = tonumber(ARGV[1]) + (tonumber(ARGV[2]) / 1000000)
local window = tonumber(ARGV[3])

for i, key in ipairs(KEYS) do
  local capacity = tonumber(ARGV[2 + i * 2])
  local refill_rate = capacity / window

  local data = redis.call('HMGET', key, 'tokens', 'last_updated')
  local tokens = tonumber(data[1])
  local last_updated = tonumber(data[2])

  if not tokens then
    tokens = capacity
    last_updated = now
  else
    local delta = math.max(0, now - last_updated)
    tokens = math.min(capacity, tokens + delta * refill_rate)
    last_updated = now
  end

  if tokens < 1.0 then
    local retry_after = math.ceil((1.0 - tokens) / refill_rate)
    return {i, math.max(1, retry_after)}
  end

  tokens = tokens - 1.0
  redis.call('HMSET', key, 'tokens', tokens, 'last_updated', last_updated)
  redis.call('EXPIRE', key, math.ceil(window * 2))
end

return {0, 0}
```

Why this shape:
- **No boundary burst**: Unlike fixed-window counters, token bucket refills smoothly.
- **Dual keying**:
  1. `rl:v1:{scope}:ip:{ip}` (protects against single-IP password spraying and Argon2 DoS).
  2. `rl:v1:{scope}:ip_email:{ip}:{sha256(email)}` (protects individual accounts from being brute-forced by that IP).
- **Victim DoS Protection**: If an attacker spams invalid passwords for `victim@example.com` from IP `198.51.100.1`, only `(198.51.100.1, victim)` is limited. The real victim logging in from their home/office IP is **not locked out**.
- **Atomic**: Keys are evaluated in priority order. If the IP bucket is empty, the `(IP + Email)` bucket is not decremented.
- **One round trip**: Atomically checks all buckets in single EVALSHA.

`Retry-After` = ceil((1 - tokens) / refill_rate), minimum 1 second.

## 4. Fail-Safe: Circuit Breaker for Redis

To satisfy "if redis is down, it should directly skip redis" without causing 5-second HTTP request lags:

1. **Tight Timeouts**:
   - Connection & command timeout to Redis is bounded at **150 ms**.
2. **Circuit Breaker State Machine**:
   - `Closed`: Normal operation. All checks execute against Redis.
   - 3 consecutive Redis errors / timeouts transition state to `Open`.
   - `Open`: Fast-fail Redis checks for **30 seconds** (cooldown period).
     - In `Open` state, `limiter.check(...)` immediately returns `Ok(())` (0 ms overhead).
     - Emits `tracing::warn!(event = "auth.rate_limit_skipped", reason = "redis_circuit_open")`.
   - `Half-Open`: After 30 seconds, 1 request attempts Redis. If successful, state resets to `Closed` (0 consecutive failures). If it fails, resets 30 s cooldown in `Open`.
3. **Outcome**: Legitimate users can always log in and register during a Redis outage with zero request latency degradation.

## 5. Client IP

- Config: `AUTH_RATE_LIMIT_TRUSTED_PROXY_HOPS` (default **0**), validated (0..=10).
  - `0`: use the TCP peer address. `X-Forwarded-For` is **ignored** (a client can send anything).
  - `N > 0`: the app sits behind N proxies you control. Use the N-th entry **from the right** of `X-Forwarded-For`
    (the right side is appended by your proxies; the left side is client-controlled).
- IPv6: key by the **/64 prefix** (an attacker normally owns a whole /64 and could rotate within it).
- If no address can be determined: use one shared `unknown` bucket. Strict, never permissive.

Implementation: `auth/client_ip.rs` with a pure function `resolve_client_ip(peer, forwarded_for, hops) -> String` plus a
small `ClientIp` extractor around it.

## 6. Components

```text
crates/common-config      + trusted_proxy_hops (+ test, validation)
Cargo.toml (workspace)    redis features += "script"
services/circuit_breaker.rs In-memory atomic circuit breaker
services/rate_limiter.rs  RateLimiter: Redis client, lazy connection, Token Bucket script, breaker
auth/client_ip.rs         resolve_client_ip() + ClientIp extractor
app_state.rs              + rate_limiter: Arc<RateLimiter>
handlers/auth.rs          3 call sites (login, register, refresh)
main.rs                   serve with ConnectInfo<SocketAddr>
.env.example, README      new key(s), proxy-hops note, runbook line
```

Key format:
- `rl:v1:{scope}:ip:{ip}`
- `rl:v1:{scope}:ip_email:{ip}:{sha256_hex(normalized_email)}`

Never a raw email in a key or a log. An email that fails parsing is counted by IP only.

### Where `check()` is called

In the handler, right after the JSON body is parsed and before the service call:
```text
parse JSON -> ClientIp -> limiter.check(...) -> auth_service::{register,login,refresh}
```

So a rate-limited request never reaches Argon2, the semaphore, or a database transaction.

## 7. Test Plan

### 7.1 Test Harness Changes
- Test config sets `AUTH_RATE_LIMIT_TRUSTED_PROXY_HOPS=1` and high default limits (10,000) so existing tests never trip.
- `TestApp::request` sends a random `X-Forwarded-For` per test instance.
- `TestApp::spawn_with(options)` for tests that test custom limits or broken Redis ports.

### 7.2 Unit Tests
- `resolve_client_ip`: hops 0 ignores header; hops 1 & 2 pick from right; IPv6 /64 prefix; invalid fallback.
- `circuit_breaker`: transitions `Closed -> Open` after 3 failures, stays `Open` during cooldown, transitions `Half-Open -> Closed` on success.
- `rate_limiter` key hashing: case-folding, no `@` or email plain text in keys.

### 7.3 Integration Tests (`tests/integration/rate_limit.rs`)
1. **IP Token Bucket limit**: Exceeding IP capacity returns `429 RATE_LIMITED` with `Retry-After`.
2. **(IP + Email) Token Bucket limit**: Exceeding attempts on an email from one IP returns 429 for that IP+Email.
3. **No Victim Lockout**: Attacker IP exhausts `(Attacker IP + victim@example.com)`, but Victim IP can still immediately log in as `victim@example.com`.
4. **Endpoint isolation**: Exhausting login bucket does not exhaust register or refresh buckets.
5. **Token refill**: After token capacity is drained, waiting the refill interval grants tokens again.
6. **Redis Outage / Fail Open (Circuit Breaker)**: When Redis points to a dead port:
   - Request does NOT return 503.
   - Request succeeds (returns 200/401/etc. as normal).
   - Fast response time (<200 ms).
   - Circuit breaker logs warning and trips to open.
7. **Concurrency & Atomicity**: Concurrent requests do not exceed bucket capacity.
