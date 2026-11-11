# Task 7 — Tenant context resolution: working plan

Source: `phase2_implementation_plan.md` Task 7 and decisions D2, D3, D7.
Builds on Task 6 (`AuthUser` extractor, `begin_user_tx`) and the RLS policies in `002_identity.sql`.

## Goal

Turn "a signed-in user + a tenant id from the URL" into a trusted `TenantContext`, or a safe error.
The tenant id from the URL is only a hint. The database decides whether the user belongs.

```text
request -> AuthUser (401) -> tenant id from path (404 if not a UUID)
        -> resolve(): one read in a user-scoped tx, tx released
        -> decide(): pure function -> TenantContext | 401 | 403 | 404
```

## Files

```text
repositories/membership_repo.rs   load_access(): the one SQL query
auth/tenant_resolver.rs           resolve() + decide(). ONLY file that builds a TenantContext
auth/extractors.rs                add TenantCtx extractor (calls resolve)
```

## The decision table (D7)

| What the database says | Result |
|---|---|
| user row missing, or `status != active` | 401 `UNAUTHORIZED` |
| no membership row (not a member, or tenant does not exist) | 404 `NOT_FOUND` |
| membership `invited` or `suspended` | 403 `FORBIDDEN` |
| active membership, tenant `suspended`/`deleted` | 403 `FORBIDDEN` |
| active membership, tenant `active` | `TenantContext` |

User status is checked first, so a disabled user is always 401, never 404.

## Gotcha found while planning (differs from the plan's SQL)

The plan's query does `memberships JOIN tenants`. But the `tenants` RLS policy only shows a tenant to
users with an **active** membership. For an `invited` or `suspended` member the JOIN would return
no row, and they would get 404 instead of the required 403.

Fix: start from `users` and `LEFT JOIN` both other tables:

```sql
SELECT u.status AS user_status, m.role, m.status AS membership_status, t.status AS tenant_status
  FROM users u
  LEFT JOIN memberships m ON m.user_id = u.id AND m.tenant_id = $1
  LEFT JOIN tenants     t ON t.id = m.tenant_id
 WHERE u.id = $2
```

- Still one query. `users` is visible (own row), `memberships` is visible (own rows).
- Invited/suspended: membership found, tenant hidden (NULL). We return 403 before looking at the tenant.
- Deleted user: no row at all, so 401.
- Because `decide()` is a pure function of this row, the whole table above is unit-testable without a DB.

## Rules

1. The resolver transaction is committed **before** `decide()`, so the connection returns to the pool (no I/O while a tx is open).
2. Authentication runs before the path is looked at: no token and a fake tenant is 401, not 404.
3. Invalid UUID in the path gives the same 404 as an unknown tenant (identical body).
4. `tenant_id` is read from the URL path only, never from body, header or query.
5. `TenantContext::from_verified_membership` is called only in `tenant_resolver.rs` (the CI grep guard is Task 10/11).
6. An unparseable role string from the DB is `Internal`, not a guess. The CHECK constraint makes it impossible in practice.
7. No Redis caching (the plan advises against it: it would delay revocation).

## Testing

Task 8 endpoints do not exist yet, so the tests build their own throwaway routes in the **tests crate**
(no production route is added). `TestApp::with_routes()` merges them into the real `AppState`.

Unit tests (no DB), in `tenant_resolver.rs`: every row of the decision table, bad role string, UUID parsing.

Integration tests (real DB, `app_runtime`), new `tests/integration/tenant_resolver.rs`:

- each role (owner/admin/member/viewer) resolves with the correct role
- a handler can `require(Permission)` (403 for viewer) and open `begin_tenant_tx` (tenant setting visible)
- non-member, unknown tenant and invalid UUID give byte-identical 404 bodies
- no token / bad token is 401, even for a fake tenant
- invited, suspended member, suspended tenant are 403
- disabled user with a valid token is 401, even as a member
- **same still-valid token**: removal gives 404 at once, suspension gives 403 at once, role change shows at once
- user in two tenants: each tenant resolves to its own role; tenant A id never grants access via B

## Build order

1. `membership_repo.rs`, `tenant_resolver.rs` (+ unit tests).
2. `TenantCtx` extractor.
3. Test helpers (`with_routes`, tenant/membership fixtures through the owner pool).
4. Integration tests.
5. `cargo fmt --check`, `clippy -D warnings`, `cargo test --workspace`, plus a repeat run.
