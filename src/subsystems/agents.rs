use std::{collections::HashMap, marker::PhantomData, num::NonZeroUsize, sync::Arc};

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use surrealdb::engine::any;
use surrealdb::method::QueryStream;
use surrealdb::{Notification, Surreal};
use surrealdb_types::{Datetime, RecordId, SurrealValue, Uuid, Value};
use tokio::sync::{RwLock, oneshot};
use tokio::time::{Duration, sleep, timeout};
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use tracing::Instrument;

use crate::bus::{self, Inbox, Sender};
use crate::error::{Error, Result, chain};
use crate::message::{MESSAGE_TABLE, Message};
use crate::settings::SETTINGS;
use crate::subsystems::sdb;

pub const AGENT_TABLE: &str = "agent";

/// Maximum time `Coalition::new` waits for each `listen_loop` to signal that
/// its LIVE query is registered before giving up. Guards against a DB that
/// stalls during LIVE registration (so the sender is neither sent nor dropped).
const READY_TIMEOUT: Duration = Duration::from_secs(10);

/// Capacity of the shared MPMC delivery bus connecting agent listen loops to
/// downstream consumers via [`Coalition::inbox`].
const INBOX_CAPACITY: NonZeroUsize = NonZeroUsize::new(256).expect("invariant: 256 is non-zero");

/// Table holding each agent's durable-log high-water-mark cursor (`cursor:<agent>`).
pub const CURSOR_TABLE: &str = "cursor";

/// Max changesets pulled per `SHOW CHANGES` page during catch-up.
const CATCHUP_BATCH: usize = 1000;

/// Reconnect backoff bounds for the LIVE wake-up subscription.
const RECONNECT_BACKOFF_START: Duration = Duration::from_millis(200);
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Maximum time [`end_live`] waits for a `KILL` of a live query.
const LIVE_KILL_TIMEOUT: Duration = Duration::from_secs(2);

