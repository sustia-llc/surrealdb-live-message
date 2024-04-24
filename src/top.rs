use crate::agent::agent_subsystem;
use miette::Result;
use std::error::Error;

use std::sync::Arc;
use tokio::time::{sleep, Duration};
use tokio_graceful_shutdown::{NestedSubsystem, SubsystemBuilder, SubsystemHandle};

pub async fn top_level(subsys: SubsystemHandle, agent_names: Vec<String>) -> Result<()> {
    tracing::info!("top_level started.");
    tracing::info!("Starting detached agent subsystems ...");
    let mut agent_subsystems: Vec<NestedSubsystem<Box<dyn Error + Sync + Send>>> = Vec::new();
    for name in agent_names {
        let agent = subsys.start(
            SubsystemBuilder::new(name.clone(), move |s| agent_subsystem(Arc::new(name), s))
                .detached(),
        );
        agent_subsystems.push(agent);
    }

    subsys.on_shutdown_requested().await;

    tracing::info!("Initiating agents shutdown ...");
    for agent in agent_subsystems.iter() {
        agent.initiate_shutdown();
        agent.join().await?;
    }

    tracing::info!("All agents finished, stopping top_level ...");
    sleep(Duration::from_millis(200)).await;
    tracing::info!("top_level stopped.");

    Ok(())
}
