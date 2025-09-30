use miette::Result;
use tokio::time::Duration;
use tokio_graceful_shutdown::{IntoSubsystem, SubsystemBuilder, SubsystemHandle, Toplevel};

use surrealdb_live_message::logger;
use surrealdb_live_message::subsystems::agents::AgentsSubsystem;
use surrealdb_live_message::subsystems::sdb::{self, SdbSubsystem};

const AGENT_BOB: &str = "bob";
const AGENT_ALICE: &str = "alice";

async fn main_subsystem(s: &mut SubsystemHandle<miette::Report>) {
    s.start(SubsystemBuilder::new("sdb", SdbSubsystem.into_subsystem()));

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

    let names = vec![AGENT_ALICE.to_string(), AGENT_BOB.to_string()];
    s.start(SubsystemBuilder::new(
        "agents",
        AgentsSubsystem { names }.into_subsystem(),
    ));
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
