//! N-agent fan-out: one sender broadcasts to every recipient, and each
//! delivery is asserted to land on the bus exactly once.
//!
//! Runnable mirror of the "N-agent fan-out" item in `TODO.md`. Exercises the
//! kanal MPMC bus under more than two producers (one listen-loop per recipient
//! all forwarding onto the shared inbox).
//!
//! - One `hub` agent plus `N` worker agents.
//! - `hub` sends the same payload to each worker.
//! - A single consumer drains the bus, tallies deliveries per recipient, and
//!   verifies each worker received exactly one before shutting down.
//!
//! Self-terminating.
//!
//! # How to run
//! ```text
//! cargo run --example fanout
//! ```
//!
//! # Prerequisites
//! Docker must be running.

use std::collections::HashMap;

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use surrealdb_types::SurrealValue;
use tokio::time::{Duration, sleep, timeout};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use surrealdb_live_message::logger;
use surrealdb_live_message::subsystems::agents::Coalition;
use surrealdb_live_message::subsystems::sdb::{self, SurrealDBWrapper};

const HUB: &str = "hub";
const WORKERS: [&str; 4] = ["w1", "w2", "w3", "w4"];

const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const FANOUT_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
pub struct Notice {
    pub body: String,
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

    let mut names = vec![HUB.to_string()];
    names.extend(WORKERS.iter().map(|w| w.to_string()));
    let coalition = Coalition::<Notice>::new(names)
        .await
        .context("failed to build coalition")?;

    // Consumer: count exactly one delivery per worker.
    let inbox = coalition.inbox();
    let consumer = tokio::spawn(async move {
        let mut tally: HashMap<String, usize> = HashMap::new();
        while let Ok(d) = inbox.recv().await {
            let n = tally.entry(d.recipient.clone()).or_default();
            *n += 1;
            tracing::info!(recipient = %d.recipient, count = *n, "delivered");
            if tally.len() == WORKERS.len() && tally.values().all(|&c| c >= 1) {
                break;
            }
        }
        tally
    });

    // Broadcast: hub → every worker.
    let hub = coalition
        .agent(HUB)
        .await
        .context("hub missing from coalition")?;
    for w in WORKERS {
        hub.send(
            w,
            Notice {
                body: format!("hello {w}"),
            },
        )
        .await
        .with_context(|| format!("hub → {w} send"))?;
    }
    tracing::info!("hub broadcast to {} workers", WORKERS.len());

    match timeout(FANOUT_TIMEOUT, consumer).await {
        Ok(Ok(tally)) => {
            let ok = WORKERS.iter().all(|w| tally.get(*w) == Some(&1));
            if ok {
                tracing::info!("fan-out verified: each worker received exactly once");
            } else {
                tracing::error!(?tally, "fan-out mismatch");
            }
        }
        Ok(Err(e)) => tracing::error!("consumer task panicked: {e}"),
        Err(_) => tracing::error!("fan-out did not complete within {FANOUT_TIMEOUT:?}"),
    }

    coalition.shutdown().await;
    token.cancel();
    tracker.wait().await;
    tracing::info!("stopped cleanly");
    Ok(())
}
