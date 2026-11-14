//! Real-Redis tests. Each test uses its own stream key and group, and skips
//! with a message when Redis is not reachable (like the DB-backed tests).

use std::time::{Duration, Instant};

use common_messaging::{EventError, IngestionEvent, StreamClient, StreamConfig, StreamError};
use uuid::Uuid;

struct Fixture {
    client: StreamClient,
    url: String,
    stream: String,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let _ = dotenvy::dotenv();
        let url = std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://localhost:6379".into());
        let stream = format!("test:ingest:{}", Uuid::new_v4().simple());
        let client =
            StreamClient::new_lazy(StreamConfig::new(url.clone(), stream.clone(), "test-group"))
                .expect("client");
        if !client.health().await {
            eprintln!("Redis at {url} is unreachable: skipping");
            return None;
        }
        Some(Self {
            client,
            url,
            stream,
        })
    }

    async fn raw(&self) -> redis::aio::MultiplexedConnection {
        redis::Client::open(self.url.as_str())
            .unwrap()
            .get_multiplexed_async_connection()
            .await
            .unwrap()
    }

    async fn cleanup(self) {
        let mut conn = self.raw().await;
        let _: i64 = redis::cmd("DEL")
            .arg(&self.stream)
            .query_async(&mut conn)
            .await
            .unwrap();
    }
}

fn event() -> IngestionEvent {
    IngestionEvent::new(Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4())
}

/// A client whose block time is short, so empty reads return quickly.
fn fast_client(f: &Fixture) -> StreamClient {
    let mut cfg = StreamConfig::new(f.url.clone(), f.stream.clone(), "test-group");
    cfg.block_ms = 50;
    StreamClient::new_lazy(cfg).unwrap()
}

#[tokio::test]
async fn ensure_group_is_idempotent() {
    let Some(f) = Fixture::new().await else {
        return;
    };
    f.client.ensure_group().await.unwrap();
    f.client.ensure_group().await.unwrap();
    f.cleanup().await;
}

#[tokio::test]
async fn message_published_before_group_exists_is_delivered() {
    let Some(f) = Fixture::new().await else {
        return;
    };
    let sent = event();
    f.client.publish(&sent, Uuid::new_v4()).await.unwrap();
    f.client.ensure_group().await.unwrap();

    let got = fast_client(&f).read("c1", 10).await.unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].event.as_ref().unwrap(), &sent);
    f.cleanup().await;
}

#[tokio::test]
async fn ack_removes_message_from_pending() {
    let Some(f) = Fixture::new().await else {
        return;
    };
    f.client.ensure_group().await.unwrap();
    f.client.publish(&event(), Uuid::new_v4()).await.unwrap();
    let reader = fast_client(&f);

    let first = reader.read("c1", 10).await.unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(f.client.stats().await.unwrap().pending, 1);

    f.client.ack(&[first[0].stream_id.clone()]).await.unwrap();
    assert_eq!(f.client.stats().await.unwrap().pending, 0);
    assert!(reader.read("c1", 10).await.unwrap().is_empty());
    f.client.ack(&[]).await.unwrap();
    f.cleanup().await;
}

#[tokio::test]
async fn unacked_message_can_be_reclaimed_by_another_consumer() {
    let Some(f) = Fixture::new().await else {
        return;
    };
    f.client.ensure_group().await.unwrap();
    let sent = event();
    f.client.publish(&sent, Uuid::new_v4()).await.unwrap();
    let reader = fast_client(&f);

    let first = reader.read("dead-consumer", 10).await.unwrap();
    assert_eq!(first.len(), 1);

    let (next, claimed) = f.client.reclaim("c2", 0, "0-0", 10).await.unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].stream_id, first[0].stream_id);
    assert_eq!(claimed[0].event.as_ref().unwrap(), &sent);
    assert_eq!(next, "0-0", "scan finished");
    f.cleanup().await;
}

#[tokio::test]
async fn garbage_entries_do_not_poison_the_batch() {
    let Some(f) = Fixture::new().await else {
        return;
    };
    f.client.ensure_group().await.unwrap();
    let good = event();
    let mut conn = f.raw().await;

    let _: String = redis::cmd("XADD")
        .arg(&f.stream)
        .arg("*")
        .arg("event")
        .arg("not json")
        .query_async(&mut conn)
        .await
        .unwrap();
    let _: String = redis::cmd("XADD")
        .arg(&f.stream)
        .arg("*")
        .arg("other")
        .arg("field")
        .query_async(&mut conn)
        .await
        .unwrap();
    f.client.publish(&good, Uuid::new_v4()).await.unwrap();

    let got = fast_client(&f).read("c1", 10).await.unwrap();
    assert_eq!(got.len(), 3);
    assert!(matches!(got[0].event, Err(EventError::Malformed(_))));
    assert!(matches!(got[1].event, Err(EventError::Malformed(_))));
    assert_eq!(got[2].event.as_ref().unwrap(), &good);
    f.cleanup().await;
}

/// Proves D6: the default 500 ms response timeout would turn this into an error.
#[tokio::test]
async fn blocking_read_on_empty_stream_returns_empty_not_timeout() {
    let Some(f) = Fixture::new().await else {
        return;
    };
    f.client.ensure_group().await.unwrap();

    let started = Instant::now();
    let got = f.client.read("c1", 10).await.expect("not a timeout error");
    let elapsed = started.elapsed();

    assert!(got.is_empty());
    assert!(
        elapsed >= Duration::from_millis(1800) && elapsed < Duration::from_secs(4),
        "blocked for {elapsed:?}"
    );
    f.cleanup().await;
}

#[tokio::test]
async fn stream_is_capped_with_maxlen() {
    let Some(f) = Fixture::new().await else {
        return;
    };
    let mut cfg = StreamConfig::new(f.url.clone(), f.stream.clone(), "test-group");
    cfg.max_len = 10;
    let client = StreamClient::new_lazy(cfg).unwrap();
    client.ensure_group().await.unwrap();
    for _ in 0..300 {
        client.publish(&event(), Uuid::new_v4()).await.unwrap();
    }
    // Approximate trimming works on whole macro-nodes, so allow slack, but it
    // must be far below 300.
    let len = client.stats().await.unwrap().len;
    assert!(len < 300, "stream was not trimmed: {len}");
    f.cleanup().await;
}

#[tokio::test]
async fn dead_redis_is_unavailable_and_health_is_false() {
    let client = StreamClient::new_lazy(StreamConfig::new(
        "redis://127.0.0.1:1",
        "test:ingest:dead",
        "g",
    ))
    .unwrap();

    // Each call must fail fast (a refused connection can take ~2 s on Windows).
    let bound = Duration::from_secs(3);
    assert!(!tokio::time::timeout(bound, client.health()).await.unwrap());
    let publish = tokio::time::timeout(bound, client.publish(&event(), Uuid::new_v4())).await;
    assert!(matches!(publish.unwrap(), Err(StreamError::Unavailable(_))));
    let group = tokio::time::timeout(bound, client.ensure_group()).await;
    assert!(matches!(group.unwrap(), Err(StreamError::Unavailable(_))));
    let read = tokio::time::timeout(bound, client.read("c", 1)).await;
    assert!(matches!(read.unwrap(), Err(StreamError::Unavailable(_))));
    let ack = tokio::time::timeout(bound, client.ack(&["1-0".into()])).await;
    assert!(matches!(ack.unwrap(), Err(StreamError::Unavailable(_))));
}
