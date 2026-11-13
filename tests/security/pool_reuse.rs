//! Security test: connection pool reuse and context leakage prevention (§19.3).
//!
//! Proves that transaction-local GUCs (`app.current_tenant_id`, `app.current_user_id`)
//! are guaranteed to be cleared when a transaction ends (commit, rollback, drop, error),
//! preventing cross-tenant leakage across reused pool connections.

use sqlx::{postgres::PgPoolOptions, PgPool, Postgres};
use test_support::TestApp;
use uuid::Uuid;

async fn spawn() -> Option<TestApp> {
    TestApp::spawn().await
}

async fn read_setting<'e>(
    executor: impl sqlx::Executor<'e, Database = Postgres>,
    name: &str,
) -> String {
    sqlx::query_scalar::<_, Option<String>>("SELECT current_setting($1, true)")
        .bind(name)
        .fetch_one(executor)
        .await
        .unwrap()
        .unwrap_or_default()
}

/// Helper that creates a single-connection pool to guarantee connection reuse.
async fn single_conn_pool(url: &str) -> PgPool {
    PgPoolOptions::new()
        .max_connections(1)
        .connect(url)
        .await
        .expect("connect single connection pool")
}

#[tokio::test]
async fn pool_clears_settings_across_all_transaction_exit_paths() {
    let Some(_app) = spawn().await else { return };

    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let pool = single_conn_pool(&url).await;

    let tenant_a = Uuid::new_v4();
    let user_a = Uuid::new_v4();

    // 1. Commit path: settings must be cleared
    {
        let mut tx = pool.begin().await.expect("begin");
        sqlx::query(
            "SELECT set_config('app.current_tenant_id', $1, true),
                    set_config('app.current_user_id', $2, true)",
        )
        .bind(tenant_a.to_string())
        .bind(user_a.to_string())
        .execute(&mut *tx)
        .await
        .expect("set config");

        assert_eq!(
            read_setting(&mut *tx, "app.current_tenant_id").await,
            tenant_a.to_string()
        );
        tx.commit().await.expect("commit");

        // The very next query on the pool reuses the same connection:
        assert_eq!(
            read_setting(&pool, "app.current_tenant_id").await,
            "",
            "setting must be empty after commit"
        );
        assert_eq!(
            read_setting(&pool, "app.current_user_id").await,
            "",
            "setting must be empty after commit"
        );
    }

    // 2. Rollback path: settings must be cleared
    {
        let mut tx = pool.begin().await.expect("begin");
        sqlx::query("SELECT set_config('app.current_tenant_id', $1, true)")
            .bind(tenant_a.to_string())
            .execute(&mut *tx)
            .await
            .expect("set config");

        tx.rollback().await.expect("rollback");

        assert_eq!(
            read_setting(&pool, "app.current_tenant_id").await,
            "",
            "setting must be empty after rollback"
        );
    }

    // 3. Drop path (uncommitted tx dropped): settings must be cleared
    {
        let mut tx = pool.begin().await.expect("begin");
        sqlx::query("SELECT set_config('app.current_tenant_id', $1, true)")
            .bind(tenant_a.to_string())
            .execute(&mut *tx)
            .await
            .expect("set config");

        drop(tx); // Uncommitted drop triggers rollback

        assert_eq!(
            read_setting(&pool, "app.current_tenant_id").await,
            "",
            "setting must be empty after drop"
        );
    }

    // 4. Error midway path: settings must be cleared
    {
        let mut tx = pool.begin().await.expect("begin");
        sqlx::query("SELECT set_config('app.current_tenant_id', $1, true)")
            .bind(tenant_a.to_string())
            .execute(&mut *tx)
            .await
            .expect("set config");

        // Execute an invalid query that aborts the transaction
        let _ = sqlx::query("SELECT * FROM nonexistent_table_forcing_abort")
            .execute(&mut *tx)
            .await;

        let _ = tx.rollback().await;

        assert_eq!(
            read_setting(&pool, "app.current_tenant_id").await,
            "",
            "setting must be empty after aborted transaction"
        );
    }
}

