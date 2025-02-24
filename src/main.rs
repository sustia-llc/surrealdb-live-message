use miette::Result;
use tokio::time::Duration;
use tokio_graceful_shutdown::{SubsystemBuilder, Toplevel};

use surrealdb_live_message::logger;
use surrealdb_live_message::subsystems::{agents, sdb};
const AGENT_BOB: &str = "bob";
const AGENT_ALICE: &str = "alice";

#[tokio::main]
async fn main() -> Result<()> {
    logger::setup();

    // Setup and execute subsystem tree
    let names = vec![AGENT_ALICE.to_string(), AGENT_BOB.to_string()];
    Toplevel::new(move |s| async move {
        s.start(SubsystemBuilder::new("sdb", sdb::sdb_subsystem));
        
        // Wait for database to be ready
        let mut rx = sdb::SurrealDBWrapper::get_ready_receiver();
        let timeout = tokio::time::sleep(Duration::from_secs(30));
        tokio::select! {
            _ = timeout => panic!("Timeout waiting for database to be ready"),
            _ = async {
                while !*rx.borrow_and_update() {
                    let _ = rx.changed().await;
                }
            } => {},
        }
        
        s.start(SubsystemBuilder::new("agents", move |s| agents::agents_subsystem(s, names)));
    })
    .catch_signals()
    .handle_shutdown_requests(Duration::from_secs(4))
    .await
    .map_err(|e| miette::miette!(e.to_string()))?;
    Ok(())
}
