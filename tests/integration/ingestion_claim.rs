//! T5: claiming an ingestion job under a system tenant transaction, and
//! extending its lease.

use axum::http::{Method, StatusCode};
use chrono::{DateTime, Utc};
use common_database::begin_system_tenant_tx;
use tenant_saas::repositories::ingestion_repo::{claim, extend_lease, ClaimResult, ClaimedJob};
use test_support::TestApp;
use uuid::Uuid;

const WORKER: &str = "test-host:1:deadbeef";
const LEASE_SECS: u64 = 300;

struct Fixture {
    app: TestApp,
    token: String,
    tenant_id: Uuid,
    document_id: Uuid,
    job_id: Uuid,
}

async fn fixture() -> Option<Fixture> {
    let app = TestApp::spawn().await?;
    let session = app.create_session().await;
    let tenant_id = app
        .seed_membership(session.user_id, "admin", "active")
        .await;
    let content = format!("claim test {}", Uuid::new_v4());
    let (status, body) = app
        .upload_file(
            tenant_id,
            &session.access_token,
            "notes.txt",
            content.as_bytes(),
            None,
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
    Some(Fixture {
        token: session.access_token,
        app,
        tenant_id,
        document_id,
        job_id,
    })
}

impl Fixture {
    /// Claims as `tenant_id` in its own committed transaction.
    async fn claim_as(&self, tenant_id: Uuid) -> ClaimResult {
        let mut tx = begin_system_tenant_tx(&self.app.runtime_pool, tenant_id)
            .await
            .unwrap();
        let result = claim(&mut tx, self.job_id, self.document_id, WORKER, LEASE_SECS)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        result
    }

    async fn claim(&self) -> ClaimResult {
        self.claim_as(self.tenant_id).await
    }

    async fn claimed(&self) -> ClaimedJob {
        match self.claim().await {
            ClaimResult::Claimed(job) => job,
            other => panic!("expected Claimed, got {other:?}"),
        }
    }

    async fn job(&self) -> (String, i32, Option<Uuid>, Option<DateTime<Utc>>) {
        sqlx::query_as(
            "SELECT status, attempts, lease_token, locked_until FROM ingestion_jobs WHERE id = $1",
        )
        .bind(self.job_id)
        .fetch_one(self.app.owner())
        .await
        .unwrap()
    }

    async fn document_status(&self) -> String {
        sqlx::query_scalar("SELECT status FROM documents WHERE id = $1")
            .bind(self.document_id)
            .fetch_one(self.app.owner())
            .await
            .unwrap()
    }

    async fn owner_exec(&self, sql: &'static str) {
        sqlx::query(sql)
            .bind(self.job_id)
            .execute(self.app.owner())
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn claims_a_pending_job() {
    let Some(f) = fixture().await else { return };

    let job = f.claimed().await;

    assert_eq!(job.job_id, f.job_id);
    assert_eq!(job.tenant_id, f.tenant_id);
    assert_eq!(job.document_id, f.document_id);
    assert_eq!(job.attempts, 1);
    assert_eq!(job.max_attempts, 5);
    assert_eq!(job.mime_type, "text/plain");
    assert!(job.size_bytes > 0);
    assert_eq!(job.content_hash.len(), 64);
    assert!(!job.object_key.is_empty());

    let (status, attempts, lease, locked_until) = f.job().await;
    assert_eq!(status, "processing");
    assert_eq!(attempts, 1);
    assert_eq!(lease, Some(job.lease_token));
    assert!(locked_until.unwrap() > Utc::now());
    assert_eq!(f.document_status().await, "processing");
}

#[tokio::test]
async fn concurrent_claims_yield_exactly_one_winner() {
    let Some(f) = fixture().await else { return };

    let (a, b) = tokio::join!(f.claim(), f.claim());

    let claimed = [&a, &b]
        .iter()
        .filter(|r| matches!(r, ClaimResult::Claimed(_)))
        .count();
    let not_claimable = [&a, &b]
        .iter()
        .filter(|r| matches!(r, ClaimResult::NotClaimable))
        .count();
    assert_eq!((claimed, not_claimable), (1, 1), "{a:?} / {b:?}");
    assert_eq!(f.job().await.1, 1, "attempts counted once");
}

#[tokio::test]
async fn expired_lease_can_be_claimed_again_with_a_new_token() {
    let Some(f) = fixture().await else { return };
    let first = f.claimed().await;

    // A live lease is not claimable.
    assert!(matches!(f.claim().await, ClaimResult::NotClaimable));

    f.owner_exec(
        "UPDATE ingestion_jobs SET locked_until = now() - interval '1 second' WHERE id = $1",
    )
    .await;
    let second = f.claimed().await;

    assert_ne!(first.lease_token, second.lease_token);
    assert_eq!(second.attempts, 2);
}

#[tokio::test]
async fn retry_scheduled_job_is_not_claimable() {
    let Some(f) = fixture().await else { return };
    f.owner_exec("UPDATE ingestion_jobs SET status = 'retry_scheduled' WHERE id = $1")
        .await;

    assert!(matches!(f.claim().await, ClaimResult::NotClaimable));
    assert_eq!(f.job().await.0, "retry_scheduled");
}

#[tokio::test]
async fn job_out_of_attempts_is_not_claimable() {
    let Some(f) = fixture().await else { return };
    f.owner_exec("UPDATE ingestion_jobs SET attempts = max_attempts WHERE id = $1")
        .await;

    assert!(matches!(f.claim().await, ClaimResult::NotClaimable));
    assert_eq!(f.job().await.0, "pending");
}

#[tokio::test]
async fn another_tenants_transaction_cannot_claim() {
    let Some(f) = fixture().await else { return };
    let other_session = f.app.create_session().await;
    let other_tenant = f
        .app
        .seed_membership(other_session.user_id, "admin", "active")
        .await;

    let result = f.claim_as(other_tenant).await;

    assert!(matches!(result, ClaimResult::Mismatch), "{result:?}");
    let (status, attempts, lease, _) = f.job().await;
    assert_eq!((status.as_str(), attempts, lease), ("pending", 0, None));
    assert_eq!(f.document_status().await, "pending");
}

#[tokio::test]
async fn deleted_document_cancels_the_job() {
    let Some(f) = fixture().await else { return };
    let (status, body) = f
        .app
        .request(
            Method::DELETE,
            &format!(
                "/api/v1/tenants/{}/documents/{}",
                f.tenant_id, f.document_id
            ),
            None,
            Some(&f.token),
        )
        .await;
    assert!(status.is_success(), "{status} {body}");

    let result = f.claim().await;

    assert!(matches!(result, ClaimResult::DocumentGone), "{result:?}");
    let (status, _, lease, locked_until) = f.job().await;
    assert_eq!(status, "cancelled");
    assert!(lease.is_none() && locked_until.is_none());
    assert_ne!(f.document_status().await, "processing");
}

#[tokio::test]
async fn extend_lease_only_works_with_the_current_token() {
    let Some(f) = fixture().await else { return };
    let job = f.claimed().await;
    let before = f.job().await.3.unwrap();

    let stale = extend_lease(
        &f.app.runtime_pool,
        f.tenant_id,
        f.job_id,
        Uuid::new_v4(),
        LEASE_SECS * 2,
    )
    .await
    .unwrap();
    assert!(!stale);
    assert_eq!(
        f.job().await.3.unwrap(),
        before,
        "stale token changes nothing"
    );

    let ours = extend_lease(
        &f.app.runtime_pool,
        f.tenant_id,
        f.job_id,
        job.lease_token,
        LEASE_SECS * 2,
    )
    .await
    .unwrap();
    assert!(ours);
    assert!(f.job().await.3.unwrap() > before);
}
