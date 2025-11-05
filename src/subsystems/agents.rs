use anyhow::Result;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use surrealdb::Notification;
use surrealdb_types::{Action, Datetime, RecordId, SurrealValue};
use tokio::time::{Duration, sleep};
use tokio_graceful_shutdown::{SubsystemBuilder, SubsystemHandle};

use crate::message::{MESSAGE_TABLE, Message, Payload};
use crate::subsystems::sdb;

pub const AGENT_TABLE: &str = "agent";

// ============================================================================
// Agent Data Structure
// ============================================================================

#[derive(Debug, Serialize, Deserialize, Clone, SurrealValue)]
pub struct Agent {
    pub id: RecordId,
    pub name: String,
    created: Datetime,
}

impl Agent {
    pub async fn new(name: &str) -> Result<Self> {
        let db = sdb::SurrealDBWrapper::connection().await;

        let agent: Agent = db
            .create((AGENT_TABLE, name))
            .content(Agent {
                id: RecordId::new(AGENT_TABLE, name),
                name: name.to_string(),
                created: Datetime::default(),
            })
            .await
            .map_err(|e| anyhow::anyhow!("Failed to create agent: {}", e))?
            .ok_or_else(|| anyhow::anyhow!("Failed to create agent: no data returned"))?;

        Ok(agent)
    }

    pub async fn send(&self, to: &str, payload: Payload) -> Result<()> {
        let db = sdb::SurrealDBWrapper::connection().await;

        // Use RecordId directly instead of string formatting
        let from_id = self.id.clone();
        let to_id = RecordId::new(AGENT_TABLE, to);

        // Create the message as a relationship using typed approach
        let query = "RELATE $from->message->$to CONTENT { created: time::now(), payload: $payload };";

        db.query(query)
            .bind(("from", from_id))
            .bind(("to", to_id))
            .bind(("payload", payload))
            .await
            .map_err(|e| anyhow::anyhow!("Failed to create message relationship: {}", e))?;

        Ok(())
    }

    /// Run the agent subsystem - manages the agent and its listener
    pub async fn run_subsystem(self, subsys: &mut SubsystemHandle) -> Result<()> {
        let agent_name = self.name.clone();
        tracing::info!("{} starting.", agent_name);

        // Start listen subsystem - it will be automatically managed by the parent
        let listen_name = format!("{}-listen", agent_name);
        let agent_for_listen = self.clone();
        subsys.start(
            SubsystemBuilder::new(listen_name, {
                async move |subsys: &mut SubsystemHandle| {
                    agent_listen_subsystem(agent_for_listen, subsys).await
                }
            }),
        );

        subsys.on_shutdown_requested().await;
        tracing::info!("{} shutting down ...", agent_name);
        sleep(Duration::from_millis(200)).await;
        tracing::info!("{} stopped.", agent_name);
        Ok(())
    }
}

/// Listen subsystem for an agent - handles incoming messages
async fn agent_listen_subsystem(agent: Agent, subsys: &mut SubsystemHandle) -> Result<()> {
    tracing::info!("{} listen subsystem starting.", agent.name);
    let db = sdb::SurrealDBWrapper::connection().await;

    // Create live query
    let _agent_id = RecordId::new(AGENT_TABLE, agent.name.clone());
    let query = format!("LIVE SELECT * FROM message WHERE out = agent:{}", agent.name);

    let mut response = db
        .query(&query)
        .await
        .map_err(|e| anyhow::anyhow!("LIVE SELECT failed: {}", e))?;

    let mut message_stream = response
        .stream::<Notification<Message>>(0)
        .map_err(|e| anyhow::anyhow!("Failed to create message stream: {}", e))?;

    loop {
        tokio::select! {
            Some(result) = message_stream.next() => {
                match result {
                    Ok(notification) => {
                        let action = notification.action;
                        let message: Message = notification.data;
                        if action == Action::Delete {
                            tracing::debug!("Message deleted: {:?}", message);
                            continue;
                        }

                        match &message.payload {
                            Payload::Text(text_payload) => {
                                tracing::info!("{} received text message: {}", agent.name, text_payload.content);
                            },
                            Payload::Image(image_payload) => {
                                tracing::info!("Image message: {}, caption: {:?}", image_payload.url, image_payload.caption);
                            },
                            Payload::Video(video_payload) => {
                                tracing::info!("Video message: {}, duration: {} seconds", video_payload.url, video_payload.duration);
                            },
                        }
                    }
                    Err(error) => tracing::error!("Stream error: {}", error),
                }
            }
            _ = subsys.on_shutdown_requested() => {
                tracing::info!("Shutdown signal received, terminating listen function for agent {}", agent.name);

                // Clean up: Drop the stream which will automatically cleanup the live query
                drop(message_stream);

                // Clean up: Delete agent messages
                let message_id = RecordId::new(MESSAGE_TABLE, agent.name.clone());
                if let Err(e) = db.delete::<Option<Message>>(message_id).await {
                    tracing::error!("Failed to delete agent messages: {}", e);
                }

                break;
            }
        }
    }
    Ok(())
}

/// Create an agents subsystem with the given agent names
pub fn agents_subsystem_with_names(
    names: Vec<String>,
) -> impl FnOnce(&mut SubsystemHandle) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + '_>> {
    move |subsys: &mut SubsystemHandle| {
        Box::pin(async move {
            tracing::info!("{} starting.", subsys.name());
            tracing::info!("Starting agent subsystems ...");

            for name in names {
                let agent = Agent::new(&name)
                    .await
                    .map_err(|e| anyhow::anyhow!("Failed to create agent '{}': {}", name, e))?;

                subsys.start(
                    SubsystemBuilder::new(name.clone(), {
                        async move |subsys: &mut SubsystemHandle| {
                            agent.run_subsystem(subsys).await
                        }
                    }),
                );
            }

            subsys.on_shutdown_requested().await;
            tracing::info!("Agents subsystem shutting down ...");
            sleep(Duration::from_millis(200)).await;
            tracing::info!("{} stopped.", subsys.name());

            Ok(())
        })
    }
}
