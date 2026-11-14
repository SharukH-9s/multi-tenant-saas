//! # common-messaging
//!
//! The ingestion event and a small Redis Streams client.
//!
//! Redis is only a doorbell: it tells a worker that a job may exist. Every
//! decision about what work exists comes from the database (plan D1).

pub mod error;
pub mod event;
pub mod streams;

pub use error::StreamError;
pub use event::{EventError, IngestionEvent, EVENT_VERSION, INGESTION_REQUESTED};
pub use streams::{Delivery, StreamClient, StreamConfig, StreamStats};
