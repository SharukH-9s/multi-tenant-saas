# Task 8 — Tenant & Membership Endpoints

> [!NOTE]
> Task 9 (rate limiting) is **already done** and Task 8 depends on it only via `AppState` (the `rate_limiter` field is already there). Task 8 is P0 — required for the Phase 2 exit criteria. Task 9 is P1. The order doesn't break anything structurally, but the endpoint tests from Task 10 that cover tenant/membership flows are blocked until Task 8 is complete.

---

## What exists vs. what's needed

| Area | Already done (from Tasks 1–9) | Still missing (Task 8) |
|---|---|---|
| `AppState` | `db`, `token_service`, `password_service`, `rate_limiter` | Nothing |
| Auth routes | `POST /auth/register`, `login`, `refresh`, `logout`, `GET /auth/me` | — |
| `TenantContext` | `auth/tenant_resolver.rs` resolves a context, `TenantCtx` extractor exists | Extractor not wired to new routes yet |
| Domain | `Slug`, `TenantName` in `domain/tenant.rs`; `Email` in `domain/user.rs` | No `TenantRole` / `Permission` check at handler level (lives in `common-auth`) |
| Repositories | `user_repo`, `refresh_token_repo`, `membership_repo` (access-check only) | Tenant CRUD repo, full membership repo (invite/update/delete/list) |
| Handlers | `handlers/auth.rs`, `handlers/health.rs` | `handlers/tenants.rs`, `handlers/members.rs`, `handlers/invitations.rs` |
| Routes | `/api/v1/auth` wired | `/api/v1/tenants` and sub-routes not wired |

---

## Step-by-step plan

### Step 1 — Tenant repository (`repositories/tenant_repo.rs`) *(~30 min)*

Add a new file. Four functions, all take `&mut Tx` (tenant-scoped unless noted).

```rust
// user-scoped tx (GUC = U only)
pub async fn insert(tx: &mut Tx, id: Uuid, name: &str, slug: &str) -> Result<TenantRow, AppError>
pub async fn list_for_user(tx: &mut Tx, user_id: Uuid, cursor: Option<Uuid>, limit: i64) -> Result<Vec<UserTenantRow>, AppError>

// tenant-scoped tx (GUC = T + U)
pub async fn get_by_id(tx: &mut Tx, tenant_id: Uuid) -> Result<TenantRow, AppError>   // RLS enforces access
pub async fn update(tx: &mut Tx, tenant_id: Uuid, patch: TenantPatch) -> Result<TenantRow, AppError>
```

`TenantPatch` is a plain struct with `Option` fields (`name`, `slug`, `plan`, `status`). The repo sends only non-None fields (build the SQL dynamically or use a fixed `UPDATE ... SET name=COALESCE($1,name), ...`). The simpler approach: fixed coalesce query — no dynamic SQL needed at this scale.

Map DB errors in `repositories/db_error.rs` (already has `map_db_error`):
- unique violation on `tenants_slug_key` → `Conflict { code: "SLUG_TAKEN", ... }`

---

### Step 2 — Extend membership repository *(~30 min)*

