# Task 6 — Auth endpoints: working plan

Source: `phase2_implementation_plan.md` Task 6 and decisions D1–D3, D7.
Tasks 1–5 are already in the working tree (common-auth, begin_user_tx, 002 migration, AppState, value objects). Task 6 only adds code on top.

## Layers (routes -> service -> repository)

```text
handlers/auth.rs            thin: parse JSON, call service, shape response
services/auth_service.rs    the rules: register, login, refresh, logout, me
repositories/user_repo.rs   SQL for users, credentials, memberships
repositories/refresh_token_repo.rs   SQL for refresh tokens
repositories/db_error.rs    map_db_error: unique-constraint name -> Conflict code
auth/extractors.rs          `AuthUser`: Bearer token -> principal (401 on any problem)
```

## Endpoint rules

| Endpoint | Steps |
|---|---|
| register | validate email + password -> hash (no tx open) -> `begin_user_tx(new_id)` -> insert -> commit. 409 `EMAIL_ALREADY_REGISTERED` on `users_email_key`. No tokens. |
| login | `auth_get_credentials(email)` -> always run Argon2 verify (dummy hash if unknown/bad email) -> unknown, wrong password and non-active all give the same `InvalidCredentials` -> issue JWT + new refresh family. |
| refresh | ONE tx: rotate row (`UPDATE ... WHERE rotated_at IS NULL AND revoked_at IS NULL AND expires_at > now()`). No row: if the hash exists and was already rotated -> revoke family, **commit**, 401. Row found: set user scope in the same tx, check `users.status = 'active'`, insert next token in same family. |
| logout | revoke the family of the given token, only if it belongs to the caller. Always 204 (idempotent, no probing). |
| me | `begin_user_tx` -> user row must exist and be active (else 401) -> active memberships of active tenants. |

## Decisions and gotchas

1. **Orphan rule**: `AuthenticatedPrincipal` lives in common-auth, so the extractor is a local newtype `AuthUser(pub AuthenticatedPrincipal)`.
2. **Refresh needs the user id before it can scope the tx.** Add `set_user_scope(&mut Tx, user_id)` to `common-database` (and reuse it inside `begin_user_tx`), so GUC setting stays in one place.
3. **Commit before returning an error** on reuse detection or disabled user; otherwise the revoke is rolled back with the error.
4. **Citext**: cast `$1::citext` on the way in, `email::text` on the way out (sqlx has no citext decoder).
5. **No network I/O inside a tx**: hashing and JWT signing happen outside any open tx.
6. **Rate limiting** is Task 9 (Redis). Task 6 leaves handlers ready for it; nothing is added here.
7. **Concurrent double-refresh with the same token** counts as reuse and kills the family. That is strict-by-design and matches the plan; documented in code.
8. **413 on oversize body** is returned as 400 by the existing `ApiJson` mapping. Accepted.
9. Audit events use `target="audit"`, carry ids only (never email, password, tokens). `request_id` comes from the existing request span.

## Build order (each step compiles and is tested before the next)

1. `common-database::set_user_scope` + app deps (`uuid`, `chrono`).
2. `db_error.rs`, `user_repo.rs`, `refresh_token_repo.rs`.
3. `extractors.rs` (+ unit tests for header parsing).
4. `auth_service.rs`.
5. `handlers/auth.rs`, wire into `api_v1()` with a 16 KiB body limit.
6. Integration tests in `tests/integration/auth.rs` (incl. log-leak test).
7. `cargo fmt --check`, `clippy -D warnings`, `cargo test --workspace`.

## Test checklist

- register: 201 / duplicate 409 / bad email 422 / short password 422 / no `password_hash` in body
- login: success shape; unknown email, wrong password, disabled user all return an identical 401 body
- me: ok with token; 401 for missing, malformed, wrong-secret, garbage token; disabled user -> 401
- refresh: rotates; old token reused -> 401 AND the new token from that family stops working; expired/unknown -> 401
- logout: 204, refresh no longer works; second call still 204; cannot revoke another user's family
- logs: captured subscriber never sees password, access token, refresh token or hash