/// An inbound message delivered to an agent's listen loop, forwarded onto the
/// coalition's shared [`bus`]. `recipient` is the agent that received it;
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
    ///
    /// Validates `name` first: it becomes a SurrealDB record-id key *and* is
    /// bound into LIVE queries, so it is restricted to a safe character set
    /// (non-empty, ASCII alphanumeric or underscore). Rejecting at the boundary
    /// keeps malformed names out of the system rather than relying on downstream
    /// escaping.
    pub async fn new(name: &str) -> Result<Self> {
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(Error::InvalidAgentName {
                name: name.to_string(),
            });
        }

        let db = sdb::SurrealDBWrapper::connection().await;

        // Reuse an existing agent record if present. Agents are now durable —
        // they are NOT deleted on shutdown (the message log persists, so the
        // agent identity must too), and `create` errors on a duplicate id. A
        // select-then-create makes coalition *restart* idempotent, which is what
        // lets a restarted agent resume its durable-log cursor.
        if let Some(existing) =
            db.select((AGENT_TABLE, name))
                .await
                .map_err(|source| Error::AgentCreate {
                    agent: name.to_string(),
                    source: Box::new(source),
                })?
        {
            return Ok(existing);
        }

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
                source: Box::new(source),
            })?
            // Defensive: on a healthy DB `create().content()` returns `Some` on
            // success or `Err` on a duplicate id, never `Ok(None)`. This branch
            // is therefore not reachable without a fault-injected connection,
            // and is intentionally left without a dedicated trigger test.
            .ok_or_else(|| Error::AgentCreateEmpty {
                agent: name.to_string(),
            })?;

        Ok(agent)
    }

    /// Send a typed payload to another agent by creating a RELATE edge in the
    /// `message` table.
    ///
    /// Rejects unknown recipients: if no `agent` record exists for `to`, returns
    /// [`Error::UnknownRecipient`] instead of creating an edge with a dangling
    /// `out` pointer.
    pub async fn send<T>(&self, to: &str, payload: T) -> Result<()>
    where
        T: SurrealValue + Send + Sync + Unpin + 'static,
    {
        let db = sdb::SurrealDBWrapper::connection().await;
        let from_id = self.id.clone();
        let to_id = RecordId::new(AGENT_TABLE, to);

        // Reject sends to a recipient that has no `agent` record. Without this,
        // RELATE silently creates an edge whose `out` dangles at a non-existent
        // agent.
        let recipient: Option<Agent> =
            db.select(to_id.clone())
                .await
                .map_err(|source| Error::Send {
                    to: to.to_string(),
                    source: Box::new(source),
                })?;
        if recipient.is_none() {
            return Err(Error::UnknownRecipient { to: to.to_string() });
        }

        let query =
            "RELATE $from->message->$to CONTENT { created: time::now(), payload: $payload };";

        db.query(query)
            .bind(("from", from_id))
            .bind(("to", to_id))
            .bind(("payload", payload))
            .await
            .map_err(|source| Error::Send {
                to: to.to_string(),
                source: Box::new(source),
            })?;

        Ok(())
    }

    /// Listen loop — runs the two-tier durable bus for this agent until `token`
    /// cancels.
    ///
    /// **Tier 1** the `message` edge table is a durable, `CHANGEFEED`-backed
    /// log. **Tier 2** a `LIVE SELECT` acts purely as a low-latency *wake-up*;
    /// every wake (and every reconnect) triggers a `SHOW CHANGES … SINCE
    /// <cursor>` catch-up that is the *sole* delivery path. Catch-up advances a
    /// persisted per-agent versionstamp cursor (`cursor = max_seen + 1`), so a
    /// message is delivered once per cursor advance and never lost across a
    /// disconnect/restart — at-least-once with idempotent, monotonic resume.
    /// (LIVE alone is at-most-once with no replay; see the `live-queries`
    /// skill, "Delivery guarantees".)
    ///
    /// `ready_tx` fires after the first successful subscribe + backlog drain, so
    /// `Agent::send` calls issued after `Coalition::new` returns cannot race the
    /// subscription. A startup-phase failure (before `ready_tx` fires) returns
    /// `Err`, dropping the sender so `Coalition::new` surfaces it; failures after
    /// readiness reconnect with capped exponential backoff. Messages are NEVER
    /// deleted here — the log is aged out by the coalition's retention sweep.
    ///
    /// Library-side lifecycle primitive. No `SubsystemHandle`, no signal
    /// handling. See `rust-practical:async-lifecycle` skill.
    async fn listen_loop<T>(
        self,
        token: CancellationToken,
        ready_tx: oneshot::Sender<()>,
        inbox_tx: Sender<Delivery<T>>,
    ) -> Result<()>
    where
        T: SurrealValue + Send + Sync + Unpin + 'static,
    {
        tracing::info!("listen_loop starting for {}", self.name);
        let db = sdb::SurrealDBWrapper::connection().await;
        let owner = RecordId::new(AGENT_TABLE, self.name.clone());

        // Cursor: resume from the persisted high-water mark, or on first run
        // snapshot the current latest versionstamp so a brand-new agent starts
        // "from now" instead of replaying pre-existing backlog.
        let mut cursor = match load_cursor(db, &self.name).await? {
            Some(c) => c,
            None => {
                let start = latest_versionstamp(db, &self.name).await? + 1;
                save_cursor(db, &self.name, start).await?;
                start
            }
        };

        let mut ready_tx = Some(ready_tx);
        let mut backoff = RECONNECT_BACKOFF_START;

        loop {
            // (Re)subscribe the LIVE wake-up.
            let (mut stream, live_id) = match subscribe_wake(db, &owner, &self.name).await {
                Ok(s) => s,
                Err(e) if ready_tx.is_some() => return Err(e), // startup failure
                Err(e) => {
                    tracing::error!("resubscribe failed for {}: {}", self.name, chain(&e));
                    if cancel_or_sleep(&token, backoff).await {
                        return Ok(());
                    }
                    backoff = next_backoff(backoff);
                    continue;
                }
            };

            // Drain the durable log up to the live edge. Anything published
            // during the drain is buffered by the live stream and handled by the
            // next catch_up in the inner loop.
            if let Err(e) = catch_up::<T>(db, &owner, &self.name, &mut cursor, &inbox_tx).await {
                end_live(db, &self.name, live_id, stream).await;
                if ready_tx.is_some() {
                    return Err(e); // startup failure
                }
                tracing::error!("catch_up failed for {}: {}", self.name, chain(&e));
                if cancel_or_sleep(&token, backoff).await {
                    return Ok(());
                }
                backoff = next_backoff(backoff);
                continue;
            }

            // First successful subscribe + drain: signal readiness once.
            if let Some(tx) = ready_tx.take() {
                let _ = tx.send(());
            }
            backoff = RECONNECT_BACKOFF_START;

            // Inner loop: each wake triggers a catch_up; stream end/error
            // breaks out to reconnect.
            loop {
                tokio::select! {
                    // `biased`: cancellation takes strict priority so a hot
                    // stream can't starve shutdown.
                    biased;
                    _ = token.cancelled() => {
                        tracing::info!("listen_loop for {} received shutdown", self.name);
                        end_live(db, &self.name, live_id, stream).await;
                        return Ok(());
                    }
                    maybe = stream.next() => match maybe {
                        Some(Ok(_wake)) => {
                            if let Err(e) =
                                catch_up::<T>(db, &owner, &self.name, &mut cursor, &inbox_tx).await
                            {
                                tracing::error!("catch_up failed for {}: {}", self.name, chain(&e));
                                break;
                            }
                        }
                        Some(Err(error)) => {
                            tracing::error!(
                                "live wake stream error for {}: {}",
                                self.name,
                                chain(&error)
                            );
                            break;
                        }
                        None => {
                            tracing::info!("live wake stream ended for {}, reconnecting", self.name);
                            break;
                        }
                    }
                }
            }

            end_live(db, &self.name, live_id, stream).await;
            if cancel_or_sleep(&token, backoff).await {
                return Ok(());
            }
            backoff = next_backoff(backoff);
        }
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
    inbox: Inbox<Delivery<T>>,
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
        Self::new_with_ready_timeout(names, READY_TIMEOUT).await
    }

    /// Like [`Coalition::new`] but with a caller-chosen readiness-handshake
    /// timeout instead of the default [`READY_TIMEOUT`]. Production callers
    /// should prefer [`Coalition::new`]; the explicit timeout is mainly useful
    /// for tuning startup against a slow DB.
    ///
    /// The per-agent handshake await is factored into [`await_ready`], which is
    /// unit-tested deterministically (a never-sent receiver → [`Error::ReadyTimeout`],
    /// a dropped sender → [`Error::ListenLoopDropped`]) without racing a real
    /// LIVE-query registration.
    pub async fn new_with_ready_timeout(
        names: Vec<String>,
        ready_timeout: Duration,
    ) -> Result<Self> {
        let agents = Arc::new(RwLock::new(HashMap::new()));
        let task_tracker = TaskTracker::new();
        let cancellation_token = CancellationToken::new();

        // Shared MPMC delivery bus. Each agent's listen_loop holds a clone of
        // the sender; the receiver lives on the Coalition and is handed out via
        // inbox(). Dropping our own sender below leaves only agent-held senders,
        // so the bus closes once all agents shut down.
        let (inbox_tx, inbox_rx) = bus::bounded::<Delivery<T>>(INBOX_CAPACITY);

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

        // One table-wide retention sweep per coalition ages out the durable
        // message log (agents never delete messages). Runs under the same
        // TaskTracker + a child token, so cancel→close→wait drains it too.
        let sweep_token = cancellation_token.child_token();
        let retention = Duration::from_secs(SETTINGS.sdb.message_retention_secs);
        task_tracker.spawn(
            retention_sweep(sweep_token, retention)
                .instrument(tracing::info_span!("retention_sweep")),
        );

        // Wait until every listen_loop has confirmed its LIVE query is
        // registered with the server. Without this handshake, Coalition::new
        // could return before subscriptions exist and the first Agent::send
        // calls would be lost. Each await is bounded by READY_TIMEOUT so an
        // unresponsive DB stalling LIVE registration (sender neither sent nor
        // dropped) cannot hang Coalition::new forever.
        for (name, rx) in ready_rxs {
            let Err(err) = await_ready(name, rx, ready_timeout).await else {
                continue;
            };
            // Handshake failed: cancel the already-spawned listen_loops, then
            // drain them (close + wait) before returning — the same
            // cancel→close→wait as shutdown(). Cancel alone would leave the
            // tasks running detached (TaskTracker's Drop neither aborts nor
            // awaits), racing the caller; and dropping the token here would NOT
            // cancel the child tokens they hold.
            cancellation_token.cancel();
            task_tracker.close();
            task_tracker.wait().await;
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

    /// A receiver on the shared MPMC delivery bus. Each clone receives a
    /// disjoint share of the deliveries. `recv()` returns `Err` once all
    /// agents have shut down and the queued deliveries are drained;
    /// `blocking_recv()` is the same for a blocking thread.
    pub fn inbox(&self) -> Inbox<Delivery<T>> {
        self.inbox.clone()
    }

    /// Three-step shutdown: cancel → close → drain.
    pub async fn shutdown(&self) {
        self.cancellation_token.cancel();
        self.task_tracker.close();
        self.task_tracker.wait().await;
    }
}

/// Await one listen-loop's readiness signal, bounded by `ready_timeout`, mapping
/// the three outcomes onto the typed handshake errors:
///
/// - signal received → `Ok(())`
/// - sender dropped before signalling → [`Error::ListenLoopDropped`]
/// - timeout elapsed first → [`Error::ReadyTimeout`]
///
/// Pure async logic over a `oneshot` — no DB — so the two error paths are
/// unit-testable deterministically (see the tests below). The integration test
/// can't exercise `ReadyTimeout` reliably: [`tokio::time::timeout`] polls the
/// inner future before the timer, so a real LIVE-query registration that wins
/// the same park cycle resolves the receiver and yields `Ok` regardless of how
/// small the timeout is.
async fn await_ready(
    name: String,
    rx: oneshot::Receiver<()>,
    ready_timeout: Duration,
) -> Result<()> {
    match timeout(ready_timeout, rx).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) => Err(Error::ListenLoopDropped { agent: name }),
        Err(_) => Err(Error::ReadyTimeout {
            agent: name,
            timeout: ready_timeout,
        }),
    }
}

