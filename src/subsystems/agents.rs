use crate::subsystems::agent::AgentSubsystem;
use miette::Result;

use std::sync::Arc;
use tokio::time::{Duration, sleep};
use tokio_graceful_shutdown::{IntoSubsystem, NestedSubsystem, SubsystemBuilder, SubsystemHandle};

pub struct AgentsSubsystem {
    pub names: Vec<String>,
}

impl IntoSubsystem<miette::Report, miette::Report> for AgentsSubsystem {
    async fn run(self, subsys: &mut SubsystemHandle<miette::Report>) -> Result<()> {
        self::agents_subsystem(subsys, self.names).await
    }
}

pub async fn agents_subsystem(
    subsys: &mut SubsystemHandle<miette::Report>,
    agent_names: Vec<String>,
) -> Result<()> {
    tracing::info!("{} starting.", subsys.name());
    tracing::info!("Starting detached agent subsystems ...");
    let mut agent_subsystems: Vec<NestedSubsystem<miette::Report>> = Vec::new();

    for name in agent_names {
        let agent_subsystem = subsys.start(
            SubsystemBuilder::new(
                name.clone(),
                AgentSubsystem {
                    name: Arc::new(name),
                }
                .into_subsystem(),
            )
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
    tracing::info!("{} stopped.", subsys.name());

    Ok(())
}
