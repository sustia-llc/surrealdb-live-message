use std::{collections::HashMap, marker::PhantomData, sync::Arc};

use anyhow::Result;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use surrealdb::Notification;
use surrealdb_types::{Action, Datetime, RecordId, SurrealValue};
use tokio::sync::RwLock;
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use tracing::Instrument;

use crate::message::{MESSAGE_TABLE, Message};
use crate::subsystems::sdb;

pub const AGENT_TABLE: &str = "agent";

// ============================================================================
// Agent
// ============================================================================

#[derive(Debug, Serialize, Deserialize, Clone, SurrealValue)]
pub struct Agent {
    pub id: RecordId,
    pub name: String,
    created: Datetime,
}

impl Agent {
    /// Create (or reuse) an agent record in the `agent` table.
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

    /// Send a typed payload to another agent by creating a RELATE edge in the
    /// `message` table.
    pub async fn send<T>(&self, to: &str, payload: T) -> Result<()>
    where
        T: SurrealValue + Send + Sync + Unpin + 'static,
    {
        let db = sdb::SurrealDBWrapper::connection().await;
        let from_id = self.id.clone();
        let to_id = RecordId::new(AGENT_TABLE, to);

        let query =
            "RELATE $from->message->$to CONTENT { created: time::now(), payload: $payload };";

        db.query(query)
            .bind(("from", from_id))
            .bind(("to", to_id))
            .bind(("payload", payload))
            .await
            .map_err(|e| anyhow::anyhow!("Failed to create message relationship: {}", e))?;

        Ok(())
    }

    /// Listen loop — subscribes to LIVE SELECT on messages addressed to this
    /// agent and drains the stream until `token` cancels.
    ///
    /// Library-side lifecycle primitive. No `SubsystemHandle`, no signal
    /// handling. See `rust-practical:async-lifecycle` skill in rust-v2
    /// v4.1.0.
    async fn listen_loop<T>(self, token: CancellationToken) -> Result<()>
    where
        T: SurrealValue + Send + Sync + Unpin + 'static,
    {
        tracing::info!("listen_loop starting for {}", self.name);
        let db = sdb::SurrealDBWrapper::connection().await;

        let query = format!(
            "LIVE SELECT * FROM message WHERE out = agent:{}",
            self.name
        );
        let mut response = db
            .query(&query)
            .await
            .map_err(|e| anyhow::anyhow!("LIVE SELECT failed: {}", e))?;
        let mut stream = response
            .stream::<Notification<Message<T>>>(0)
            .map_err(|e| anyhow::anyhow!("Failed to create message stream: {}", e))?;

        loop {
            tokio::select! {
                Some(result) = stream.next() => {
                    match result {
                        Ok(notification) => {
                            let action = notification.action;
                            if action == Action::Delete {
                                continue;
                            }
                            let message = notification.data;
                            tracing::info!(
                                target = %self.name,
                                from = ?message.r#in,
                                "message received"
                            );
                        }
                        Err(error) => tracing::error!("stream error: {}", error),
                    }
                }
                _ = token.cancelled() => {
                    tracing::info!("listen_loop for {} received shutdown", self.name);
                    drop(stream);
                    // Best-effort cleanup of this agent's message edges.
                    let message_id = RecordId::new(MESSAGE_TABLE, self.name.clone());
                    if let Err(e) = db.delete::<Option<Message<T>>>(message_id).await {
                        tracing::warn!("failed to delete agent messages on shutdown: {}", e);
                    }
                    break;
                }
            }
        }
        Ok(())
    }
}

// ============================================================================
// Coalition<T>
// ============================================================================

/// A scoped group of agents all exchanging payloads of type `T`.
///
/// Library-first lifecycle — exposes `cancellation_token()` so callers wire
/// their own top-level shutdown, and `shutdown().await` for the
/// cancel-close-drain three-step. See `rust-practical:async-lifecycle` skill.
pub struct Coalition<T: SurrealValue + Send + Sync + Unpin + 'static> {
    agents: Arc<RwLock<HashMap<String, Agent>>>,
    task_tracker: TaskTracker,
    cancellation_token: CancellationToken,
    _payload: PhantomData<T>,
}

impl<T: SurrealValue + Send + Sync + Unpin + 'static> Coalition<T> {
    /// Create `N` agents and spawn their listen loops under an internal
    /// `TaskTracker`, each with its own `child_token()`.
    pub async fn new(names: Vec<String>) -> Result<Self> {
        let agents = Arc::new(RwLock::new(HashMap::new()));
        let task_tracker = TaskTracker::new();
        let cancellation_token = CancellationToken::new();

        for name in names {
            let agent = Agent::new(&name).await?;
            agents.write().await.insert(name.clone(), agent.clone());

            let token = cancellation_token.child_token();
            let span = tracing::info_span!("agent", name = %name);
            task_tracker.spawn(agent.listen_loop::<T>(token).instrument(span));
        }

        Ok(Self {
            agents,
            task_tracker,
            cancellation_token,
            _payload: PhantomData,
        })
    }

    /// Look up an agent by name for send calls.
    pub async fn agent(&self, name: &str) -> Option<Agent> {
        self.agents.read().await.get(name).cloned()
    }

    /// Root cancellation token — downstream binaries wire top-level shutdown
    /// onto this.
    pub fn cancellation_token(&self) -> &CancellationToken {
        &self.cancellation_token
    }

    /// Three-step shutdown: cancel → close → drain.
    pub async fn shutdown(&self) {
        self.cancellation_token.cancel();
        self.task_tracker.close();
        self.task_tracker.wait().await;
    }
}