// ============================================================================
// Durable-log helpers (two-tier bus)
// ============================================================================

/// The single-field shape written to the `cursor` table.
#[derive(Debug, SurrealValue)]
struct CursorRow {
    versionstamp: i64,
}

/// Pull the `versionstamp` out of a `cursor` record (or a `SHOW CHANGES`
/// changeset) `Value::Object`.
fn versionstamp_of(v: &Value) -> Option<i64> {
    match v {
        Value::Object(o) => match o.get("versionstamp") {
            Some(Value::Number(n)) => n.to_int(),
            _ => None,
        },
        _ => None,
    }
}

/// Load the persisted high-water-mark cursor for `agent`, if any.
async fn load_cursor(db: &Surreal<any::Any>, agent: &str) -> Result<Option<i64>> {
    let row: Option<Value> =
        db.select((CURSOR_TABLE, agent))
            .await
            .map_err(|source| Error::CursorLoad {
                agent: agent.to_string(),
                source: Box::new(source),
            })?;
    Ok(row.as_ref().and_then(versionstamp_of))
}

/// Persist `agent`'s cursor (UPSERT so the first save creates the row). The
/// return is read back as `Value` to avoid coupling to the record's `id` field.
async fn save_cursor(db: &Surreal<any::Any>, agent: &str, versionstamp: i64) -> Result<()> {
    let _: Option<Value> = db
        .upsert((CURSOR_TABLE, agent))
        .content(CursorRow { versionstamp })
        .await
        .map_err(|source| Error::CursorSave {
            agent: agent.to_string(),
            source: Box::new(source),
        })?;
    Ok(())
}

