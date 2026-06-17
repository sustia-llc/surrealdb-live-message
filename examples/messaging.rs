//! Request/response round-trip over the coalition delivery bus.
//!
//! The two shutdown examples only show a one-way `alice → bob` greeting. This
//! example shows the actual messaging contract: a reply travelling back to the
//! original sender, both legs observed off a single [`Coalition::inbox`].
//!
//! - `alice` sends a `Chat { from: "alice", body: "ping" }` to `bob`.
//! - A consumer task drains the bus. When it sees a delivery addressed to
//!   `bob`, it has `bob` reply to `payload.from` — the sender name is carried
//!   *in the payload*, which is cleaner than extracting it from the edge's
//!   `in` `RecordId`.
//! - When the reply lands back on `alice`, the round-trip is complete and the
//!   example shuts down.
//!
//! Self-terminating: runs to completion, then performs the standard
//! `coalition.shutdown()` → cancel sdb token → drain sequence.
//!
//! # How to run
//! ```text
//! cargo run --example messaging
//! ```
//!
//! # Prerequisites
//! Docker must be running. The library spins up a SurrealDB container on start.

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use surrealdb_types::SurrealValue;
use tokio::time::{Duration, sleep, timeout};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use surrealdb_live_message::logger;
use surrealdb_live_message::subsystems::agents::Coalition;
use surrealdb_live_message::subsystems::sdb::{self, SurrealDBWrapper};

const ALICE: &str = "alice";
const BOB: &str = "bob";

const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const ROUNDTRIP_TIMEOUT: Duration = Duration::from_secs(15);

/// Chat payload. `from` carries the sender's name so the recipient can reply
/// without parsing the edge's `in` `RecordId`.
#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
pub struct Chat {
    pub from: String,
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

    // Startup supervision — DB ready, sdb exit, or hard timeout.
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

    let coalition = Coalition::<Chat>::new(vec![ALICE.to_string(), BOB.to_string()])
        .await
        .context("failed to build coalition")?;

    // Bob replies to whoever pinged him. Cloned out of the coalition so the
    // consumer task owns a handle for sending.
    let bob = coalition
        .agent(BOB)
        .await
        .context("bob missing from coalition")?;

    // Consumer: observe the request, fire the reply, observe the reply, done.
    let inbox = coalition.inbox();
    let consumer = tokio::spawn(async move {
        while let Ok(d) = inbox.recv().await {
            tracing::info!(
                recipient = %d.recipient,
                from = %d.message.payload.from,
                body = %d.message.payload.body,
                "delivered"
            );
            match d.recipient.as_str() {
                BOB => {
                    let reply = Chat {
                        from: BOB.to_string(),
                        body: format!("re: {}", d.message.payload.body),
                    };
                    if let Err(e) = bob.send(&d.message.payload.from, reply).await {
                        tracing::error!("bob reply failed: {e}");
                    }
                }
                // Reply landed back on the original sender — round-trip done.
                _ => break,
            }
        }
    });

    // Kick off the round-trip.
    let alice = coalition
        .agent(ALICE)
        .await
        .context("alice missing from coalition")?;
    alice
        .send(
            BOB,
            Chat {
                from: ALICE.to_string(),
                body: "ping".to_string(),
            },
        )
        .await
        .context("alice → bob send")?;
    tracing::info!("alice pinged bob; awaiting reply …");

    match timeout(ROUNDTRIP_TIMEOUT, consumer).await {
        Ok(_) => tracing::info!("round-trip complete"),
        Err(_) => tracing::error!("round-trip did not complete within {ROUNDTRIP_TIMEOUT:?}"),
    }

    // Standard shutdown order: coalition first (edges deleted while DB is up),
    // then the sdb token, then drain.
    coalition.shutdown().await;
    token.cancel();
    tracker.wait().await;
    tracing::info!("stopped cleanly");
    Ok(())
}
