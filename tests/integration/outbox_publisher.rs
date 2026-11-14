//! T4: the outbox publisher moves events to Redis without holding a
//! transaction, survives Redis outages, and dead-letters unusable payloads.
//! Every test holds `INGEST_GLOBAL` because the publisher acts on all tenants.

use std::collections::HashMap;

use axum::http::StatusCode;
use common_messaging::{StreamClient, StreamConfig};
use redis::{streams::StreamRangeReply, AsyncCommands};
use serde_json::{json, Value};
use tenant_saas::{
    repositories::outbox_repo,
    workers::outbox_publisher::{publish_batch, PublisherError},
};
use test_support::{TestApp, INGEST_GLOBAL};
use uuid::Uuid;

struct Harness {
    app: TestApp,
    streams: StreamClient,
    stream: String,
}

impl Harness {
    /// `None` (test skipped) when the database tests are skipped or Redis is
    /// unreachable.
    async fn new() -> Option<Self> {
        let app = TestApp::spawn().await?;
        let stream = format!("test:outbox:{}", Uuid::new_v4());
        let url = app.state.config.redis.url.clone();
        let streams =
            StreamClient::new_lazy(StreamConfig::new(url, stream.clone(), "test-group")).unwrap();
        if !streams.health().await {
            eprintln!("Redis unreachable: skipping outbox publisher test");
            return None;
        }
        Some(Self {
            app,
            streams,
            stream,
        })
    }

    /// `publish_batch` until a cycle handles nothing.
    async fn drain(&self) {
        drain_with(&self.app, &self.streams).await;
    }

    /// stream entries per `job_id`.
    async fn published_counts(&self) -> HashMap<Uuid, usize> {
        let client = redis::Client::open(self.app.state.config.redis.url.as_str()).unwrap();
        let mut conn = client.get_multiplexed_async_connection().await.unwrap();
        let reply: StreamRangeReply = conn.xrange_all(&self.stream).await.unwrap();
        let mut counts = HashMap::new();
        for entry in reply.ids {
            let raw: String = entry.get("event").expect("event field");
            let value: Value = serde_json::from_str(&raw).unwrap();
            let job_id: Uuid = value["job_id"].as_str().unwrap().parse().unwrap();
            *counts.entry(job_id).or_insert(0) += 1;
        }
        counts
    }

    async fn cleanup(&self) {
        let client = redis::Client::open(self.app.state.config.redis.url.as_str()).unwrap();
        if let Ok(mut conn) = client.get_multiplexed_async_connection().await {
            let _: Result<(), _> = conn.del(&self.stream).await;
        }
    }
}

async fn drain_with(app: &TestApp, streams: &StreamClient) {
    let cfg = &app.state.config.ingestion;
    for _ in 0..200 {
        let report = publish_batch(&app.runtime_pool, streams, cfg)
            .await
            .expect("publish_batch");
        if report.handled() == 0 {
            return;
        }
    }
    panic!("outbox did not drain");
}

fn payload(job_id: Uuid) -> Value {
    json!({
        "v": 1,
        "job_id": job_id.to_string(),
        "tenant_id": Uuid::new_v4().to_string(),
        "document_id": Uuid::new_v4().to_string(),
    })
}

/// Inserts an unpublished event through the owner pool. Returns
/// `(outbox_id, job_id)`.
async fn insert_event(app: &TestApp, payload_for: impl FnOnce(Uuid) -> Value) -> (Uuid, Uuid) {
    let job_id = Uuid::new_v4();
    let (id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO outbox_events (tenant_id, aggregate_type, aggregate_id, event_type, payload) \
         VALUES (NULL, 'ingestion_job', $1, 'ingestion.requested', $2) RETURNING id",
    )
    .bind(job_id)
    .bind(payload_for(job_id))
    .fetch_one(app.owner())
    .await
    .unwrap();
    (id, job_id)
}

#[derive(Debug)]
struct OutboxState {
    published: bool,
    dead_lettered: bool,
    claimed: bool,
    attempts: i32,
    last_error: Option<String>,
}

async fn state_of(app: &TestApp, id: Uuid) -> OutboxState {
    let (published, dead_lettered, claimed, attempts, last_error): (
        bool,
        bool,
        bool,
        i32,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT published_at IS NOT NULL, dead_lettered_at IS NOT NULL, \
                claimed_until IS NOT NULL, publish_attempts, last_error \
           FROM outbox_events WHERE id = $1",
    )
    .bind(id)
    .fetch_one(app.owner())
    .await
    .unwrap();
    OutboxState {
        published,
        dead_lettered,
        claimed,
        attempts,
        last_error,
    }
}

