use miette::Result;
use tokio::time::Duration;
use tokio_graceful_shutdown::{SubsystemBuilder, Toplevel};

use surrealdb_live_message::logger;
use surrealdb_live_message::sdb_server;
use surrealdb_live_message::subsystems::{agents, sdb};

const AGENT_BOB: &str = "bob";
const AGENT_ALICE: &str = "alice";

#[tokio::main]
async fn main() -> Result<()> {
    logger::setup();

    // Setup and execute subsystem tree
    let names = vec![AGENT_ALICE.to_string(), AGENT_BOB.to_string()];
    Toplevel::new(move |s| async move {
        s.start(SubsystemBuilder::new(sdb::SUBSYS_NAME, sdb::sdb_subsystem));
        sdb_server::surrealdb_ready().await.unwrap();
        s.start(SubsystemBuilder::new(agents::SUBSYS_NAME, move |s| {
            agents::agents_subsystem(s, names)
        }));
    })
    .catch_signals()
    .handle_shutdown_requests(Duration::from_millis(1000))
    .await
    .map_err(Into::into)
}