/// Largest `versionstamp` in a `SHOW CHANGES` result array (0 if empty/none).
fn max_versionstamp(v: &Value) -> i64 {
    let Value::Array(arr) = v else { return 0 };
    arr.iter().filter_map(versionstamp_of).max().unwrap_or(0)
}

/// Latest versionstamp currently retained in the message changefeed, for the
/// first-run "start from now" snapshot. Reads (and discards) the retained window
/// once; returns 0 when the feed is empty. Only runs when an agent has no
/// persisted cursor yet.
async fn latest_versionstamp(db: &Surreal<any::Any>, agent: &str) -> Result<i64> {
    let q = format!("SHOW CHANGES FOR TABLE {MESSAGE_TABLE} SINCE 0 LIMIT 1000000");
    let v: Value = db
        .query(&q)
        .await
        .map_err(|source| Error::CatchUp {
            agent: agent.to_string(),
            source: Box::new(source),
        })?
        .take(0)
        .map_err(|source| Error::CatchUp {
            agent: agent.to_string(),
            source: Box::new(source),
        })?;
    Ok(max_versionstamp(&v))
}

/// Drain the durable log from `*cursor` and deliver every message addressed to
/// `owner`, advancing + persisting the cursor past everything seen. Bounded and
/// resumable; loops until a short page signals the live edge is reached. Because
/// `cursor = max_seen + 1`, a delivered message is never re-read (no dedup set
/// needed); `SHOW CHANGES` is table-wide, so non-owner changes are filtered out.
async fn catch_up<T>(
    db: &Surreal<any::Any>,
    owner: &RecordId,
    agent: &str,
    cursor: &mut i64,
    inbox_tx: &Sender<Delivery<T>>,
) -> Result<()>
where
    T: SurrealValue + Send + Sync + Unpin + 'static,
{
    loop {
        let q = format!(
            "SHOW CHANGES FOR TABLE {MESSAGE_TABLE} SINCE {} LIMIT {CATCHUP_BATCH}",
            *cursor
        );
        let v: Value = db
            .query(&q)
            .await
            .map_err(|source| Error::CatchUp {
                agent: agent.to_string(),
                source: Box::new(source),
            })?
            .take(0)
            .map_err(|source| Error::CatchUp {
                agent: agent.to_string(),
                source: Box::new(source),
            })?;
        let Value::Array(changesets) = v else { break };
        let page = changesets.len();
        if page == 0 {
            break;
        }

        let mut max_vs = *cursor - 1;
        for changeset in changesets.iter() {
            let Value::Object(obj) = changeset else {
                continue;
            };
            if let Some(vs) = versionstamp_of(changeset) {
                max_vs = max_vs.max(vs);
            }
            let Some(Value::Array(changes)) = obj.get("changes") else {
                continue;
            };
            for change in changes.iter() {
                let Value::Object(op) = change else { continue };
                // RELATE-created edges surface as a "create"/"update" op carrying
                // the full record; "delete" ops (retention sweep) are skipped.
                let Some(record) = op.get("update").or_else(|| op.get("create")) else {
                    continue;
                };
                let message = match Message::<T>::from_value(record.clone()) {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::warn!("skipping undecodable change for {agent}: {}", chain(&e));
                        continue;
                    }
                };
                if message.out.as_ref() != Some(owner) {
                    continue; // table-wide feed — keep only ours
                }
                tracing::info!(target = %agent, from = ?message.r#in, "message delivered");
                if let Err(e) = inbox_tx
                    .send(Delivery {
                        recipient: agent.to_string(),
                        message,
                    })
                    .await
                {
                    tracing::debug!("inbox bus closed for {agent}: {e}");
                }
            }
        }

        if max_vs >= *cursor {
            *cursor = max_vs + 1;
            save_cursor(db, agent, *cursor).await?;
        }
        if page < CATCHUP_BATCH {
            break;
        }
    }
    Ok(())
}

