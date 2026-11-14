//! Background loops run by the worker binary. Each takes a `CancellationToken`
//! and exits when it fires.

pub mod ingestion_consumer;
pub mod outbox_publisher;
pub mod supervisor;
pub mod sweeper;
