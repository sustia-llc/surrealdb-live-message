use crate::agent::agent_subsystem;
use miette::Result;
use std::error::Error;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::time::{sleep, Duration};
use tokio_graceful_shutdown::{NestedSubsystem, SubsystemBuilder, SubsystemHandle};

lazy_static::lazy_static! {
    static ref READY_FLAG: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
}

pub fn is_ready() -> bool {
    READY_FLAG.load(Ordering::SeqCst)
}

fn send_ready_signal() {
    READY_FLAG.store(true, Ordering::SeqCst);
}

pub async fn top_level(subsys: SubsystemHandle, agent_names: Vec<String>) -> Result<()> {
    tracing::info!("top_level started.");
    tracing::info!("Starting detached agent subsystems ...");
    let mut agents: Vec<NestedSubsystem<Box<dyn Error + Sync + Send>>> = Vec::new();
    for name in agent_names {
        let agent = subsys.start(
            SubsystemBuilder::new(name.clone(), move |s| agent_subsystem(Arc::new(name), s))
                .detached(),
        );
        agents.push(agent);
    }
    send_ready_signal();
    subsys.on_shutdown_requested().await;

    tracing::info!("Initiating agents shutdown ...");
    for agent in agents.iter() {
        agent.initiate_shutdown();
        agent.join().await?;
    }

    tracing::info!("All agents finished, stopping top_level ...");
    sleep(Duration::from_millis(200)).await;
    tracing::info!("top_level stopped.");

    Ok(())
}
