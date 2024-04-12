use miette::Result;
use std::sync::Arc;
use tokio::time::{sleep, Duration};
use tokio_graceful_shutdown::{SubsystemBuilder, SubsystemHandle};
use crate::agent::{AGENT_BOB, AGENT_ALICE, agent_subsystem};

pub async fn top_level(subsys: SubsystemHandle) -> Result<()> {
    tracing::info!("top_level started.");
    tracing::info!("Starting detached agent subsystems ...");

    let bob_subsys = subsys.start(
        SubsystemBuilder::new("AgentBob", move |s| {
            agent_subsystem(Arc::new(AGENT_BOB.to_string()), s)
        })
        .detached(),
    );

    let alice_subsys = subsys.start(
        SubsystemBuilder::new("AgentAlice", move |s| {
            agent_subsystem(Arc::new(AGENT_ALICE.to_string()), s)
        })
        .detached(),
    );

    subsys.on_shutdown_requested().await;

    tracing::info!("Initiating AgentBob shutdown ...");
    bob_subsys.initiate_shutdown();
    bob_subsys.join().await?;
    tracing::info!("Initiating AgentAlice shutdown ...");
    alice_subsys.initiate_shutdown();
    alice_subsys.join().await?;

    tracing::info!("All agents finished, stopping top_level ...");
    sleep(Duration::from_millis(200)).await;
    tracing::info!("top_level stopped.");

    Ok(())
}