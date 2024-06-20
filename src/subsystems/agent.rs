use crate::subsystems::sdb;
use futures::StreamExt;
use miette::Result;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use surrealdb::opt::Resource;
use surrealdb::sql::{Datetime, Thing};
use surrealdb::Notification;
use tokio::time::{sleep, Duration};
use tokio_graceful_shutdown::{SubsystemBuilder, SubsystemHandle};

use crate::message::{Message, Payload, MESSAGE_TABLE};

pub const AGENT_TABLE: &str = "agent";

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Agent {
    pub id: Thing,
    created: Datetime,
}

pub static REGISTRY: Lazy<Mutex<Vec<Agent>>> =
    Lazy::new(|| Mutex::new(Vec::new()));

pub fn get_registry() -> &'static Mutex<Vec<Agent>> {
    &REGISTRY
}

impl Agent {
    pub async fn new(name: &str) -> Self {
        let db = sdb::connection().await.to_owned();
        let agent: Agent = db
            .upsert((AGENT_TABLE, name))
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
        let db = sdb::connection().await.to_owned();

        let payload = serde_json::to_string(&payload).unwrap();
        let from = self.id.id.to_string();
        let query = format!("RELATE agent:{}->message->agent:{} CONTENT {{ created: time::now(), payload: {{{}}} }};", from, to, payload);

        let _ = db.query(&query).await.expect("RELATE failed.");
        Ok(())
    }

    pub async fn listen(&self, shutdown_signal: SubsystemHandle) -> surrealdb::Result<()> {
        let db = sdb::connection().await.to_owned();

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
                                    tracing::info!("{:?}: Text message: {}", action, text_payload.content);
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
                    tracing::info!("Shutdown signal received, terminating listen function.");
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

pub async fn agent_subsystem(name: Arc<String>, subsys: SubsystemHandle) -> Result<()> {
    tracing::info!("{} starting.", name);
    let agent = Agent::new(name.as_str()).await;

    let listen_subsys = subsys.start(
        SubsystemBuilder::new(format!("{}-listen", name), |subsys| async move {
            agent
                .listen(subsys)
                .await
                .expect("Failed to listen for new messages");
            Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
        })
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
