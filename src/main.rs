use anyhow::Result;
use serde::{Deserialize, Serialize};
use surrealdb_types::SurrealValue;
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use surrealdb_live_message::logger;
use surrealdb_live_message::subsystems::agents::Coalition;
use surrealdb_live_message::subsystems::sdb::{self, SurrealDBWrapper};

const AGENT_BOB: &str = "bob";
const AGENT_ALICE: &str = "alice";

/// Example daemon payload type. The coalition is parameterised over this.
#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
#[allow(dead_code)]
pub struct DaemonPayload {
    pub kind: String,
    pub body: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    logger::setup();

    // Root cancellation token — everything hangs off this.
    let token = CancellationToken::new();
    let tracker = TaskTracker::new();

    // Spawn SurrealDB lifecycle as an independent task with a child token.
    let sdb_token = token.child_token();
    tracker.spawn(async move {
        if let Err(e) = sdb::sdb_task(sdb_token).await {
            tracing::error!("sdb_task failed: {}", e);
        }
    });

    // Wait until the database is ready before we build the coalition.
    SurrealDBWrapper::wait_until_ready().await?;

    // Spin up the coalition. Uses its own internal TaskTracker +
    // CancellationToken — the daemon does not need to know.
    let coalition =
        Coalition::<DaemonPayload>::new(vec![AGENT_ALICE.to_string(), AGENT_BOB.to_string()])
            .await?;

    // Pattern 1 from `rust-practical:async-lifecycle`: bare ctrl-c.
    tokio::signal::ctrl_c().await?;
    tracing::info!("ctrl-c received, shutting down.");

    // Shut the coalition first (drains agent listen loops) then the database.
    coalition.shutdown().await;
    token.cancel();
    tracker.close();
    tracker.wait().await;

    tracing::info!("daemon stopped cleanly.");
    Ok(())
}
