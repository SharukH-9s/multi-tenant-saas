//! Redis Streams client for the ingestion doorbell.
//!
//! Two connections (plan D6): `cmd` for short commands with a 2 s timeout, and
//! `reader` for the blocking `XREADGROUP`, whose response timeout is longer than
//! the block time. A multiplexed connection runs commands in order, so a
//! blocking read on the shared connection would delay every ack behind it.

use std::time::Duration;

use redis::{
    aio::{ConnectionManager, ConnectionManagerConfig},
    streams::{StreamAutoClaimOptions, StreamId, StreamMaxlen, StreamReadOptions, StreamReadReply},
    AsyncCommands, RedisError, Value,
};
use uuid::Uuid;

use crate::{
    error::StreamError,
    event::{EventError, IngestionEvent},
};

/// `XREADGROUP BLOCK` time.
pub const BLOCK_MS: u64 = 2000;
/// Approximate stream cap. Trimming can drop a pending entry; the sweeper's
/// lost-doorbell rule recovers it (plan D1, D6).
pub const MAX_LEN: usize = 100_000;

const CMD_TIMEOUT: Duration = Duration::from_secs(2);
const EVENT_FIELD: &str = "event";
const OUTBOX_FIELD: &str = "outbox_id";

#[derive(Debug, Clone)]
pub struct StreamConfig {
    pub url: String,
    pub stream: String,
    pub group: String,
    pub block_ms: u64,
    pub max_len: usize,
}

impl StreamConfig {
    pub fn new(
        url: impl Into<String>,
        stream: impl Into<String>,
        group: impl Into<String>,
    ) -> Self {
        Self {
            url: url.into(),
            stream: stream.into(),
            group: group.into(),
            block_ms: BLOCK_MS,
            max_len: MAX_LEN,
        }
    }
}

/// One stream entry. A bad payload does not fail the read: it is returned
/// with `event: Err(..)` so the caller can ack it and move on.
#[derive(Debug)]
pub struct Delivery {
    pub stream_id: String,
    pub event: Result<IngestionEvent, EventError>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamStats {
    /// Entries currently in the stream (`XLEN`).
    pub len: u64,
    /// Delivered but not yet acknowledged, group-wide.
    pub pending: u64,
}

#[derive(Clone)]
pub struct StreamClient {
    cmd: ConnectionManager,
    reader: ConnectionManager,
    cfg: StreamConfig,
}

impl StreamClient {
    /// Builds both connections lazily: no I/O happens here, so this works while
    /// Redis is down.
    pub fn new_lazy(cfg: StreamConfig) -> Result<Self, StreamError> {
        let client = redis::Client::open(cfg.url.as_str())?;
        let base = || {
            ConnectionManagerConfig::new()
                .set_connection_timeout(Some(CMD_TIMEOUT))
                .set_number_of_retries(0)
        };
        let cmd = ConnectionManager::new_lazy_with_config(
            client.clone(),
            base().set_response_timeout(Some(CMD_TIMEOUT)),
        )?;
        let reader = ConnectionManager::new_lazy_with_config(
            client,
            base().set_response_timeout(Some(Duration::from_millis(cfg.block_ms) + CMD_TIMEOUT)),
        )?;
        Ok(Self { cmd, reader, cfg })
    }

    pub fn config(&self) -> &StreamConfig {
        &self.cfg
    }

