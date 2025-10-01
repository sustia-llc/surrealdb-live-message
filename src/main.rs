use miette::Result;
use tokio::time::Duration;
use tokio_graceful_shutdown::{SubsystemBuilder, SubsystemHandle, Toplevel};

use surrealdb_live_message::logger;
use surrealdb_live_message::subsystems::{agents, sdb};

const AGENT_BOB: &str = "bob";
const AGENT_ALICE: &str = "alice";

async fn main_subsystem(s: &mut SubsystemHandle<miette::Report>) {
    // Start database subsystem
    s.start(SubsystemBuilder::new("sdb", sdb::sdb_subsystem));

    // Wait for database to be ready with timeout
    let db_ready_rx = sdb::SurrealDBWrapper::get_ready_receiver();
    match tokio::time::timeout(Duration::from_secs(30), db_ready_rx).await {
        Ok(Ok(())) => tracing::info!("Database is ready, starting agents..."),
        Ok(Err(_)) => panic!("Database ready signal channel closed"),
        Err(_) => panic!("Timeout waiting for database to be ready"),
    }

    // Initialize agent names and start agents subsystem
    let names = vec![AGENT_ALICE.to_string(), AGENT_BOB.to_string()];
    agents::AgentsWrapper::init(names);
    s.start(SubsystemBuilder::new("agents", agents::agents_subsystem));
}

#[tokio::main]
async fn main() -> Result<()> {
    logger::setup();

    Toplevel::<miette::Report>::new(main_subsystem)
        .catch_signals()
        .handle_shutdown_requests(Duration::from_secs(4))
        .await
        .map_err(|e| miette::miette!(e.to_string()))?;

    Ok(())
}