/// Periodic age-out sweep: delete message rows older than the retention window.
/// One task per coalition (table-wide); the durable log is otherwise never
/// pruned. Runs every `retention/4` (min 60s) until cancelled.
async fn retention_sweep(token: CancellationToken, retention: Duration) {
    let db = sdb::SurrealDBWrapper::connection().await;
    let interval = (retention / 4).max(Duration::from_secs(60));
    let secs = retention.as_secs();
    let q = format!("DELETE {MESSAGE_TABLE} WHERE created < time::now() - {secs}s");
    loop {
        tokio::select! {
            _ = token.cancelled() => break,
            _ = sleep(interval) => {
                if let Err(e) = db.query(&q).await.and_then(|r| r.check()) {
                    tracing::warn!("retention sweep failed: {}", chain(&e));
                }
            }
        }
    }
}

/// Sleep for `dur` unless cancelled first. Returns `true` if cancellation won
/// (the caller should stop).
async fn cancel_or_sleep(token: &CancellationToken, dur: Duration) -> bool {
    tokio::select! {
        _ = token.cancelled() => true,
        _ = sleep(dur) => false,
    }
}

/// Capped exponential backoff step.
fn next_backoff(current: Duration) -> Duration {
    (current * 2).min(RECONNECT_BACKOFF_MAX)
}