#[tokio::test]
async fn interleaved_tenants_on_single_connection_see_only_own_data() {
    let Some(app) = spawn().await else { return };

    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let pool = single_conn_pool(&url).await;

    let user_a = Uuid::new_v4();
    let user_b = Uuid::new_v4();
    let tenant_a = app.seed_membership(user_a, "owner", "active").await;
    let tenant_b = app.seed_membership(user_b, "owner", "active").await;

    for _ in 0..5 {
        // Query as Tenant A
        {
            let mut tx = pool.begin().await.unwrap();
            sqlx::query("SELECT set_config('app.current_tenant_id', $1, true)")
                .bind(tenant_a.to_string())
                .execute(&mut *tx)
                .await
                .unwrap();

            let ids: Vec<(Uuid,)> = sqlx::query_as("SELECT id FROM tenants")
                .fetch_all(&mut *tx)
                .await
                .unwrap();
            assert!(ids.iter().any(|(id,)| *id == tenant_a));
            assert!(!ids.iter().any(|(id,)| *id == tenant_b));
            tx.commit().await.unwrap();
        }

        // Immediately query as Tenant B on the same connection
        {
            let mut tx = pool.begin().await.unwrap();
            sqlx::query("SELECT set_config('app.current_tenant_id', $1, true)")
                .bind(tenant_b.to_string())
                .execute(&mut *tx)
                .await
                .unwrap();

            let ids: Vec<(Uuid,)> = sqlx::query_as("SELECT id FROM tenants")
                .fetch_all(&mut *tx)
                .await
                .unwrap();
            assert!(ids.iter().any(|(id,)| *id == tenant_b));
            assert!(!ids.iter().any(|(id,)| *id == tenant_a));
            tx.commit().await.unwrap();
        }
    }
}

#[tokio::test]
async fn interleaved_tenants_on_single_connection_see_only_own_documents() {
    let Some(app) = spawn().await else { return };

    let user_a = app.create_session().await;
    let user_b = app.create_session().await;

    let tenant_a = app.seed_membership(user_a.user_id, "owner", "active").await;
    let tenant_b = app.seed_membership(user_b.user_id, "owner", "active").await;

    let doc_a = app.create_document(tenant_a, &user_a.access_token).await;
    let doc_b = app.create_document(tenant_b, &user_b.access_token).await;

    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let pool = single_conn_pool(&url).await;

    for _ in 0..5 {
        // Query documents as Tenant A
        {
            let mut tx = pool.begin().await.unwrap();
            sqlx::query("SELECT set_config('app.current_tenant_id', $1, true)")
                .bind(tenant_a.to_string())
                .execute(&mut *tx)
                .await
                .unwrap();

            let ids: Vec<(Uuid,)> = sqlx::query_as("SELECT id FROM documents")
                .fetch_all(&mut *tx)
                .await
                .unwrap();
            assert!(
                ids.iter().any(|(id,)| *id == doc_a),
                "Tenant A must see its own document"
            );
            assert!(
                !ids.iter().any(|(id,)| *id == doc_b),
                "Tenant A must NEVER see Tenant B document"
            );
            tx.commit().await.unwrap();
        }

        // Immediately query documents as Tenant B on the reused connection
        {
            let mut tx = pool.begin().await.unwrap();
            sqlx::query("SELECT set_config('app.current_tenant_id', $1, true)")
                .bind(tenant_b.to_string())
                .execute(&mut *tx)
                .await
                .unwrap();

            let ids: Vec<(Uuid,)> = sqlx::query_as("SELECT id FROM documents")
                .fetch_all(&mut *tx)
                .await
                .unwrap();
            assert!(
                ids.iter().any(|(id,)| *id == doc_b),
                "Tenant B must see its own document"
            );
            assert!(
                !ids.iter().any(|(id,)| *id == doc_a),
                "Tenant B must NEVER see Tenant A document"
            );
            tx.commit().await.unwrap();
        }
    }
}

#[tokio::test]
async fn concurrent_tasks_under_shared_pool_never_leak_tenant_context() {
    let Some(app) = spawn().await else { return };

    let user_a = Uuid::new_v4();
    let user_b = Uuid::new_v4();
    let tenant_a = app.seed_membership(user_a, "owner", "active").await;
    let tenant_b = app.seed_membership(user_b, "owner", "active").await;

    // Small pool of 3 connections shared across 20 concurrent tasks
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let pool = PgPoolOptions::new()
        .max_connections(3)
        .connect(&url)
        .await
        .unwrap();

    let mut handles = Vec::new();

    for i in 0..20 {
        let pool = pool.clone();
        let (my_tenant, foreign_tenant) = if i % 2 == 0 {
            (tenant_a, tenant_b)
        } else {
            (tenant_b, tenant_a)
        };

        handles.push(tokio::spawn(async move {
            for _ in 0..10 {
                let mut tx = pool.begin().await.unwrap();
                sqlx::query("SELECT set_config('app.current_tenant_id', $1, true)")
                    .bind(my_tenant.to_string())
                    .execute(&mut *tx)
                    .await
                    .unwrap();

                let ids: Vec<(Uuid,)> = sqlx::query_as("SELECT id FROM tenants")
                    .fetch_all(&mut *tx)
                    .await
                    .unwrap();

                assert!(
                    ids.iter().any(|(id,)| *id == my_tenant),
                    "must see own tenant"
                );
                assert!(
                    !ids.iter().any(|(id,)| *id == foreign_tenant),
                    "must NEVER see foreign tenant"
                );

                tx.commit().await.unwrap();
            }
        }));
    }

    for handle in handles {
        handle.await.expect("task succeeded without panic");
    }
}
