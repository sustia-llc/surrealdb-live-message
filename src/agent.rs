use futures::StreamExt;
use miette::Result;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use surrealdb::sql::{Datetime, Thing};
use surrealdb::Notification;
use tokio::sync::oneshot;
use tokio::time::{sleep, Duration};
use tokio_graceful_shutdown::SubsystemHandle;

use crate::connection;
use crate::message::{save_message_history, Message, Payload, TextPayload, MESSAGE_TABLE};
// use crate::get_registry;

pub const AGENT_TABLE: &str = "agent";
pub const AGENT_BOB: &str = "bob";
pub const AGENT_ALICE: &str = "alice";

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Agent {
    pub id: Thing,
    created: Datetime,
}

lazy_static::lazy_static! {
    static ref REGISTRY: Mutex<Vec<Agent>> = Mutex::new(Vec::new());
}

pub fn get_registry() -> &'static Mutex<Vec<Agent>> {
    &REGISTRY
}

impl Agent {
    pub async fn new(name: &str) -> Self {
        let db = connection().await.to_owned();
        let agent: Agent = db
            .update((AGENT_TABLE, name))
            .content(Agent {
                id: Thing::from((AGENT_TABLE, name)),
                created: Datetime::default(),
            })
            .await
            .unwrap()
            .unwrap();
        let mut registry = REGISTRY.lock().unwrap();
        registry.push(agent.clone());
 
        agent
    }

    pub async fn create_message_record(&self) -> surrealdb::Result<()> {
        let db = connection().await.to_owned();
        // create initial message queue for alice
        let text_payload = TextPayload {
            content: "Initializing message queue".to_owned(),
        };

        let name = &self.id.id.to_string();
        let user_id: Thing = Thing::from((MESSAGE_TABLE, "user"));
        let message_id: Thing = Thing::from((MESSAGE_TABLE, name.as_str()));
        let _: Message = db
            .update((MESSAGE_TABLE, name))
            .content(Message {
                id: message_id,
                payload: Payload::Text(text_payload),
                from: user_id,
                created: Some(Datetime::default()),
                updated: None,
            })
            .await
            .unwrap()
            .unwrap();
        Ok(())
    }

    pub async fn send(&self, to: &str, payload: Payload) -> surrealdb::Result<()> {
        let db = connection().await.to_owned();
        let _: Message = db
            .update((MESSAGE_TABLE, to))
            .content(Message {
                id: Thing::from((MESSAGE_TABLE, to)),
                payload,
                from: self.id.clone(),
                created: Some(Datetime::default()),
                updated: None,
            })
            .await
            .unwrap()
            .unwrap();
        Ok(())
    }

    pub async fn listen(
        &self,
        mut shutdown_signal: oneshot::Receiver<()>,
    ) -> surrealdb::Result<()> {
        let db = connection().await.to_owned();

        let name = &self.id.id.to_string();
        let query = format!("LIVE SELECT * FROM message where id = message:{}", name);
        let mut response = db.query(&query).await.unwrap();
        let mut message_stream = response.stream::<Notification<Message>>(0)?;

        loop {
            tokio::select! {
                Some(result) = message_stream.next() => {
                    match result {
                        Ok(notification) => {
                            let action = notification.action;
                            let message: Message = notification.data;

                            match &message.payload {
                                Payload::Text(text_payload) => {
                                    tracing::debug!("{:?}: Text message: {}", action, text_payload.content);
                                },
                                Payload::Image(image_payload) => {
                                    tracing::debug!("Image message: {}, caption: {:?}", image_payload.url, image_payload.caption);
                                },
                                Payload::Video(video_payload) => {
                                    tracing::debug!("Video message: {}, duration: {} seconds", video_payload.url, video_payload.duration);
                                },
                            }
                            save_message_history(message).await?;
                        }
                        Err(error) => tracing::error!("{}", error),
                    }
                }
                _ = &mut shutdown_signal => {
                    tracing::debug!("Shutdown signal received, terminating listen function.");
                    drop(message_stream);
                    drop(response);
                    // TODO: verify live query is killed
                    break;
                }
            }
        }
        Ok(())
    }
}

pub async fn agent_subsystem(
    name: Arc<String>,
    subsys: SubsystemHandle,
) -> Result<()> {
    tracing::info!("{} started.", name);
    let agent = Agent::new(name.as_str()).await;
    agent.create_message_record().await.unwrap();

// FIXME: add agent to registry
    // { 
    //     let agent_clone = agent.clone();
    //     let mut registry = get_registry().lock().unwrap();
    //     registry.add_agent(agent_clone);
    // }
    
    let (shutdown_sender, shutdown_receiver) = oneshot::channel::<()>();
    let listen_task = tokio::spawn({
        async move {
            agent
                .listen(shutdown_receiver)
                .await
                .expect("Failed to listen for new messages");
        }
    });

    subsys.on_shutdown_requested().await;
    let _ = shutdown_sender.send(()); // Ignore the error if the receiver is dropped
    let _ = listen_task.await; // Wait for the listen task to complete

    tracing::info!("{name} shutting down ...");
    sleep(Duration::from_millis(200)).await;
    tracing::info!("{name} stopped.");
    Ok(())
}