/// Register the LIVE wake-up subscription for `owner` and return its stream
/// with its live-query id, read from statement 0's result.
///
/// The payload is typed as `Value` and only signals that a message for `owner`
/// changed; delivery happens via catch_up. `owner` is bound as a parameter.
async fn subscribe_wake(
    db: &Surreal<any::Any>,
    owner: &RecordId,
    agent: &str,
) -> Result<(QueryStream<Notification<Value>>, Uuid)> {
    let mut response = db
        .query("LIVE SELECT id FROM message WHERE out = $owner")
        .bind(("owner", owner.clone()))
        .await
        .map_err(|source| Error::LiveQuery {
            agent: agent.to_string(),
            source: Box::new(source),
        })?;
    let stream = response
        .stream::<Notification<Value>>(0)
        .map_err(|source| Error::Stream {
            agent: agent.to_string(),
            source: Box::new(source),
        })?;
    match response.take::<Value>(0) {
        Ok(Value::Uuid(live_id)) => Ok((stream, live_id)),
        _ => Err(Error::LiveQueryIdMissing {
            agent: agent.to_string(),
        }),
    }
}

/// `KILL` live query `live_id` through `db` within [`LIVE_KILL_TIMEOUT`],
/// logging a failure or timeout at warn, then drop `stream`.
async fn end_live(
    db: &Surreal<any::Any>,
    agent: &str,
    live_id: Uuid,
    stream: QueryStream<Notification<Value>>,
) {
    let kill = db.query("KILL $id").bind(("id", Value::Uuid(live_id)));
    let outcome = match timeout(LIVE_KILL_TIMEOUT, kill).await {
        Ok(result) => result
            .and_then(|r| r.check())
            .map(drop)
            .map_err(|source| Error::LiveKill {
                agent: agent.to_string(),
                live_id,
                source: Box::new(source),
            }),
        Err(_elapsed) => Err(Error::LiveKillTimeout {
            agent: agent.to_string(),
            live_id,
            timeout: LIVE_KILL_TIMEOUT,
        }),
    };
    if let Err(e) = outcome {
        warn_end_live(&e);
    }
    drop(stream);
}

