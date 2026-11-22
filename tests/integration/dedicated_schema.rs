//! Dedicated schema end-to-end integration and RLS security test (Phase 5, Task 14a).
//!
//! Validates that a promoted tenant:
//! 1. Routes chunk ingestion to its dedicated schema (0 rows in public.document_chunks).
//! 2. Routes hybrid search candidates and hydration (joining public.documents) correctly.
//! 3. Enforces RLS under `app_runtime` (another tenant cannot read or search chunks).
//! 4. Guarantees chunk immutability (UPDATE is denied to `app_runtime`).
//! 5. Routes document deletion to remove chunks from the dedicated table.
//! 6. Executes `vector_candidates` EXPLAIN successfully under `app_runtime`.

use axum::http::{Method, StatusCode};
use common_auth::{TenantContext, TenantRole};
use common_database::begin_tenant_tx;
use common_messaging::IngestionEvent;
use serde_json::json;
use tenant_saas::services::ingestion_service::{self, IngestionDeps, ProcessOutcome};
use test_support::TestApp;
use uuid::Uuid;

const DEDICATED_DOC_TEXT: &[u8] =
    b"Vanguard systems propulsion architecture and orbital synchronization protocol.";

#[tokio::test]
async fn dedicated_schema_end_to_end_and_rls() {
    let Some(app) = TestApp::spawn().await else {
        return;
    };

    // 1. Setup tenant & user
    let session = app.create_session().await;
    let tenant_id = app
        .seed_membership(session.user_id, "owner", "active")
        .await;

    // Schema suffix must be <= 56 chars (NAMEDATALEN - 1 = 63 total)
    let schema_name = format!("tenant_{}", &tenant_id.simple().to_string()[..32]);

    // 2. Provision dedicated schema via db_owner fixture pool
    let owner = app.owner();
    sqlx::query(sqlx::AssertSqlSafe(
        format!("CREATE SCHEMA IF NOT EXISTS {schema_name}").as_str(),
    ))
    .execute(owner)
    .await
    .unwrap();

    sqlx::query(sqlx::AssertSqlSafe(
        format!("GRANT USAGE ON SCHEMA {schema_name} TO app_runtime").as_str(),
    ))
    .execute(owner)
    .await
    .unwrap();

    sqlx::query(sqlx::AssertSqlSafe(
        format!(
            "CREATE TABLE {schema_name}.document_chunks ( \
                id uuid PRIMARY KEY DEFAULT gen_random_uuid(), \
                tenant_id uuid NOT NULL REFERENCES public.tenants(id) ON DELETE CASCADE, \
                document_id uuid NOT NULL REFERENCES public.documents(id) ON DELETE CASCADE, \
                chunk_index integer NOT NULL, \
                content text NOT NULL, \
                token_count integer NOT NULL, \
                embedding vector(1536) NOT NULL, \
                embedding_model text NOT NULL, \
                embedding_version text NOT NULL, \
                metadata jsonb NOT NULL DEFAULT '{{}}'::jsonb, \
                tsv_content tsvector GENERATED ALWAYS AS (to_tsvector('english', content)) STORED, \
                created_at timestamptz NOT NULL DEFAULT now(), \
                CONSTRAINT {schema_name}_doc_chunk_idx UNIQUE (tenant_id, document_id, chunk_index) \
            )"
        )
        .as_str(),
    ))
    .execute(owner)
    .await
    .unwrap();

    sqlx::query(sqlx::AssertSqlSafe(
        format!("ALTER TABLE {schema_name}.document_chunks ENABLE ROW LEVEL SECURITY").as_str(),
    ))
    .execute(owner)
    .await
    .unwrap();

    sqlx::query(sqlx::AssertSqlSafe(
        format!("ALTER TABLE {schema_name}.document_chunks FORCE ROW LEVEL SECURITY").as_str(),
    ))
    .execute(owner)
    .await
    .unwrap();

    sqlx::query(sqlx::AssertSqlSafe(
        format!(
            "CREATE POLICY chunk_sel ON {schema_name}.document_chunks \
                FOR SELECT TO app_runtime \
                USING (tenant_id = (SELECT app_tenant_id()))"
        )
        .as_str(),
    ))
    .execute(owner)
    .await
    .unwrap();

    sqlx::query(sqlx::AssertSqlSafe(
        format!(
            "CREATE POLICY chunk_ins ON {schema_name}.document_chunks \
                FOR INSERT TO app_runtime \
                WITH CHECK (tenant_id = (SELECT app_tenant_id()))"
        )
        .as_str(),
    ))
    .execute(owner)
    .await
    .unwrap();

    sqlx::query(sqlx::AssertSqlSafe(
        format!(
            "CREATE POLICY chunk_del ON {schema_name}.document_chunks \
                FOR DELETE TO app_runtime \
                USING (tenant_id = (SELECT app_tenant_id()))"
        )
        .as_str(),
    ))
    .execute(owner)
    .await
    .unwrap();

    sqlx::query(sqlx::AssertSqlSafe(
        format!("GRANT SELECT, INSERT, DELETE ON {schema_name}.document_chunks TO app_runtime")
            .as_str(),
    ))
    .execute(owner)
    .await
    .unwrap();

    sqlx::query(sqlx::AssertSqlSafe(
        format!(
            "CREATE INDEX {schema_name}_tsv_idx ON {schema_name}.document_chunks USING gin (tsv_content)"
        )
        .as_str(),
    ))
    .execute(owner)
    .await
    .unwrap();

    sqlx::query(sqlx::AssertSqlSafe(
        format!(
            "CREATE INDEX {schema_name}_hnsw_idx ON {schema_name}.document_chunks \
                USING hnsw (embedding vector_cosine_ops) WITH (m=16, ef_construction=64)"
        )
        .as_str(),
    ))
    .execute(owner)
    .await
    .unwrap();

    // 3. Promote tenant: update dedicated_schema using owner_scoped_tx
    {
        let mut tx = app.owner_scoped_tx(tenant_id, session.user_id).await;
        sqlx::query("UPDATE tenants SET dedicated_schema = $2 WHERE id = $1")
            .bind(tenant_id)
            .bind(&schema_name)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    // 4. Ingest document via API
    let (status, body) = app
        .upload_file(
            tenant_id,
            &session.access_token,
            "propulsion.txt",
            DEDICATED_DOC_TEXT,
            Some("Propulsion Spec"),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let document_id: Uuid = body["id"].as_str().unwrap().parse().unwrap();

    let (job_id,): (Uuid,) =
        sqlx::query_as("SELECT id FROM ingestion_jobs WHERE tenant_id = $1 AND document_id = $2")
            .bind(tenant_id)
            .bind(document_id)
            .fetch_one(app.owner())
            .await
            .unwrap();

    let deps = IngestionDeps::new(app.state.clone(), format!("worker-{}", Uuid::new_v4()));
    let outcome =
        ingestion_service::process(&deps, &IngestionEvent::new(job_id, tenant_id, document_id))
            .await;
    assert!(
        matches!(outcome, ProcessOutcome::Succeeded { chunks: 1 }),
        "ingestion must succeed: {outcome:?}"
    );

    // 5. Verify chunk routing: chunks exist in dedicated table, ZERO in public
    let (dedicated_count,): (i64,) = sqlx::query_as(sqlx::AssertSqlSafe(
        format!("SELECT count(*) FROM {schema_name}.document_chunks WHERE document_id = $1")
            .as_str(),
    ))
    .bind(document_id)
    .fetch_one(app.owner())
    .await
    .unwrap();
    assert_eq!(dedicated_count, 1, "chunks must be in dedicated schema");

    let (public_count,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM public.document_chunks WHERE document_id = $1")
            .bind(document_id)
            .fetch_one(app.owner())
            .await
            .unwrap();
    assert_eq!(public_count, 0, "zero chunks must be in public schema");

    // 6. HTTP Search over dedicated table
    let (search_status, search_body) = app
        .request(
            Method::POST,
            &format!("/api/v1/tenants/{tenant_id}/search"),
            Some(json!({"query": "orbital synchronization propulsion"})),
            Some(&session.access_token),
        )
        .await;
    assert_eq!(search_status, StatusCode::OK, "{search_body}");
    let hit_ids: Vec<String> = search_body["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["document_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        hit_ids,
        vec![document_id.to_string()],
        "search must retrieve document from dedicated table"
    );

    // 7. Verify cross-tenant isolation (RLS on dedicated table)
    let foreign_session = app.create_session().await;
    let foreign_tenant_id = app
        .seed_membership(foreign_session.user_id, "owner", "active")
        .await;

    // Search from tenant B finds nothing
    let (foreign_search_status, foreign_body) = app
        .request(
            Method::POST,
            &format!("/api/v1/tenants/{foreign_tenant_id}/search"),
            Some(json!({"query": "orbital synchronization propulsion"})),
            Some(&foreign_session.access_token),
        )
        .await;
    assert_eq!(foreign_search_status, StatusCode::OK);
    assert!(
        foreign_body["results"].as_array().unwrap().is_empty(),
        "other tenant must see 0 results"
    );

    // Direct repository query as tenant B against tenant A's dedicated table
    {
        let foreign_ctx = TenantContext::from_verified_membership(
            foreign_tenant_id,
            foreign_session.user_id,
            TenantRole::Owner,
        );
        let mut foreign_tx = begin_tenant_tx(&app.state.db, &foreign_ctx).await.unwrap();
        let foreign_rows: Vec<(Uuid,)> = sqlx::query_as(sqlx::AssertSqlSafe(
            format!("SELECT id FROM {schema_name}.document_chunks WHERE tenant_id = $1").as_str(),
        ))
        .bind(tenant_id)
        .fetch_all(&mut *foreign_tx)
        .await
        .unwrap();
        assert!(
            foreign_rows.is_empty(),
            "RLS must return 0 rows for other tenant on dedicated table"
        );
        foreign_tx.rollback().await.unwrap();
    }

    // 8. Verify immutability: UPDATE on chunks must be denied to app_runtime
    {
        let ctx =
            TenantContext::from_verified_membership(tenant_id, session.user_id, TenantRole::Owner);
        let mut tx = begin_tenant_tx(&app.state.db, &ctx).await.unwrap();
        let update_err = sqlx::query(sqlx::AssertSqlSafe(
            format!(
                "UPDATE {schema_name}.document_chunks SET content = 'hacked' WHERE document_id = $1"
            )
            .as_str(),
        ))
        .bind(document_id)
        .execute(&mut *tx)
        .await
        .unwrap_err();
        assert!(
            update_err.to_string().contains("permission denied")
                || update_err.to_string().contains("42501"),
            "app_runtime must not have UPDATE permission on document_chunks: {update_err}"
        );
        tx.rollback().await.unwrap();
    }

    // 9. Verify EXPLAIN on vector_candidates query as app_runtime
    {
        let ctx =
            TenantContext::from_verified_membership(tenant_id, session.user_id, TenantRole::Owner);
        let mut tx = begin_tenant_tx(&app.state.db, &ctx).await.unwrap();
        sqlx::query("SELECT set_config('hnsw.iterative_scan', 'relaxed_order', true)")
            .execute(&mut *tx)
            .await
            .unwrap();

        let dummy_vec = vec![0.1f32; 1536];
        let explain_sql = format!(
            "EXPLAIN (ANALYZE, BUFFERS) \
             WITH nearest AS MATERIALIZED ( \
                 SELECT id, embedding <=> $2::vector AS distance \
                   FROM {schema_name}.document_chunks \
                  WHERE tenant_id = $1 \
                  ORDER BY embedding <=> $2::vector \
                  LIMIT 10 \
             ) \
             SELECT id FROM nearest ORDER BY distance, id"
        );
        let plan_rows: Vec<(String,)> = sqlx::query_as(sqlx::AssertSqlSafe(explain_sql.as_str()))
            .bind(tenant_id)
            .bind(tenant_saas::repositories::chunk_repo::vector_literal(
                &dummy_vec,
            ))
            .fetch_all(&mut *tx)
            .await
            .unwrap();
        assert!(
            !plan_rows.is_empty(),
            "EXPLAIN must succeed for app_runtime"
        );
        tx.rollback().await.unwrap();
    }

    // 10. HTTP Document Deletion cleans up dedicated chunks
    let (del_status, _) = app
        .request(
            Method::DELETE,
            &format!("/api/v1/tenants/{tenant_id}/documents/{document_id}"),
            None,
            Some(&session.access_token),
        )
        .await;
    assert_eq!(del_status, StatusCode::NO_CONTENT);

    let (remaining_chunks,): (i64,) = sqlx::query_as(sqlx::AssertSqlSafe(
        format!("SELECT count(*) FROM {schema_name}.document_chunks WHERE document_id = $1")
            .as_str(),
    ))
    .bind(document_id)
    .fetch_one(app.owner())
    .await
    .unwrap();
    assert_eq!(
        remaining_chunks, 0,
        "delete must delete chunks from dedicated table"
    );

    // 11. Cleanup schema
    sqlx::query(sqlx::AssertSqlSafe(
        format!("DROP SCHEMA IF EXISTS {schema_name} CASCADE").as_str(),
    ))
    .execute(owner)
    .await
    .unwrap();
}
