//! MPMC worker pool over the delivery bus, plus the sync-handler bridge.
//!
//! The whole point of the kanal MPMC bus is that [`Coalition::inbox`] can be
//! cloned and pulled from concurrently. This example fans a burst of messages
//! out across:
//!
//! - `N_ASYNC` async workers, each holding its own `inbox()` clone; kanal
//!   load-balances deliveries across them (each delivery goes to exactly one
//!   worker).
//! - one **sync** worker bridged via `inbox().to_sync()`, running inside
//!   `spawn_blocking` so a blocking handler never stalls the async runtime —
//!   the pattern `CLAUDE.md` advertises but no other example shows.
//!
//! All workers exit when the bus closes: `coalition.shutdown()` cancels the
//! listen-loops, the last sender drops, and every `recv()` returns `Err`.
//!
//! Self-terminating.
//!
//! # How to run
//! ```text
//! cargo run --example worker_pool
//! ```
//!
//! # Prerequisites
//! Docker must be running.

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use surrealdb_types::SurrealValue;
use tokio::time::{Duration, sleep};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use surrealdb_live_message::logger;
use surrealdb_live_message::subsystems::agents::Coalition;
use surrealdb_live_message::subsystems::sdb::{self, SurrealDBWrapper};

const SENDER: &str = "producer";
const SINK: &str = "sink";
const N_ASYNC: usize = 3;
const N_MESSAGES: usize = 12;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
pub struct Job {
    pub seq: i64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    logger::setup();

    let token = CancellationToken::new();
    let tracker = TaskTracker::new();

    let sdb_token = token.child_token();
    let mut sdb_handle = tracker.spawn(async move { sdb::sdb_task(sdb_token).await });
    tracker.close();

    tokio::select! {
        result = SurrealDBWrapper::wait_until_ready() => {
            result.context("SurrealDB ready signal failed")?;
        }
        result = &mut sdb_handle => {
            token.cancel();
            anyhow::bail!("sdb_task exited during startup: {result:?}");
        }
        _ = sleep(STARTUP_TIMEOUT) => {
            token.cancel();
            tracker.wait().await;
            anyhow::bail!("SurrealDB not ready within {STARTUP_TIMEOUT:?}");
        }
    }
    tracing::info!("SurrealDB ready.");

    let coalition = Coalition::<Job>::new(vec![SENDER.to_string(), SINK.to_string()])
        .await
        .context("failed to build coalition")?;

    // Async worker pool — each worker owns a clone of the inbox. kanal hands
    // each delivery to exactly one of them.
    let mut workers = Vec::with_capacity(N_ASYNC);
    for id in 0..N_ASYNC {
        let rx = coalition.inbox();
        workers.push(tokio::spawn(async move {
            let mut handled = 0usize;
            while let Ok(d) = rx.recv().await {
                handled += 1;
                tracing::info!(worker = id, seq = d.message.payload.seq, "async handled");
            }
            tracing::info!(worker = id, handled, "async worker done");
        }));
    }

    // Sync bridge — a blocking consumer driven off the same bus. `to_sync()`
    // converts the cloned async receiver into a sync `Receiver`; `recv()` then
    // blocks the worker thread, so it must live on `spawn_blocking`, never on
    // an async task.
    let sync_rx = coalition.inbox().to_sync();
    let sync_worker = tokio::task::spawn_blocking(move || {
        let mut handled = 0usize;
        while let Ok(d) = sync_rx.recv() {
            handled += 1;
            tracing::info!(seq = d.message.payload.seq, "sync handled");
        }
        tracing::info!(handled, "sync worker done");
    });

    // Burst of work, all addressed to the single sink agent. Its listen-loop
    // forwards every delivery onto the shared bus, where the pool competes.
    let producer = coalition
        .agent(SENDER)
        .await
        .context("producer missing from coalition")?;
    for seq in 0..N_MESSAGES as i64 {
        producer
            .send(SINK, Job { seq })
            .await
            .with_context(|| format!("send seq {seq}"))?;
    }
    tracing::info!("queued {N_MESSAGES} jobs across {N_ASYNC} async workers + 1 sync worker");

    // Let live-query notifications propagate and the pool drain before closing.
    sleep(Duration::from_secs(3)).await;

    // Shutdown closes the bus → every worker's recv() returns Err → they exit.
    coalition.shutdown().await;
    for w in workers {
        let _ = w.await;
    }
    let _ = sync_worker.await;

    token.cancel();
    tracker.wait().await;
    tracing::info!("stopped cleanly");
    Ok(())
}
