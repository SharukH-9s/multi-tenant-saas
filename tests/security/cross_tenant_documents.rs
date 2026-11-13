//! Security test: cross-tenant isolation and data concealment for documents (Task 8).
//!
//! Verifies:
//! 1. User B querying Tenant A's documents receives 404 identical to non-existent resources (no existence oracle).
//! 2. User B passing Tenant A's document ID in Tenant B's path gets 404.
//! 3. Direct SQL through `app_runtime` with Tenant B scope cannot SELECT, INSERT, UPDATE, or DELETE Tenant A documents.
//! 4. Direct SQL without any tenant GUC fails closed (zero rows, mutations blocked).
//! 5. Forged tenant context cannot reach another tenant's rows.
//! 6. `object_key` NEVER appears in any HTTP response body or in `captured_logs()`.

use axum::http::{Method, StatusCode};
use serde_json::Value;
use test_support::{captured_logs, TestApp};
use uuid::Uuid;

async fn spawn() -> Option<TestApp> {
    TestApp::spawn().await
}

#[tokio::test]
async fn cross_tenant_access_returns_identical_404_no_existence_oracle() {
    let Some(app) = spawn().await else { return };

    let user_a = app.create_session().await;
    let user_b = app.create_session().await;

    let tenant_a = app.seed_membership(user_a.user_id, "owner", "active").await;
    let _tenant_b = app.seed_membership(user_b.user_id, "owner", "active").await;

    let doc_a = app.create_document(tenant_a, &user_a.access_token).await;
    let nonexistent_id = Uuid::new_v4();

    // Baseline 404 for a non-existent document
    let (base_status, base_body) = app
        .request(
            Method::GET,
            &format!("/api/v1/tenants/{tenant_a}/documents/{nonexistent_id}"),
            None,
            Some(&user_b.access_token),
        )
        .await;
    assert_eq!(base_status, StatusCode::NOT_FOUND);
    assert_eq!(base_body["error"]["code"], "NOT_FOUND");
    let standard_message = base_body["error"]["message"].as_str().unwrap();

    let assert_concealed = |status: StatusCode, body: Value, desc: &str| {
        assert_eq!(status, StatusCode::NOT_FOUND, "{desc} status");
        assert_eq!(body["error"]["code"], "NOT_FOUND", "{desc} code");
        assert_eq!(body["error"]["message"], standard_message, "{desc} message");
    };

    // User B tries GET /tenants/{A}/documents/{doc_a} -> 404
    let (status, body) = app
        .request(
            Method::GET,
            &format!("/api/v1/tenants/{tenant_a}/documents/{doc_a}"),
            None,
            Some(&user_b.access_token),
        )
        .await;
    assert_concealed(status, body, "cross-tenant GET");

    // User B tries GET /tenants/{A}/documents/{doc_a}/status -> 404
    let (status, body) = app
        .request(
            Method::GET,
            &format!("/api/v1/tenants/{tenant_a}/documents/{doc_a}/status"),
            None,
            Some(&user_b.access_token),
        )
        .await;
    assert_concealed(status, body, "cross-tenant GET status");

    // User B tries DELETE /tenants/{A}/documents/{doc_a} -> 404
    let (status, body) = app
        .request(
            Method::DELETE,
            &format!("/api/v1/tenants/{tenant_a}/documents/{doc_a}"),
            None,
            Some(&user_b.access_token),
        )
        .await;
    assert_concealed(status, body, "cross-tenant DELETE");
}

