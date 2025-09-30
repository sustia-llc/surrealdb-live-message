use futures::StreamExt;
use miette::Result;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, LazyLock, Mutex};
use surrealdb::Notification;
use surrealdb::opt::Resource;
use surrealdb::sql::{Datetime, Thing};
use tokio::time::{Duration, sleep};
use tokio_graceful_shutdown::{IntoSubsystem, SubsystemBuilder, SubsystemHandle};

use crate::message::{MESSAGE_TABLE, Message, Payload};
use crate::subsystems::sdb;

pub const AGENT_TABLE: &str = "agent";

pub struct AgentSubsystem {
    pub name: Arc<String>,
}

impl IntoSubsystem<miette::Report, miette::Report> for AgentSubsystem {
    async fn run(self, subsys: &mut SubsystemHandle<miette::Report>) -> Result<()> {
        agent_subsystem(self.name, subsys).await
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Agent {
    pub id: Thing,
    created: Datetime,
}

pub static REGISTRY: LazyLock<Mutex<Vec<Agent>>> = LazyLock::new(|| Mutex::new(Vec::new()));

pub fn get_registry() -> &'static Mutex<Vec<Agent>> {
    &REGISTRY
}

impl Agent {
    pub async fn new(name: &str) -> Self {
        let db = sdb::SurrealDBWrapper::connection().await.to_owned();
        let agent: Agent = db
            .create((AGENT_TABLE, name))
            .content(Agent {
                id: Thing::from((AGENT_TABLE, name)),
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

    pub async fn listen(
        &self,
        shutdown_signal: &mut SubsystemHandle<miette::Report>,
    ) -> surrealdb::Result<()> {
        let db = sdb::SurrealDBWrapper::connection().await.to_owned();

        let name = &self.id.id.to_string();
        let query = format!("LIVE SELECT * FROM message where out = agent:{}", name);
        let mut response = db.query(&query).await.expect("LIVE SELECT failed.");
        let mut message_stream = response.stream::<Notification<Message>>(0)?;

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
                                    tracing::info!("{} received text message: {}", name, text_payload.content);
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
                _ = shutdown_signal.on_shutdown_requested() => {
                    tracing::info!("Shutdown signal received, terminating listen function for agent {}", name);
                    drop(message_stream);
                    drop(response);
                    db.delete(Resource::from((MESSAGE_TABLE, name))).await.expect("agent message delete failed");
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
    async fn run(self, subsys: &mut SubsystemHandle<miette::Report>) -> miette::Result<()> {
        self.agent
            .listen(subsys)
            .await
            .map_err(|e| miette::miette!(e.to_string()))?;
        Ok(())
    }
}

pub async fn agent_subsystem(
    name: Arc<String>,
    subsys: &mut SubsystemHandle<miette::Report>,
) -> Result<()> {
    tracing::info!("{} starting.", name);
    let agent = Agent::new(name.as_str()).await;

    let listen_subsys = subsys.start(
        SubsystemBuilder::new(
            format!("{}-listen", name),
            ListenSubsystem {
                agent: agent.clone(),
            }
            .into_subsystem(),
        )
        .detached(),
    );

    subsys.on_shutdown_requested().await;

    listen_subsys.initiate_shutdown();
    let _ = listen_subsys.join().await;

    tracing::info!("{name} shutting down ...");
    sleep(Duration::from_millis(200)).await;
    tracing::info!("{name} stopped.");
    Ok(())
}
