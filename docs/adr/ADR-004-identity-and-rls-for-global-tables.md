# ADR-004: Identity and RLS for Global Tables

**Status:** Accepted

## Context
Phase 1 established multi-tenancy using PostgreSQL Row-Level Security (RLS) on shared tables. However, identity and tenant discovery require tables that do not belong strictly to a single tenant:
- `users`: Global accounts that can join multiple tenants.
- `tenants`: Root tenant records.
- `memberships`: The cross-product connecting users to tenants with specific roles.
- `refresh_tokens`: Global session credentials.

In addition, authentication requires robust abuse controls (rate limiting) without introducing a single point of failure that can cause availability loss for legitimate users.

## Decision

### 1. Local Authentication & Stateless JWTs
- **Password Hashing:** Argon2id with memory cost, iterations, and parallelism tuned to OWASP standards, wrapped in a concurrency-limiting semaphore to prevent async runtime starvation.
- **Access Tokens:** Signed HS256 JWTs with a short TTL (900s / 15 minutes). Crucially, **JWTs carry identity (`sub: user_id`) only — never tenant or role claims**.
- **Per-Request Authorization:** Tenant membership, roles, and status are verified dynamically from the database on every tenant-scoped request via `TenantCtx`. If a membership is revoked, access terminates immediately on the next request.

### 2. Global Table RLS Strategy & Dual GUCs
Transactions utilize two transaction-local GUC settings (`set_config(..., true)`):
- `app.current_user_id`: Scopes queries to the authenticated user's own data (`begin_user_tx`).
- `app.current_tenant_id`: Scopes queries to the active tenant (`begin_tenant_tx`).

Table-by-table RLS design:
- `tenants`: `ENABLE` and `FORCE ROW LEVEL SECURITY`. Accessible if `id = app_tenant_id()` OR user has an active or invited membership (allowing invited users to inspect tenant name/slug for pending invitations).
- `memberships`: `ENABLE` and `FORCE ROW LEVEL SECURITY`. Accessible if `tenant_id = app_tenant_id()` OR `user_id = app_user_id()`. No policies reference `tenants` directly, preventing recursion.
- `users`: `ENABLE ROW LEVEL SECURITY` (without `FORCE`). `SELECT` and `UPDATE` allowed only for `id = app_user_id()`. Non-FORCE allows SECURITY DEFINER functions owned by `db_owner` to look up credentials and member directories while preventing `app_runtime` from directly scanning table rows.
- `refresh_tokens`: Not tenant data. Looked up strictly by SHA-256 hash of a 256-bit random secret. Rotation is atomic, and reuse of a previously rotated token revokes the entire token family.

### 3. Narrow SECURITY DEFINER Functions
Owned by `db_owner`, pinned `search_path = pg_catalog, public`, revoked from `PUBLIC`, and granted strictly to `app_runtime`:
- `auth_get_credentials(email)`: The only path to retrieve `password_hash`.
- `tenant_member_directory()`: Reads `app.current_tenant_id` internally without taking a tenant parameter, preventing cross-tenant exfiltration.
- `find_user_id_by_email(email)`: Safe user discovery for invitations without enumerating account data.

### 4. Abuse Controls: Token-Bucket Rate Limiter & Fail-Open Circuit Breaker
- Backed by Redis with atomic Lua replenishment across both IP and (IP + email) dimensions.
- **Fail-Open Policy:** When Redis is unreachable or times out (150 ms), the rate limiter fails open and trips an internal circuit breaker, ensuring that Redis outages never take down user authentication.

### 5. Invariant & Escalation Safeguards
- **Last Active Owner:** Guaranteed by the `memberships_protect_last_owner` BEFORE UPDATE/DELETE trigger and service checks.
- **Escalation Rules:** Admins cannot promote anyone to `owner`, cannot modify or remove an `Owner`, and cannot alter critical tenant settings (`slug`, `plan`, `status`).

## Consequences

### Positive
- Strict isolation: Tenant A cannot read or write Tenant B's data at both the application and database (RLS) layers.
- Membership revocations and role updates take effect immediately on subsequent requests.
- No single point of failure from Redis outages.

### Negative / Trade-offs
- Reading membership on every tenant-scoped request incurs a fast indexed DB join. (Redis caching can be added in Phase 7 if profiling requires it).
- Access tokens cannot be revoked before their 15-minute expiration; disabled users are blocked on the next request because membership resolution and `/auth/me` re-validate `users.status`.

## Revisit When
Phase 7 (Platform Hardening) for JWT key rotation (`kid`), OIDC SSO integration, or multi-region read replicas.