    /// `XGROUP CREATE <stream> <group> 0 MKSTREAM`. The start id is `0`, not
    /// `$`, so events published before the first worker started are not
    /// skipped. An existing group is success.
    pub async fn ensure_group(&self) -> Result<(), StreamError> {
        let mut conn = self.cmd.clone();
        let result: Result<(), RedisError> = conn
            .xgroup_create_mkstream(&self.cfg.stream, &self.cfg.group, "0")
            .await;
        match result {
            Ok(()) => Ok(()),
            Err(e) if e.code() == Some("BUSYGROUP") => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// `XADD <stream> MAXLEN ~ <max_len> * event <json> outbox_id <uuid>`.
    /// Returns the stream id.
    pub async fn publish(
        &self,
        event: &IngestionEvent,
        outbox_id: Uuid,
    ) -> Result<String, StreamError> {
        let mut conn = self.cmd.clone();
        let id: Option<String> = conn
            .xadd_maxlen(
                &self.cfg.stream,
                StreamMaxlen::Approx(self.cfg.max_len),
                "*",
                &[
                    (EVENT_FIELD, event.to_json()),
                    (OUTBOX_FIELD, outbox_id.to_string()),
                ],
            )
            .await?;
        id.ok_or_else(|| StreamError::Protocol("XADD returned nil".into()))
    }

    /// `XREADGROUP GROUP g consumer COUNT n BLOCK block_ms STREAMS stream >`.
    /// Returns an empty vector when the block time passes with nothing new.
    pub async fn read(&self, consumer: &str, count: usize) -> Result<Vec<Delivery>, StreamError> {
        let options = StreamReadOptions::default()
            .group(&self.cfg.group, consumer)
            .count(count)
            .block(self.cfg.block_ms as usize);
        let mut conn = self.reader.clone();
        let reply: Option<StreamReadReply> = conn
            .xread_options(&[&self.cfg.stream], &[">"], &options)
            .await?;
        Ok(reply
            .map(|r| {
                r.keys
                    .into_iter()
                    .flat_map(|key| key.ids)
                    .map(delivery)
                    .collect()
            })
            .unwrap_or_default())
    }

    /// `XACK`. A no-op for an empty slice.
    pub async fn ack(&self, ids: &[String]) -> Result<(), StreamError> {
        if ids.is_empty() {
            return Ok(());
        }
        let mut conn = self.cmd.clone();
        let _: usize = conn.xack(&self.cfg.stream, &self.cfg.group, ids).await?;
        Ok(())
    }

    /// `XAUTOCLAIM`: takes over entries idle for at least `min_idle_ms`.
    /// Start with cursor `"0-0"`; pass the returned cursor to continue.
    pub async fn reclaim(
        &self,
        consumer: &str,
        min_idle_ms: u64,
        cursor: &str,
        count: usize,
    ) -> Result<(String, Vec<Delivery>), StreamError> {
        let mut conn = self.cmd.clone();
        let reply = conn
            .xautoclaim_options(
                &self.cfg.stream,
                &self.cfg.group,
                consumer,
                min_idle_ms,
                cursor,
                StreamAutoClaimOptions::default().count(count),
            )
            .await?;
        let reply: redis::streams::StreamAutoClaimReply = reply;
        Ok((
            reply.next_stream_id,
            reply.claimed.into_iter().map(delivery).collect(),
        ))
    }

    /// `XLEN` plus the group-wide `XPENDING` count.
    pub async fn stats(&self) -> Result<StreamStats, StreamError> {
        let mut conn = self.cmd.clone();
        let len: u64 = conn.xlen(&self.cfg.stream).await?;
        let pending: redis::streams::StreamPendingReply =
            conn.xpending(&self.cfg.stream, &self.cfg.group).await?;
        Ok(StreamStats {
            len,
            pending: pending.count() as u64,
        })
    }

    /// `PING`, bounded to 3 s. Never errors: false means unreachable.
    pub async fn health(&self) -> bool {
        let mut conn = self.cmd.clone();
        let command = redis::cmd("PING");
        let ping = command.query_async::<String>(&mut conn);
        matches!(
            tokio::time::timeout(Duration::from_secs(3), ping).await,
            Ok(Ok(_))
        )
    }
}

fn delivery(entry: StreamId) -> Delivery {
    let event = match entry.map.get(EVENT_FIELD) {
        Some(Value::BulkString(bytes)) => match std::str::from_utf8(bytes) {
            Ok(raw) => IngestionEvent::parse(raw),
            Err(_) => Err(EventError::Malformed("event is not UTF-8")),
        },
        Some(Value::SimpleString(raw)) => IngestionEvent::parse(raw),
        _ => Err(EventError::Malformed("missing event field")),
    };
    Delivery {
        stream_id: entry.id,
        event,
    }
}
