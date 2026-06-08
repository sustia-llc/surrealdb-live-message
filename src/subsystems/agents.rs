use std::{collections::HashMap, marker::PhantomData, sync::Arc};

use futures::StreamExt;
use kanal::{AsyncReceiver, AsyncSender};
use serde::{Deserialize, Serialize};
use surrealdb::Notification;
use surrealdb_types::{Action, Datetime, RecordId, SurrealValue};
use tokio::sync::{RwLock, oneshot};
use tokio::time::{Duration, timeout};
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use tracing::Instrument;

use crate::error::{Error, Result};
use crate::message::{MESSAGE_TABLE, Message};
use crate::subsystems::sdb;

pub const AGENT_TABLE: &str = "agent";

/// Maximum time `Coalition::new` waits for each `listen_loop` to signal that
/// its LIVE query is registered before giving up. Guards against a DB that
/// stalls during LIVE registration (so the sender is neither sent nor dropped).
const READY_TIMEOUT: Duration = Duration::from_secs(10);

/// Capacity of the shared MPMC delivery bus connecting agent listen loops to
/// downstream consumers via [`Coalition::inbox`].
const INBOX_CAPACITY: usize = 256;

/// An inbound message delivered to an agent's listen loop, forwarded onto the
/// coalition's shared kanal bus. `recipient` is the agent that received it;
/// `message.r#in` is the sender.
#[derive(Debug)]
pub struct Delivery<T: SurrealValue> {
    pub recipient: String,
    pub message: Message<T>,
}

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
            .map_err(|source| Error::AgentCreate {
                agent: name.to_string(),
                source,
            })?
            .ok_or_else(|| Error::AgentCreateEmpty {
                agent: name.to_string(),
            })?;

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
            .map_err(|source| Error::Send {
                to: to.to_string(),
                source,
            })?;

        Ok(())
    }

    /// Listen loop — subscribes to LIVE SELECT on messages addressed to this
    /// agent and drains the stream until `token` cancels.
    ///
    /// `ready_tx` fires after the LIVE query is registered with the server
    /// but before the first notification is polled. Callers (e.g.
    /// `Coalition::new`) await this so `Agent::send` calls can't race the
    /// subscription.
    ///
    /// Library-side lifecycle primitive. No `SubsystemHandle`, no signal
    /// handling. See `rust-practical:async-lifecycle` skill in rust-v2
    /// v4.1.0.
    async fn listen_loop<T>(
        self,
        token: CancellationToken,
        ready_tx: oneshot::Sender<()>,
        inbox_tx: AsyncSender<Delivery<T>>,
    ) -> Result<()>
    where
        T: SurrealValue + Send + Sync + Unpin + 'static,
    {
        tracing::info!("listen_loop starting for {}", self.name);
        let db = sdb::SurrealDBWrapper::connection().await;

        // Explicit field projection — `LIVE SELECT *` on edges doesn't
        // always include `in`/`out` in the notification payload.
        let query = format!(
            "LIVE SELECT *, in, out FROM message WHERE out = agent:{}",
            self.name
        );
        let mut response = db.query(&query).await.map_err(|source| Error::LiveQuery {
            agent: self.name.clone(),
            source,
        })?;
        let mut stream = response
            .stream::<Notification<Message<T>>>(0)
            .map_err(|source| Error::Stream {
                agent: self.name.clone(),
                source,
            })?;

        // Signal the caller that this agent's subscription is live. Send()
        // calls issued after Coalition::new returns are now guaranteed to
        // be captured by this stream.
        let _ = ready_tx.send(());

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
                            if let Err(e) = inbox_tx
                                .send(Delivery { recipient: self.name.clone(), message })
                                .await
                            {
                                tracing::debug!("inbox bus closed for {}: {}", self.name, e);
                            }
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
    inbox: AsyncReceiver<Delivery<T>>,
    _payload: PhantomData<T>,
}

impl<T: SurrealValue + Send + Sync + Unpin + 'static> Coalition<T> {
    /// Create `N` agents and spawn their listen loops under an internal
    /// `TaskTracker`, each with its own `child_token()`.
    ///
    /// Waits for every `listen_loop` to confirm its LIVE query is registered
    /// before returning. Each wait is bounded by [`READY_TIMEOUT`]; a stalled
    /// or dropped listen loop yields an error instead of hanging.
    pub async fn new(names: Vec<String>) -> Result<Self> {
        let agents = Arc::new(RwLock::new(HashMap::new()));
        let task_tracker = TaskTracker::new();
        let cancellation_token = CancellationToken::new();

        // Shared MPMC delivery bus. Each agent's listen_loop holds a clone of
        // the sender; the receiver lives on the Coalition and is handed out via
        // inbox(). Dropping our own sender below leaves only agent-held senders,
        // so the bus closes once all agents shut down.
        let (inbox_tx, inbox_rx) = kanal::bounded_async::<Delivery<T>>(INBOX_CAPACITY);

        let mut ready_rxs = Vec::with_capacity(names.len());
        for name in names {
            let agent = Agent::new(&name).await?;
            agents.write().await.insert(name.clone(), agent.clone());

            let token = cancellation_token.child_token();
            let (ready_tx, ready_rx) = oneshot::channel();
            ready_rxs.push((name.clone(), ready_rx));

            let span = tracing::info_span!("agent", name = %name);
            task_tracker.spawn(
                agent
                    .listen_loop::<T>(token, ready_tx, inbox_tx.clone())
                    .instrument(span),
            );
        }

        // Drop our sender so only agent-held senders remain; the bus then closes
        // (recv returns Err) once every agent's listen_loop has shut down.
        drop(inbox_tx);

        // Wait until every listen_loop has confirmed its LIVE query is
        // registered with the server. Without this handshake, Coalition::new
        // could return before subscriptions exist and the first Agent::send
        // calls would be lost. Each await is bounded by READY_TIMEOUT so an
        // unresponsive DB stalling LIVE registration (sender neither sent nor
        // dropped) cannot hang Coalition::new forever.
        for (name, rx) in ready_rxs {
            let err = match timeout(READY_TIMEOUT, rx).await {
                Ok(Ok(())) => continue,
                Ok(Err(_)) => Error::ListenLoopDropped { agent: name },
                Err(_) => Error::ReadyTimeout {
                    agent: name,
                    timeout: READY_TIMEOUT,
                },
            };
            // Handshake failed: cancel the already-spawned listen_loops so they
            // shut down cleanly instead of running on detached (dropping the
            // token here would NOT cancel the child tokens they hold).
            cancellation_token.cancel();
            return Err(err);
        }

        Ok(Self {
            agents,
            task_tracker,
            cancellation_token,
            inbox: inbox_rx,
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

    /// A receiver on the shared MPMC delivery bus. Clone freely — multiple
    /// workers can pull concurrently (kanal MPMC). `recv()` returns `Err` once
    /// all agents have shut down and the bus closes.
    pub fn inbox(&self) -> AsyncReceiver<Delivery<T>> {
        self.inbox.clone()
    }

    /// Three-step shutdown: cancel → close → drain.
    pub async fn shutdown(&self) {
        self.cancellation_token.cancel();
        self.task_tracker.close();
        self.task_tracker.wait().await;
    }
}