[`membership_repo.rs`](file:///c:/Users/xDDDDD/.gemini/antigravity/scratch/portfolio-engine/apps/tenant-saas/src/repositories/membership_repo.rs) already has `load_access`. Add:

```rust
// user-scoped tx
pub async fn insert_owner(tx: &mut Tx, tenant_id: Uuid, user_id: Uuid) -> Result<(), AppError>
pub async fn list_for_user(tx: &mut Tx, user_id: Uuid) -> Result<Vec<UserMembershipRow>, AppError>  // GET /invitations

// tenant-scoped tx (uses tenant_member_directory() definer function)
pub async fn list_members(tx: &mut Tx) -> Result<Vec<MemberRow>, AppError>

// tenant-scoped tx
pub async fn find_member(tx: &mut Tx, target_user_id: Uuid) -> Result<Option<MembershipRow>, AppError>
pub async fn upsert_invited(tx: &mut Tx, target_user_id: Uuid, role: &str, invited_by: Uuid) -> Result<(), AppError>
pub async fn update_member(tx: &mut Tx, target_user_id: Uuid, role: Option<&str>, status: Option<&str>) -> Result<MembershipRow, AppError>
pub async fn delete_member(tx: &mut Tx, target_user_id: Uuid) -> Result<(), AppError>
pub async fn accept_invitation(tx: &mut Tx, user_id: Uuid, tenant_id: Uuid) -> Result<(), AppError>
pub async fn count_owned_tenants(tx: &mut Tx, user_id: Uuid) -> Result<i64, AppError>
```

Map: unique violation `memberships_tenant_user_unique` → `Conflict { code: "ALREADY_MEMBER", ... }`. Trigger error `P0001` hint `LAST_OWNER` → `Conflict { code: "LAST_OWNER", ... }`.

---

### Step 3 — Tenant service (`services/tenant_service.rs`) *(~30 min)*

Business logic only. No HTTP types. Each method takes `&AppState` or just `&PgPool` + relevant inputs.

```rust
pub async fn create_tenant(state: &AppState, user_id: Uuid, name: &str, slug: &str)
    -> Result<TenantRow, AppError>
```
1. `begin_user_tx(pool, user_id)`.
2. `count_owned_tenants(&mut tx, user_id)` → if ≥ 10, return `Validation("tenant limit reached")`.
3. `tenant_repo::insert(&mut tx, tenant_id, name, slug)`.
4. `membership_repo::insert_owner(&mut tx, tenant_id, user_id)`.
5. Commit. Return tenant.

```rust
pub async fn list_user_tenants(state: &AppState, user_id: Uuid, cursor: Option<Uuid>, limit: i64)
    -> Result<Vec<UserTenantRow>, AppError>
```
Just opens a user tx, calls the repo, commits.

```rust
pub async fn update_tenant(state: &AppState, ctx: &TenantContext, patch: TenantPatch)
    -> Result<TenantRow, AppError>
```
Permission checks before the tx:
- Any field → `ctx.require(Permission::TenantUpdateSettings)?`
- `patch.slug | patch.plan | patch.status` are Some → additionally `ctx.require(Permission::TenantManageCritical)?`

Then `begin_tenant_tx`, call `tenant_repo::update`, commit.

---

### Step 4 — Membership service (`services/membership_service.rs`) *(~45 min)*

The escalation rules (from D8) all live here.

```rust
pub async fn invite_member(state: &AppState, ctx: &TenantContext, email: &str, role: TenantRole)
    -> Result<(), AppError>
// 1. ctx.require(Permission::MembersManage)?
// 2. ctx.role().can_manage(role)? else 403
// 3. begin_tenant_tx
// 4. find_user_id_by_email (definer fn) — if not found, still return Ok (no enumeration)
// 5. upsert_invited (noop if already active member)
// 6. commit. Always return 202.

pub async fn list_members(state: &AppState, ctx: &TenantContext)
    -> Result<Vec<MemberRow>, AppError>
// ctx.require(Permission::MembersManage)?
// begin_tenant_tx, call list_members repo fn, commit.

pub async fn update_member(state: &AppState, ctx: &TenantContext, target_user_id: Uuid, role: Option<TenantRole>, status: Option<&str>)
    -> Result<MembershipRow, AppError>
// 1. ctx.require(Permission::MembersManage)?
// 2. Guard: target_user_id == ctx.user_id → 403 (no self-change)
// 3. begin_tenant_tx, load target membership
// 4. target.role == Owner and ctx.role != Owner → 403
// 5. promoting to Owner and ctx.role != Owner → 403
// 6. call update_member repo (trigger catches last-owner)
// 7. commit

pub async fn remove_member(state: &AppState, ctx: &TenantContext, target_user_id: Uuid)
    -> Result<(), AppError>
// Same guards as update. Can self-leave only if you're not last owner.

pub async fn accept_invitation(state: &AppState, principal: &AuthenticatedPrincipal, tenant_id: Uuid)
    -> Result<(), AppError>
// begin_user_tx, verify membership status == 'invited' for this user+tenant, then:
// begin_tenant_tx, set status = 'active'. commit.
```

---

### Step 5 — Handlers (`handlers/tenants.rs`, `handlers/members.rs`, `handlers/invitations.rs`) *(~45 min)*

Handlers are thin: validate input, call a service, return a response. No business logic.

**`handlers/tenants.rs`**

```
POST /tenants             → create_tenant handler → 201
GET  /tenants             → list_user_tenants handler → 200
GET  /tenants/:id         → get_tenant handler → 200 (uses TenantCtx extractor)
PATCH /tenants/:id        → update_tenant handler → 200
```

**`handlers/members.rs`**

```
GET    /tenants/:id/members           → list_members → 200
PATCH  /tenants/:id/members/:user_id  → update_member → 200
DELETE /tenants/:id/members/:user_id  → remove_member → 204
```

**`handlers/invitations.rs`**

```
POST /tenants/:id/invitations         → invite_member → 202
GET  /invitations                     → list_pending_invitations → 200 (user-scoped, AuthUser extractor)
POST /invitations/:tenant_id/accept   → accept_invitation → 200
```

**Shared pattern for every handler:**

```rust
async fn create_tenant(
    State(state): State<AppState>,
    AuthUser(principal): AuthUser,
    Json(body): Json<CreateTenantRequest>,
) -> Result<impl IntoResponse, AppError> {
    let name = TenantName::parse(&body.name)?;
    let slug = Slug::parse(&body.slug)?;
    let tenant = tenant_service::create_tenant(&state, principal.user_id, name.as_str(), slug.as_str()).await?;
    Ok((StatusCode::CREATED, Json(TenantResponse::from(tenant))))
}
```

---

### Step 6 — Wire routes in `routes.rs` *(~15 min)*

In [`routes.rs`](file:///c:/Users/xDDDDD/.gemini/antigravity/scratch/portfolio-engine/apps/tenant-saas/src/routes.rs), extend `api_v1()`:

```rust
fn api_v1() -> Router<AppState> {
    Router::new()
        .nest("/auth", handlers::auth::router())
        .nest("/tenants", handlers::tenants::router())
        .nest("/invitations", handlers::invitations::router())
}
```

Tenant router (in `handlers/tenants.rs`):
```rust
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", post(create_tenant).get(list_tenants))
        .route("/:tenant_id", get(get_tenant).patch(update_tenant))
        .route("/:tenant_id/members", get(list_members))
        .route("/:tenant_id/members/:user_id", patch(update_member).delete(remove_member))
        .route("/:tenant_id/invitations", post(invite_member))
        .layer(DefaultBodyLimit::max(32 * 1024))
}
```

---

### Step 7 — Audit events *(~15 min)*

Add `tracing::info!(target: "audit", ...)` calls in service methods. No new dependency — just structured log fields:

```rust
tracing::info!(
    target: "audit",
    event = "tenant.created",
    request_id = %ctx.request_id,  // pass from handler
    actor_user_id = %user_id,
    tenant_id = %tenant.id,
);
```

Events to add: `tenant.created`, `member.invited`, `member.role_changed`, `member.removed`, `invitation.accepted`.

---

### Step 8 — Tests *(alongside above, ~30 min)*

Add to the existing test files in `tests/integration/`:

**`tests/integration/multi_tenant_membership.rs`** (new file):
1. Create user A. User A creates tenant-1 (becomes owner).
2. User A invites user B as `member`. User B accepts.
3. `GET /tenants` for A returns both tenant-1. For B returns tenant-1 with role `member`.
4. User B cannot `PATCH /tenants/:id` (403). User A can.
5. User A cannot `PATCH /tenants/:id/members/:b_id` to promote B to `owner` (admin check path — A is owner so this succeeds; test with an admin trying instead).

**`tests/integration/invitations.rs`** (new file):
1. Invite unregistered email → `202` (no enumeration).
2. Invite already-active member → `202`, membership unchanged.
3. Invite registered but unregistered → `202`.
4. User accepts → `GET /tenants` now returns that tenant.

**`tests/security/escalation.rs`** (new file):
1. Last-owner demote → `409 LAST_OWNER`.
2. Admin tries to remove an owner → `403`.
3. User tries to change their own role → `403`.

---

## Impact on Task 9 (what we need to change)

Task 9 is **fail-open** (Redis down → allow). The plan specifies **fail-closed** (`503`) for auth endpoints. Looking at [`rate_limiter.rs`](file:///c:/Users/xDDDDD/.gemini/antigravity/scratch/portfolio-engine/apps/tenant-saas/src/services/rate_limiter.rs):

> [!WARNING]
> The implemented behavior is **fail-open** (Redis down → `Ok(())`). The plan spec (Task 9 in `phase2_implementation_plan.md`) says "Fail closed when Redis is unavailable: auth endpoints return `503`". There is a **conflict** between the spec and what was built.

**Decision needed:** The fail-open implementation is actually *safer* for availability (users aren't blocked by a Redis outage). The spec says fail-closed for security. Pick one:

| Option | Pros | Cons |
|---|---|---|
| **Keep fail-open** (current) | No availability loss during Redis outage | Rate limiting can be bypassed during outage |
| **Change to fail-closed** | Stricter security | Login broken for all users when Redis is down |

**Recommendation:** Keep fail-open. The circuit breaker + WARN logs still give visibility. Document it as an explicit ADR decision. The `rate_limit.rs` integration test already asserts fail-open behavior and the test for Redis outage (`redis_outage_fails_open_and_trips_circuit_breaker`) matches what's built.

If you decide to keep fail-open (recommended), update the comment in `phase2_implementation_plan.md` Task 9 to reflect the final decision. No code changes needed.

---

## File checklist

```
apps/tenant-saas/src/
  repositories/
    tenant_repo.rs         ← NEW
    membership_repo.rs     ← EXTEND (5+ new functions)
    db_error.rs            ← EXTEND (SLUG_TAKEN, ALREADY_MEMBER, LAST_OWNER)
  services/
    tenant_service.rs      ← NEW
    membership_service.rs  ← NEW
  handlers/
    tenants.rs             ← NEW
    members.rs             ← NEW  (or combine with tenants.rs)
    invitations.rs         ← NEW
  routes.rs                ← EXTEND api_v1()
tests/
  integration/
    multi_tenant_membership.rs  ← NEW
    invitations.rs              ← NEW
  security/
    escalation.rs               ← NEW
```

**Estimated time: ~3.5–4 hours** (matches the plan's ~3h estimate, slightly more with tests).