#[tokio::test]
async fn upload_then_drain_publishes_own_event_once() {
    let _global = INGEST_GLOBAL.lock().await;
    let Some(h) = Harness::new().await else {
        return;
    };
    let session = h.app.create_session().await;
    let tenant_id = h
        .app
        .seed_membership(session.user_id, "admin", "active")
        .await;
    let (status, body) = h
        .app
        .upload_file(
            tenant_id,
            &session.access_token,
            "notes.txt",
            b"outbox publisher upload",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");

    let (outbox_id, job_id): (Uuid, Uuid) =
        sqlx::query_as("SELECT id, aggregate_id FROM outbox_events WHERE tenant_id = $1")
            .bind(tenant_id)
            .fetch_one(h.app.owner())
            .await
            .unwrap();

    h.drain().await;

    assert_eq!(h.published_counts().await.get(&job_id), Some(&1));
    assert!(state_of(&h.app, outbox_id).await.published);
    h.cleanup().await;
}

#[tokio::test]
async fn concurrent_publishers_publish_each_event_once() {
    let _global = INGEST_GLOBAL.lock().await;
    let Some(h) = Harness::new().await else {
        return;
    };
    let mut jobs = Vec::new();
    for _ in 0..30 {
        jobs.push(insert_event(&h.app, payload).await.1);
    }

    tokio::join!(
        drain_with(&h.app, &h.streams),
        drain_with(&h.app, &h.streams)
    );

    let counts = h.published_counts().await;
    for job in jobs {
        assert_eq!(counts.get(&job), Some(&1), "job {job}");
    }
    h.cleanup().await;
}

#[tokio::test]
async fn crash_between_publish_and_mark_publishes_twice() {
    let _global = INGEST_GLOBAL.lock().await;
    let Some(h) = Harness::new().await else {
        return;
    };
    h.drain().await;
    let (outbox_id, job_id) = insert_event(&h.app, payload).await;

    // The "crashed" publisher: claim and XADD, but never mark.
    let claimed = outbox_repo::claim_batch(&h.app.runtime_pool, 100, 30)
        .await
        .unwrap();
    let ids: Vec<Uuid> = claimed.iter().map(|r| r.id).collect();
    let own = claimed
        .iter()
        .find(|r| r.id == outbox_id)
        .expect("own event claimed");
    let event =
        common_messaging::IngestionEvent::parse(&own.payload.to_string()).expect("valid payload");
    h.streams.publish(&event, own.id).await.unwrap();

    // While the claim is live, another publisher leaves the row alone.
    h.drain().await;
    assert_eq!(h.published_counts().await.get(&job_id), Some(&1));

    sqlx::query(
        "UPDATE outbox_events SET claimed_until = now() - interval '1 second' WHERE id = ANY($1)",
    )
    .bind(&ids)
    .execute(h.app.owner())
    .await
    .unwrap();
    h.drain().await;

    assert_eq!(h.published_counts().await.get(&job_id), Some(&2));
    let state = state_of(&h.app, outbox_id).await;
    assert!(state.published);
    assert_eq!(state.attempts, 2);
    h.cleanup().await;
}

#[tokio::test]
async fn dead_redis_releases_rows_and_recovery_drains_them() {
    let _global = INGEST_GLOBAL.lock().await;
    let Some(h) = Harness::new().await else {
        return;
    };
    h.drain().await;
    let mut rows = Vec::new();
    for _ in 0..3 {
        rows.push(insert_event(&h.app, payload).await);
    }

    let dead = StreamClient::new_lazy(StreamConfig::new(
        "redis://127.0.0.1:1",
        h.stream.clone(),
        "test-group",
    ))
    .unwrap();
    let result = publish_batch(&h.app.runtime_pool, &dead, &h.app.state.config.ingestion).await;
    assert!(
        matches!(result, Err(PublisherError::RedisUnavailable(_))),
        "{result:?}"
    );

    for (id, _) in &rows {
        let state = state_of(&h.app, *id).await;
        assert!(!state.claimed, "claim must be released");
        assert!(!state.published);
        assert!(!state.dead_lettered);
    }

    h.drain().await;
    let counts = h.published_counts().await;
    for (id, job) in &rows {
        assert!(state_of(&h.app, *id).await.published);
        assert_eq!(counts.get(job), Some(&1));
    }
    h.cleanup().await;
}

#[tokio::test]
async fn unsupported_payload_is_dead_lettered_and_others_publish() {
    let _global = INGEST_GLOBAL.lock().await;
    let Some(h) = Harness::new().await else {
        return;
    };
    let (bad_id, bad_job) = insert_event(&h.app, |_| json!({ "v": 99 })).await;
    let (good_id, good_job) = insert_event(&h.app, payload).await;

    h.drain().await;

    let bad = state_of(&h.app, bad_id).await;
    assert!(bad.dead_lettered && !bad.published, "{bad:?}");
    assert!(bad.last_error.is_some_and(|e| !e.is_empty()));
    assert!(state_of(&h.app, good_id).await.published);

    let counts = h.published_counts().await;
    assert_eq!(counts.get(&good_job), Some(&1));
    assert_eq!(counts.get(&bad_job), None);
    h.cleanup().await;
}

#[tokio::test]
async fn purge_published_only_deletes_old_published_rows() {
    let _global = INGEST_GLOBAL.lock().await;
    let Some(h) = Harness::new().await else {
        return;
    };
    let (old, _) = insert_event(&h.app, payload).await;
    let (recent, _) = insert_event(&h.app, payload).await;
    let (unpublished, _) = insert_event(&h.app, payload).await;
    sqlx::query("UPDATE outbox_events SET published_at = now() - interval '10 days' WHERE id = $1")
        .bind(old)
        .execute(h.app.owner())
        .await
        .unwrap();
    sqlx::query("UPDATE outbox_events SET published_at = now() WHERE id = $1")
        .bind(recent)
        .execute(h.app.owner())
        .await
        .unwrap();
    // An old unpublished row must survive too.
    sqlx::query("UPDATE outbox_events SET created_at = now() - interval '10 days' WHERE id = $1")
        .bind(unpublished)
        .execute(h.app.owner())
        .await
        .unwrap();

    let deleted = outbox_repo::purge_published(&h.app.runtime_pool, 7, 1000)
        .await
        .unwrap();
    assert!(deleted >= 1);

    let remaining: Vec<(Uuid,)> = sqlx::query_as("SELECT id FROM outbox_events WHERE id = ANY($1)")
        .bind(vec![old, recent, unpublished])
        .fetch_all(h.app.owner())
        .await
        .unwrap();
    let remaining: Vec<Uuid> = remaining.into_iter().map(|(id,)| id).collect();
    assert!(!remaining.contains(&old));
    assert!(remaining.contains(&recent));
    assert!(remaining.contains(&unpublished));
    h.cleanup().await;
}
