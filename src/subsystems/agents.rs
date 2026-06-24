use std::{collections::HashMap, marker::PhantomData, sync::Arc};

use futures::StreamExt;
use kanal::{AsyncReceiver, AsyncSender};
use serde::{Deserialize, Serialize};
use surrealdb::engine::any;
use surrealdb::{Notification, Surreal};
use surrealdb_types::{Datetime, RecordId, SurrealValue, Value};
use tokio::sync::{RwLock, oneshot};
use tokio::time::{Duration, sleep, timeout};
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use tracing::Instrument;

use crate::error::{Error, Result};
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
const INBOX_CAPACITY: usize = 256;

/// Table holding each agent's durable-log high-water-mark cursor (`cursor:<agent>`).
pub const CURSOR_TABLE: &str = "cursor";

/// Max changesets pulled per `SHOW CHANGES` page during catch-up.
const CATCHUP_BATCH: usize = 1000;

/// Reconnect backoff bounds for the LIVE wake-up subscription.
const RECONNECT_BACKOFF_START: Duration = Duration::from_millis(200);
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(30);

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
        if let Some(existing) = db
            .select((AGENT_TABLE, name))
            .await
            .map_err(|source| Error::AgentCreate {
                agent: name.to_string(),
                source,
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
                source,
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
                    source,
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
                source,
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
        inbox_tx: AsyncSender<Delivery<T>>,
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
            // (Re)subscribe the LIVE wake-up. The payload is ignored (typed as
            // `Value` so it can never fail to deserialize) — it only signals
            // "something changed"; delivery happens via catch_up. The owner is
            // bound as a param so the agent name can't alter the query.
            let query = "LIVE SELECT id FROM message WHERE out = $owner";
            let subscribe = async {
                let mut response =
                    db.query(query)
                        .bind(("owner", owner.clone()))
                        .await
                        .map_err(|source| Error::LiveQuery {
                            agent: self.name.clone(),
                            source,
                        })?;
                response
                    .stream::<Notification<Value>>(0)
                    .map_err(|source| Error::Stream {
                        agent: self.name.clone(),
                        source,
                    })
            };
            let mut stream = match subscribe.await {
                Ok(s) => s,
                Err(e) if ready_tx.is_some() => return Err(e), // startup failure
                Err(e) => {
                    tracing::error!("resubscribe failed for {}: {e}", self.name);
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
            if let Err(e) =
                catch_up::<T>(db, &owner, &self.name, &mut cursor, &inbox_tx).await
            {
                if ready_tx.is_some() {
                    return Err(e); // startup failure
                }
                tracing::error!("catch_up failed for {}: {e}", self.name);
                drop(stream);
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
                        return Ok(());
                    }
                    maybe = stream.next() => match maybe {
                        Some(Ok(_wake)) => {
                            if let Err(e) =
                                catch_up::<T>(db, &owner, &self.name, &mut cursor, &inbox_tx).await
                            {
                                tracing::error!("catch_up failed for {}: {e}", self.name);
                                break;
                            }
                        }
                        Some(Err(error)) => {
                            tracing::error!("live wake stream error for {}: {error}", self.name);
                            break;
                        }
                        None => {
                            tracing::info!("live wake stream ended for {}, reconnecting", self.name);
                            break;
                        }
                    }
                }
            }

            drop(stream);
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
                source,
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
            source,
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
            source,
        })?
        .take(0)
        .map_err(|source| Error::CatchUp {
            agent: agent.to_string(),
            source,
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
    inbox_tx: &AsyncSender<Delivery<T>>,
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
                source,
            })?
            .take(0)
            .map_err(|source| Error::CatchUp {
                agent: agent.to_string(),
                source,
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
                        tracing::warn!("skipping undecodable change for {agent}: {e}");
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
                    tracing::warn!("retention sweep failed: {e}");
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