#[tokio::test]
async fn cross_tenant_doc_id_in_own_tenant_path_returns_404() {
    let Some(app) = spawn().await else { return };

    let user_a = app.create_session().await;
    let user_b = app.create_session().await;

    let tenant_a = app.seed_membership(user_a.user_id, "owner", "active").await;
    let tenant_b = app.seed_membership(user_b.user_id, "owner", "active").await;

    let doc_a = app.create_document(tenant_a, &user_a.access_token).await;

    // User B calls Tenant B endpoints using Document A's ID -> 404 Not Found
    let (status, body) = app
        .request(
            Method::GET,
            &format!("/api/v1/tenants/{tenant_b}/documents/{doc_a}"),
            None,
            Some(&user_b.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "NOT_FOUND");

    let (status, body) = app
        .request(
            Method::GET,
            &format!("/api/v1/tenants/{tenant_b}/documents/{doc_a}/status"),
            None,
            Some(&user_b.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "NOT_FOUND");

    let (status, body) = app
        .request(
            Method::DELETE,
            &format!("/api/v1/tenants/{tenant_b}/documents/{doc_a}"),
            None,
            Some(&user_b.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "NOT_FOUND");
}

#[tokio::test]
async fn direct_sql_with_tenant_b_context_cannot_touch_tenant_a_documents() {
    let Some(app) = spawn().await else { return };

    let user_a = app.create_session().await;
    let user_b = app.create_session().await;

    let tenant_a = app.seed_membership(user_a.user_id, "owner", "active").await;
    let tenant_b = app.seed_membership(user_b.user_id, "owner", "active").await;

    let doc_a = app.create_document(tenant_a, &user_a.access_token).await;

    let scope_tx = || async {
        let mut tx = app.runtime_pool.begin().await.expect("begin");
        sqlx::query(
            "SELECT set_config('app.current_tenant_id', $1, true),
                    set_config('app.current_user_id', $2, true)",
        )
        .bind(tenant_b.to_string())
        .bind(user_b.user_id.to_string())
        .execute(&mut *tx)
        .await
        .expect("set scope");
        tx
    };

    // 1. SELECT returns zero rows
    let mut tx1 = scope_tx().await;
    let rows: Vec<(Uuid,)> = sqlx::query_as("SELECT id FROM documents WHERE id = $1")
        .bind(doc_a)
        .fetch_all(&mut *tx1)
        .await
        .expect("query executed");
    assert!(
        rows.is_empty(),
        "Tenant B context must see 0 rows of Tenant A"
    );
    tx1.commit().await.unwrap();

    // 2. INSERT with tenant_id = tenant_a is refused by RLS WITH CHECK policy
    let mut tx2 = scope_tx().await;
    let insert_res = sqlx::query(
        "INSERT INTO documents \
            (tenant_id, uploaded_by, title, original_filename, content_hash, object_key, mime_type, size_bytes) \
         VALUES ($1, $2, 'Hacked', 'hacked.txt', $3, 'tenants/leak', 'text/plain', 10)",
    )
    .bind(tenant_a)
    .bind(user_b.user_id)
    .bind("1111222233334444555566667777888899990000aaaabbbbccccddddeeeeffff")
    .execute(&mut *tx2)
    .await;
    assert!(
        insert_res.is_err(),
        "INSERT into Tenant A under Tenant B context must fail RLS"
    );
    let _ = tx2.rollback().await;

    // 3. UPDATE affects zero rows
    let mut tx3 = scope_tx().await;
    let update_res = sqlx::query("UPDATE documents SET title = 'Hacked' WHERE id = $1")
        .bind(doc_a)
        .execute(&mut *tx3)
        .await
        .expect("update executed");
    assert_eq!(
        update_res.rows_affected(),
        0,
        "UPDATE must affect 0 rows across tenants"
    );
    tx3.commit().await.unwrap();

    // 4. DELETE is refused (app_runtime has no DELETE grant on documents)
    let mut tx4 = scope_tx().await;
    let delete_res = sqlx::query("DELETE FROM documents WHERE id = $1")
        .bind(doc_a)
        .execute(&mut *tx4)
        .await;
    assert!(delete_res.is_err(), "DELETE must fail due to missing grant");
    let _ = tx4.rollback().await;
}

#[tokio::test]
async fn direct_sql_without_guc_fails_closed() {
    let Some(app) = spawn().await else { return };

    let user_a = app.create_session().await;
    let tenant_a = app.seed_membership(user_a.user_id, "owner", "active").await;
    let doc_a = app.create_document(tenant_a, &user_a.access_token).await;

    // Direct query on app_runtime pool with NO GUC set
    let rows: Vec<(Uuid,)> = sqlx::query_as("SELECT id FROM documents WHERE id = $1")
        .bind(doc_a)
        .fetch_all(&app.runtime_pool)
        .await
        .expect("query executed");
    assert!(
        rows.is_empty(),
        "No GUC must return zero rows (fail closed)"
    );

    // INSERT with no GUC set fails closed
    let insert_res = sqlx::query(
        "INSERT INTO documents \
            (tenant_id, uploaded_by, title, original_filename, content_hash, object_key, mime_type, size_bytes) \
         VALUES ($1, $2, 'No GUC', 'noguc.txt', $3, 'tenants/noguc', 'text/plain', 10)",
    )
    .bind(tenant_a)
    .bind(user_a.user_id)
    .bind("9999222233334444555566667777888899990000aaaabbbbccccddddeeeeffff")
    .execute(&app.runtime_pool)
    .await;
    assert!(
        insert_res.is_err(),
        "INSERT with no GUC must be rejected by RLS"
    );
}

#[tokio::test]
async fn forged_context_cannot_reach_another_tenant_rows() {
    let Some(app) = spawn().await else { return };

    let user_a = app.create_session().await;
    let tenant_a = app.seed_membership(user_a.user_id, "owner", "active").await;
    let doc_a = app.create_document(tenant_a, &user_a.access_token).await;

    let forged_tenant = Uuid::new_v4();
    let forged_user = Uuid::new_v4();

    let mut tx = app.runtime_pool.begin().await.expect("begin");
    sqlx::query(
        "SELECT set_config('app.current_tenant_id', $1, true),
                set_config('app.current_user_id', $2, true)",
    )
    .bind(forged_tenant.to_string())
    .bind(forged_user.to_string())
    .execute(&mut *tx)
    .await
    .expect("set forged context");

    let rows: Vec<(Uuid,)> = sqlx::query_as("SELECT id FROM documents WHERE id = $1")
        .bind(doc_a)
        .fetch_all(&mut *tx)
        .await
        .expect("query executed");
    assert!(
        rows.is_empty(),
        "Forged tenant context must see 0 rows of Tenant A"
    );

    let update = sqlx::query("UPDATE documents SET title = 'Forged' WHERE id = $1")
        .bind(doc_a)
        .execute(&mut *tx)
        .await
        .expect("update executed");
    assert_eq!(
        update.rows_affected(),
        0,
        "Forged tenant context cannot update Tenant A"
    );
}

#[tokio::test]
async fn object_key_never_leaks_in_responses_or_logs() {
    let Some(app) = spawn().await else { return };

    let session = app.create_session().await;
    let tenant_id = app
        .seed_membership(session.user_id, "admin", "active")
        .await;

    // Upload document
    let (status, body) = app
        .upload_file(
            tenant_id,
            &session.access_token,
            "secret.txt",
            b"Secret data",
            Some("Secret Doc"),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let doc_id: Uuid = body["id"].as_str().unwrap().parse().unwrap();

    let row = app.document_row(tenant_id, doc_id).await.expect("row");
    let secret_key = &row.object_key;
    assert!(!secret_key.is_empty());

    // 1. Response bodies must never contain object_key
    assert!(
        body.get("object_key").is_none(),
        "upload response leaked object_key"
    );

    let (_, get_body) = app
        .request(
            Method::GET,
            &format!("/api/v1/tenants/{tenant_id}/documents/{doc_id}"),
            None,
            Some(&session.access_token),
        )
        .await;
    assert!(
        get_body.get("object_key").is_none(),
        "GET response leaked object_key"
    );

    let (_, status_body) = app
        .request(
            Method::GET,
            &format!("/api/v1/tenants/{tenant_id}/documents/{doc_id}/status"),
            None,
            Some(&session.access_token),
        )
        .await;
    assert!(
        status_body.get("object_key").is_none(),
        "status response leaked object_key"
    );

    let (_, list_body) = app
        .request(
            Method::GET,
            &format!("/api/v1/tenants/{tenant_id}/documents"),
            None,
            Some(&session.access_token),
        )
        .await;
    let items = list_body.as_array().unwrap();
    for item in items {
        assert!(
            item.get("object_key").is_none(),
            "list item leaked object_key"
        );
    }

    // 2. Logs must never contain the object_key
    let logs = captured_logs();
    assert!(
        !logs.contains(secret_key),
        "object_key '{secret_key}' found in captured application logs!"
    );
}