/// Log an [`end_live`] failure at warn with its full source chain.
fn warn_end_live(e: &Error) {
    tracing::warn!("{}", chain(e));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A never-sent receiver deterministically yields [`Error::ReadyTimeout`]:
    /// the receiver never resolves, so the (small, real) timeout always wins.
    #[tokio::test]
    async fn await_ready_times_out() {
        let (_tx, rx) = oneshot::channel();
        let err = await_ready("frank".to_string(), rx, Duration::from_millis(10))
            .await
            .expect_err("a never-sent ready signal must time out");
        assert!(
            matches!(err, Error::ReadyTimeout { ref agent, .. } if agent == "frank"),
            "expected ReadyTimeout for 'frank', got {err:?}"
        );
    }

    /// A dropped sender deterministically yields [`Error::ListenLoopDropped`]:
    /// the receiver resolves to `Err(RecvError)` before the timeout.
    #[tokio::test]
    async fn await_ready_detects_dropped_loop() {
        let (tx, rx) = oneshot::channel();
        drop(tx);
        let err = await_ready("frank".to_string(), rx, Duration::from_secs(30))
            .await
            .expect_err("a dropped sender must surface ListenLoopDropped");
        assert!(
            matches!(err, Error::ListenLoopDropped { ref agent } if agent == "frank"),
            "expected ListenLoopDropped for 'frank', got {err:?}"
        );
    }

    /// An in-memory datastore with the `message` table defined.
    async fn mem_db() -> Surreal<any::Any> {
        let db = any::connect("mem://").await.expect("connect mem://");
        db.use_ns("test").use_db("test").await.expect("use ns/db");
        db.query(format!("DEFINE TABLE {MESSAGE_TABLE}"))
            .await
            .and_then(|r| r.check())
            .expect("define message table");
        db
    }

    /// Ids of the live queries the datastore holds on `message`, read from
    /// `INFO FOR TABLE message` → `lives`.
    async fn message_lives(db: &Surreal<any::Any>) -> Vec<String> {
        let info: Value = db
            .query(format!("INFO FOR TABLE {MESSAGE_TABLE}"))
            .await
            .expect("info query")
            .take(0)
            .expect("info result");
        let Value::Object(info) = info else {
            panic!("INFO FOR TABLE returned a non-object: {info:?}");
        };
        match info.get("lives") {
            Some(Value::Object(lives)) => lives.keys().cloned().collect(),
            other => panic!("INFO FOR TABLE has no `lives` object: {other:?}"),
        }
    }

    /// Time for a `Drop`-spawned SDK KILL to run before the datastore is read.
    const DROP_KILL_SETTLE: Duration = Duration::from_millis(300);

    /// [`end_live`] removes the wake-up subscription from the datastore.
    #[tokio::test]
    async fn end_live_removes_the_live_query() {
        let db = mem_db().await;
        let owner = RecordId::new(AGENT_TABLE, "frank");
        let (stream, live_id) = subscribe_wake(&db, &owner, "frank")
            .await
            .expect("subscribe");
        let id = live_id.to_string();
        let before = message_lives(&db).await;
        assert_eq!(
            before,
            vec![id.clone()],
            "subscribe registers one live query"
        );

        end_live(&db, "frank", live_id, stream).await;
        sleep(DROP_KILL_SETTLE).await;

        let after = message_lives(&db).await;
        assert!(
            after.is_empty(),
            "live query {id} must be gone after end_live; datastore still lists {after:?}"
        );
    }

    /// Countermodel: dropping the SDK stream alone leaves the live query
    /// registered. Its `Drop` KILL runs on a fresh session with no namespace,
    /// database or sign-in, which the datastore refuses.
    #[tokio::test]
    async fn dropping_the_stream_leaves_the_live_query() {
        let db = mem_db().await;
        let owner = RecordId::new(AGENT_TABLE, "frank");
        let (stream, live_id) = subscribe_wake(&db, &owner, "frank")
            .await
            .expect("subscribe");

        drop(stream);
        sleep(DROP_KILL_SETTLE).await;

        let after = message_lives(&db).await;
        assert_eq!(
            after,
            vec![live_id.to_string()],
            "the SDK's Drop KILL is expected to fail and leave {live_id} registered"
        );
    }

    /// In-memory sink for a test tracing subscriber.
    #[derive(Clone, Default)]
    struct LogSink(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for LogSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("log sink lock").extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// [`end_live`]'s warn line for [`Error::LiveKill`] carries the inner
    /// surrealdb error text, not only the variant's own message.
    #[test]
    fn end_live_warning_includes_the_kill_error_source() {
        let inner = "kill refused by datastore";
        let err = Error::LiveKill {
            agent: "frank".to_string(),
            live_id: Uuid::new_v4(),
            source: Box::new(surrealdb::Error::internal(inner.to_string())),
        };

        let sink = LogSink::default();
        let writer = sink.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || warn_end_live(&err));

        let logs =
            String::from_utf8(sink.0.lock().expect("log sink lock").clone()).expect("utf-8 logs");
        let line = logs
            .lines()
            .find(|l| l.contains("failed to KILL live query"))
            .unwrap_or_else(|| panic!("no LiveKill warn line; captured logs {logs:?}"));
        assert!(
            line.contains(inner),
            "LiveKill warn line must contain the inner error {inner:?}; got {line:?}"
        );
    }

    /// A pre-sent signal resolves to `Ok(())` — the happy path of the seam.
    #[tokio::test]
    async fn await_ready_succeeds_when_signalled() {
        let (tx, rx) = oneshot::channel();
        tx.send(()).expect("send ready");
        await_ready("frank".to_string(), rx, Duration::from_secs(30))
            .await
            .expect("a signalled ready must succeed");
    }
}
