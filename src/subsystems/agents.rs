use futures::StreamExt;
use miette::Result;
use serde::{Deserialize, Serialize};
use std::sync::{LazyLock, Mutex, OnceLock};
use surrealdb::Notification;
use surrealdb::opt::Resource;
use surrealdb::sql::{Datetime, Thing};
use tokio::time::{Duration, sleep};
use tokio_graceful_shutdown::{IntoSubsystem, NestedSubsystem, SubsystemBuilder, SubsystemHandle};

use crate::message::{MESSAGE_TABLE, Message, Payload};
use crate::subsystems::sdb;

pub const AGENT_TABLE: &str = "agent";

// Static storage for agent names
static AGENT_NAMES: OnceLock<Vec<String>> = OnceLock::new();

// Agent registry
pub static REGISTRY: LazyLock<Mutex<Vec<Agent>>> = LazyLock::new(|| Mutex::new(Vec::new()));

pub fn get_registry() -> &'static Mutex<Vec<Agent>> {
    &REGISTRY
}

// ============================================================================
// Agent Data Structure
// ============================================================================

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Agent {
    pub id: Thing,
    pub name: String,
    created: Datetime,
}

impl Agent {
    pub async fn new(name: &str) -> Self {
        let db = sdb::SurrealDBWrapper::connection().await.to_owned();
        let agent: Agent = db
            .create((AGENT_TABLE, name))
            .content(Agent {
                id: Thing::from((AGENT_TABLE, name)),
                name: name.to_string(),
                created: Datetime::default(),
            })
            .await
            .unwrap()
            .expect("Failed to create agent.");
        let mut registry = REGISTRY.lock().unwrap();
        registry.push(agent.clone());

        agent
    }

    pub async fn send(&self, to: &str, payload: Payload) -> surrealdb::Result<()> {
        let db = sdb::SurrealDBWrapper::connection().await.to_owned();

        let payload = serde_json::to_string(&payload).unwrap();
        let from = self.id.id.to_string();
        let query = format!(
            "RELATE agent:{}->message->agent:{} CONTENT {{ created: time::now(), payload: {{{}}} }};",
            from, to, payload
        );

        let _ = db.query(&query).await.expect("RELATE failed.");
        Ok(())
    }

    /// Run the agent subsystem - manages the agent and its listener
    pub async fn run_subsystem(self, subsys: &mut SubsystemHandle<miette::Report>) -> Result<()> {
        tracing::info!("{} starting.", self.name);

        // Start listen subsystem
        let listen_subsys = subsys.start(
            SubsystemBuilder::new(
                format!("{}-listen", self.name),
                ListenSubsystem {
                    agent: self.clone(),
                }
                .into_subsystem(),
            )
            .detached(),
        );

        subsys.on_shutdown_requested().await;

        listen_subsys.initiate_shutdown();
        let _ = listen_subsys.join().await;

        tracing::info!("{} shutting down ...", self.name);
        sleep(Duration::from_millis(200)).await;
        tracing::info!("{} stopped.", self.name);
        Ok(())
    }

    /// Run the listen subsystem - handles incoming messages
    async fn run_listen_subsystem(
        self,
        subsys: &mut SubsystemHandle<miette::Report>,
    ) -> Result<()> {
        let db = sdb::SurrealDBWrapper::connection().await.to_owned();

        let query = format!("LIVE SELECT * FROM message where out = agent:{}", self.name);
        let mut response = db.query(&query).await.expect("LIVE SELECT failed.");
        let mut message_stream = response
            .stream::<Notification<Message>>(0)
            .map_err(|e| miette::miette!(e.to_string()))?;

        loop {
            tokio::select! {
                Some(result) = message_stream.next() => {
                    match result {
                        Ok(notification) => {
                            let action = notification.action;
                            let message: Message = notification.data;
                            if action == surrealdb::Action::Delete {
                                tracing::debug!("Message deleted: {:?}", message);
                                continue;
                            }

                            match &message.payload {
                                Payload::Text(text_payload) => {
                                    tracing::info!("{} received text message: {}", self.name, text_payload.content);
                                },
                                Payload::Image(image_payload) => {
                                    tracing::info!("Image message: {}, caption: {:?}", image_payload.url, image_payload.caption);
                                },
                                Payload::Video(video_payload) => {
                                    tracing::info!("Video message: {}, duration: {} seconds", video_payload.url, video_payload.duration);
                                },
                            }
                        }
                        Err(error) => tracing::error!("{}", error),
                    }
                }
                _ = subsys.on_shutdown_requested() => {
                    tracing::info!("Shutdown signal received, terminating listen function for agent {}", self.name);
                    drop(message_stream);
                    drop(response);
                    db.delete(Resource::from((MESSAGE_TABLE, self.name.as_str())))
                        .await
                        .expect("agent message delete failed");
                    // TODO: verify live query is killed
                    break;
                }
            }
        }
        Ok(())
    }
}

struct ListenSubsystem {
    agent: Agent,
}

impl IntoSubsystem<miette::Report, miette::Report> for ListenSubsystem {
    async fn run(self, subsys: &mut SubsystemHandle<miette::Report>) -> Result<()> {
        self.agent.run_listen_subsystem(subsys).await
    }
}

impl IntoSubsystem<miette::Report, miette::Report> for Agent {
    async fn run(self, subsys: &mut SubsystemHandle<miette::Report>) -> Result<()> {
        self.run_subsystem(subsys).await
    }
}

pub struct AgentsWrapper;

impl AgentsWrapper {
    /// Initialize agent names storage (call once at startup)
    pub fn init(names: Vec<String>) {
        AGENT_NAMES
            .set(names)
            .expect("Agent names already initialized");
    }

    /// Get stored agent names
    fn get_names() -> &'static Vec<String> {
        AGENT_NAMES.get().expect("Agent names not initialized")
    }
}
/// Main agents subsystem - manages all agents
pub async fn agents_subsystem(subsys: &mut SubsystemHandle<miette::Report>) -> Result<()> {
    tracing::info!("{} starting.", subsys.name());
    tracing::info!("Starting detached agent subsystems ...");

    let names = AgentsWrapper::get_names();
    let mut agent_subsystems: Vec<NestedSubsystem<miette::Report>> = Vec::new();

    for name in names {
        let agent = Agent::new(name).await;
        let agent_subsys =
            subsys.start(SubsystemBuilder::new(name.clone(), agent.into_subsystem()).detached());
        agent_subsystems.push(agent_subsys);
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
