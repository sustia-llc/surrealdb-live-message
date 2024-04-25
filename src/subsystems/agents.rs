use crate::subsystems::agent::agent_subsystem;
use miette::Result;
use std::error::Error;

use std::sync::Arc;
use tokio::time::{sleep, Duration};
use tokio_graceful_shutdown::{NestedSubsystem, SubsystemBuilder, SubsystemHandle};

pub const SUBSYS_NAME: &str = "agents_subsystem";

pub async fn agents_subsystem(subsys: SubsystemHandle, agent_names: Vec<String>) -> Result<()> {
    tracing::info!("{} starting.", SUBSYS_NAME);
    tracing::info!("Starting detached agent subsystems ...");
    let mut agent_subsystems: Vec<NestedSubsystem<Box<dyn Error + Sync + Send>>> = Vec::new();
    for name in agent_names {
        let agent_subsystem = subsys.start(
            SubsystemBuilder::new(name.clone(), move |s| agent_subsystem(Arc::new(name), s))
                .detached(),
        );
        agent_subsystems.push(agent_subsystem);
    }

    subsys.on_shutdown_requested().await;

    tracing::info!("Initiating agents shutdown ...");
    for agent_subsystem in agent_subsystems.iter() {
        agent_subsystem.initiate_shutdown();
        agent_subsystem.join().await?;
    }
    tracing::info!("All agents finished ...");

    sleep(Duration::from_millis(200)).await;
    tracing::info!("{} stopped.", SUBSYS_NAME);

    Ok(())
}
