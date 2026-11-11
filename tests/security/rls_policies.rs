//! Security test: raw SQL RLS policies enforced directly on `app_runtime`.
//!
//! Proves that even if the application layer were completely bypassed or had a bug,
//! PostgreSQL Row-Level Security on the database connection strictly stops
//! cross-tenant data leaks and unauthorized mutations.

use test_support::TestApp;
use uuid::Uuid;

async fn spawn() -> Option<TestApp> {
    TestApp::spawn().await
}

#[tokio::test]
async fn no_guc_set_fails_closed_zero_rows_and_insert_blocked() {
    let Some(app) = spawn().await else { return };

    // Seed two tenants and memberships using the owner pool
    let user_a = Uuid::new_v4();
    let tenant_a = app.seed_membership(user_a, "owner", "active").await;

    // Direct query through app_runtime pool with NO GUC set
    let tenants: Vec<(Uuid,)> = sqlx::query_as("SELECT id FROM tenants")
        .fetch_all(&app.runtime_pool)
        .await
        .expect("query must execute safely");
    assert!(
        !tenants.iter().any(|(id,)| *id == tenant_a),
        "tenants with no GUC set must return zero rows for tenant A"
    );

    let memberships: Vec<(Uuid,)> = sqlx::query_as("SELECT id FROM memberships")
        .fetch_all(&app.runtime_pool)
        .await
        .expect("query must execute safely");
    assert!(
        memberships.is_empty(),
        "memberships with no GUC set must return 0 rows"
    );

    // Direct INSERT without GUC -> fails RLS WITH CHECK policy
    let insert_res = sqlx::query(
        "INSERT INTO memberships (tenant_id, user_id, role, status) VALUES ($1, $2, 'member', 'active')",
    )
    .bind(tenant_a)
    .bind(Uuid::new_v4())
    .execute(&app.runtime_pool)
    .await;

    assert!(
        insert_res.is_err(),
        "INSERT into memberships with no GUC must fail RLS policy"
    );
}

#[tokio::test]
async fn guc_scoped_to_tenant_a_cannot_read_or_modify_tenant_b() {
    let Some(app) = spawn().await else { return };

    let user_a = Uuid::new_v4();
    let user_b = Uuid::new_v4();
    let tenant_a = app.seed_membership(user_a, "owner", "active").await;
    let tenant_b = app.seed_membership(user_b, "owner", "active").await;

    // Start a transaction as app_runtime and scope to Tenant A
    let mut tx = app.runtime_pool.begin().await.expect("begin");
    sqlx::query(
        "SELECT set_config('app.current_tenant_id', $1, true),
                set_config('app.current_user_id', $2, true)",
    )
    .bind(tenant_a.to_string())
    .bind(user_a.to_string())
    .execute(&mut *tx)
    .await
    .expect("set scope");

    // 1. SELECT from tenants sees Tenant A, never Tenant B
    let visible_tenants: Vec<(Uuid,)> = sqlx::query_as("SELECT id FROM tenants")
        .fetch_all(&mut *tx)
        .await
        .expect("query tenants");
    assert!(visible_tenants.iter().any(|(id,)| *id == tenant_a));
    assert!(
        !visible_tenants.iter().any(|(id,)| *id == tenant_b),
        "Tenant B must not be visible under Tenant A scope"
    );

    // 2. Cannot INSERT a membership belonging to Tenant B (WITH CHECK violation)
    let insert_b = sqlx::query(
        "INSERT INTO memberships (tenant_id, user_id, role, status) VALUES ($1, $2, 'member', 'active')",
    )
    .bind(tenant_b)
    .bind(Uuid::new_v4())
    .execute(&mut *tx)
    .await;
    assert!(
        insert_b.is_err(),
        "INSERT into foreign tenant must be blocked by RLS WITH CHECK"
    );

    // 3. UPDATE of Tenant B row affects 0 rows
    let update_b = sqlx::query("UPDATE tenants SET name = 'Tampered' WHERE id = $1")
        .bind(tenant_b)
        .execute(&mut *tx)
        .await
        .expect("update query executes");
    assert_eq!(
        update_b.rows_affected(),
        0,
        "UPDATE on foreign tenant must affect 0 rows"
    );

    // 4. DELETE of Tenant B membership affects 0 rows
    let delete_b = sqlx::query("DELETE FROM memberships WHERE tenant_id = $1")
        .bind(tenant_b)
        .execute(&mut *tx)
        .await
        .expect("delete query executes");
    assert_eq!(
        delete_b.rows_affected(),
        0,
        "DELETE on foreign membership must affect 0 rows"
    );

    tx.rollback().await.expect("rollback");
}

#[tokio::test]
async fn invalid_guc_format_fails_safely_never_leaks() {
    let Some(app) = spawn().await else { return };

    let mut tx = app.runtime_pool.begin().await.expect("begin");
    // Setting GUC to invalid non-UUID string
    sqlx::query("SELECT set_config('app.current_tenant_id', 'not-a-valid-uuid', true)")
        .execute(&mut *tx)
        .await
        .expect("set_config raw string succeeds");

    // Query on tenants invokes app_tenant_id() which casts to ::uuid
    // This MUST safely error on cast failure rather than falling back or leaking
    let res: Result<Vec<(Uuid,)>, _> = sqlx::query_as("SELECT id FROM tenants")
        .fetch_all(&mut *tx)
        .await;

    assert!(
        res.is_err(),
        "query with malformed UUID in GUC must safely error on cast failure"
    );

    tx.rollback().await.expect("rollback");
}

#[tokio::test]
async fn app_runtime_permission_restrictions() {
    let Some(app) = spawn().await else { return };

    // 1. app_runtime CANNOT read password_hash directly from users
    let read_pwd = sqlx::query("SELECT password_hash FROM users")
        .fetch_all(&app.runtime_pool)
        .await;
    assert!(
        read_pwd.is_err(),
        "app_runtime must be denied SELECT on password_hash"
    );

    // 2. app_runtime CANNOT delete from tenants
    let del_tenant = sqlx::query("DELETE FROM tenants")
        .execute(&app.runtime_pool)
        .await;
    assert!(
        del_tenant.is_err(),
        "app_runtime must not have DELETE grant on tenants"
    );

    // 3. app_runtime CANNOT delete from users
    let del_users = sqlx::query("DELETE FROM users")
        .execute(&app.runtime_pool)
        .await;
    assert!(
        del_users.is_err(),
        "app_runtime must not have DELETE grant on users"
    );

    // 4. app_runtime CANNOT run DDL (CREATE/ALTER)
    let ddl = sqlx::query("CREATE TABLE hacked_table (id int)")
        .execute(&app.runtime_pool)
        .await;
    assert!(
        ddl.is_err(),
        "app_runtime must not have CREATE table privileges"
    );
}

