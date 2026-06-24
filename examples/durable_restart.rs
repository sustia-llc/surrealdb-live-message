//! Durable replay across a restart — the headline of the two-tier durable bus.
//!
//! Plain `LIVE` is at-most-once: a message published while a subscriber is gone
//! is lost forever. This example shows the opposite — a message sent to an agent
//! while its coalition is **fully shut down** is replayed when the coalition
//! restarts, because the `message` table is a durable, CHANGEFEED-backed log and
//! each agent resumes from a persisted versionstamp cursor.
//!
//! Flow:
//! 1. Round 1: bring up `alice` + `bob`, grab a sender handle, then
//!    `shutdown()` the coalition — `bob` is now offline. Messages and agents
//!    persist; nothing is deleted.
//! 2. While `bob` is offline, `alice` sends him a message. `send` only needs the
//!    global connection (still up), not a live subscriber. Under plain LIVE this
//!    delivery would be dropped on the floor.
//! 3. Round 2: restart the coalition. `bob` resumes from his saved cursor and
//!    the catch-up replays the message that arrived while he was down.
//!
//! Both rounds share one SurrealDB container (the connection is process-global),
//! exactly as a real restart against a persistent store would.
//!
//! # How to run
//! ```text
//! cargo run --example durable_restart
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
const REPLAY_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
pub struct Note {
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

    // Round 1: bring the coalition up, keep alice's sender handle, then take it
    // down so bob has no live subscription.
    let round1 = Coalition::<Note>::new(vec![ALICE.to_string(), BOB.to_string()])
        .await
        .context("round-1 coalition")?;
    let alice = round1.agent(ALICE).await.context("alice missing")?;
    round1.shutdown().await;
    tracing::info!("round 1 down — bob is now OFFLINE");

    // While bob is offline, alice sends him a message. With plain LIVE this
    // delivery would be lost; here it lands as a durable edge in the log.
    alice
        .send(
            BOB,
            Note {
                body: "sent while you were offline".to_string(),
            },
        )
        .await
        .context("alice → bob (offline) send")?;
    tracing::info!("alice sent bob a message while he was offline");

    // Round 2: restart. bob resumes from his persisted cursor; the durable-log
    // catch-up replays the missed message before `new()` even returns, so it is
    // already waiting on the bus.
    let round2 = Coalition::<Note>::new(vec![ALICE.to_string(), BOB.to_string()])
        .await
        .context("round-2 coalition (restart)")?;
    let inbox = round2.inbox();
    tracing::info!("round 2 up — bob restarted; awaiting replay …");

    match timeout(REPLAY_TIMEOUT, inbox.recv()).await {
        Ok(Ok(d)) => tracing::info!(
            recipient = %d.recipient,
            body = %d.message.payload.body,
            id = ?d.message.id,
            "REPLAYED on restart — durable delivery (plain LIVE would have lost this)"
        ),
        Ok(Err(_)) => tracing::error!("bus closed before the replay arrived"),
        Err(_) => tracing::error!("no replay within {REPLAY_TIMEOUT:?} — durability failed"),
    }

    // Standard shutdown order: coalition first, then the sdb token, then drain.
    round2.shutdown().await;
    token.cancel();
    tracker.wait().await;
    tracing::info!("stopped cleanly");
    Ok(())
}
