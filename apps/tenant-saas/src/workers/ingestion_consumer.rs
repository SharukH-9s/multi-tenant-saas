//! Ingestion consumer: reads messages from the Redis stream and processes them.

use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use common_messaging::{Delivery, StreamClient};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::services::ingestion_service::{self, IngestionDeps};

const BACKOFF: Duration = Duration::from_secs(1);

/// Jobs currently being processed by this process. Read by the heartbeat.
/// One consumer runs per worker process, so a process-wide counter is enough.
static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

/// Number of deliveries this process is processing right now.
pub fn jobs_in_flight() -> usize {
    IN_FLIGHT.load(Ordering::Relaxed)
}

/// Decrements [`IN_FLIGHT`] on drop, so a panicking job still releases its count.
struct InFlightGuard;

impl InFlightGuard {
    fn new() -> Self {
        IN_FLIGHT.fetch_add(1, Ordering::Relaxed);
        Self
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        IN_FLIGHT.fetch_sub(1, Ordering::Relaxed);
    }
}

pub async fn run(deps: Arc<IngestionDeps>, streams: Arc<StreamClient>, cancel: CancellationToken) {
    let concurrency = deps.state.config.ingestion.worker_concurrency.max(1);
    let slots = Arc::new(Semaphore::new(concurrency));
    let mut reclaim_interval = tokio::time::interval(Duration::from_secs(30));
    reclaim_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut reclaim_cursor = "0-0".to_string();

    info!(concurrency, "ingestion consumer started");

    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,

            _ = reclaim_interval.tick() => {
                let free = slots.available_permits();
                if free > 0 {
                    let min_idle_ms = deps.state.config.ingestion.job_lease_secs * 1000;
                    match streams.reclaim(&deps.worker_id, min_idle_ms, &reclaim_cursor, free).await {
                        Ok((next_cursor, deliveries)) => {
                            reclaim_cursor = next_cursor;
                            dispatch_deliveries(&deps, &streams, &slots, deliveries, &cancel).await;
                        }
                        Err(e) => {
                            warn!(error = %e, "failed to reclaim stream entries");
                        }
                    }
                }
            }

            permit = slots.clone().acquire_owned() => {
                let permit = match permit {
                    Ok(p) => p,
                    Err(_) => return, // semaphore closed
                };

                let free = slots.available_permits() + 1; // including the permit we just acquired
                match streams.read(&deps.worker_id, free).await {
                    Ok(deliveries) if deliveries.is_empty() => {
                        drop(permit);
                    }
                    Ok(mut deliveries) => {
                        // First delivery uses our acquired permit
                        let first = deliveries.remove(0);
                        spawn_delivery(&deps, &streams, permit, first);

                        // Any remaining deliveries acquire their own permits
                        dispatch_deliveries(&deps, &streams, &slots, deliveries, &cancel).await;
                    }
                    Err(e) => {
                        warn!(error = %e, "stream read failed, backing off");
                        drop(permit);
                        tokio::select! {
                            _ = cancel.cancelled() => return,
                            _ = tokio::time::sleep(BACKOFF) => {}
                        }
                    }
                }
            }
        }
    }
}

async fn dispatch_deliveries(
    deps: &Arc<IngestionDeps>,
    streams: &Arc<StreamClient>,
    slots: &Arc<Semaphore>,
    deliveries: Vec<Delivery>,
    cancel: &CancellationToken,
) {
    for delivery in deliveries {
        let permit = match slots.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    p = slots.clone().acquire_owned() => match p {
                        Ok(p) => p,
                        Err(_) => return,
                    }
                }
            }
        };
        spawn_delivery(deps, streams, permit, delivery);
    }
}

fn spawn_delivery(
    deps: &Arc<IngestionDeps>,
    streams: &Arc<StreamClient>,
    permit: tokio::sync::OwnedSemaphorePermit,
    delivery: Delivery,
) {
    let deps = deps.clone();
    let streams = streams.clone();
    let stream_id = delivery.stream_id;

    tokio::spawn(async move {
        let _permit = permit;
        let _in_flight = InFlightGuard::new();
        match delivery.event {
            Err(e) => {
                warn!(stream_id = %stream_id, error = %e, "poison message in stream");
                let _ = streams.ack(&[stream_id]).await;
            }
            Ok(event) => {
                let outcome = ingestion_service::process(&deps, &event).await;
                if outcome.should_ack() {
                    if let Err(e) = streams.ack(std::slice::from_ref(&stream_id)).await {
                        warn!(stream_id = %stream_id, error = %e, "failed to ack stream message");
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_flight_guard_counts_up_and_down() {
        let before = jobs_in_flight();
        let a = InFlightGuard::new();
        let b = InFlightGuard::new();
        assert!(jobs_in_flight() >= before + 2);
        drop(a);
        drop(b);
        // Other tests may hold guards concurrently, so only check we returned
        // to at most the starting value plus anything they added.
        assert!(jobs_in_flight() < before + 2);
    }
}
