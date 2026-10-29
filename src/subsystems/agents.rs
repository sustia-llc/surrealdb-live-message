use std::{collections::HashMap, marker::PhantomData, ops::ControlFlow, sync::Arc};

use async_channel::{Receiver, Sender};
use futures::{FutureExt, StreamExt};
use serde::{Deserialize, Serialize};
use surrealdb::engine::any;
use surrealdb::method::QueryStream;
use surrealdb::{Notification, Surreal};
use surrealdb_types::{Datetime, RecordId, SurrealValue, Uuid, Value};
use tokio::sync::{RwLock, oneshot};
use tokio::time::{Duration, sleep, timeout};
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use tracing::Instrument;

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
const INBOX_CAPACITY: usize = 256;

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
/// coalition's shared bounded `async_channel`. `recipient` is the agent that received it;
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
    /// <cursor>` catch-up that is the *sole* delivery path. The persisted
    /// per-agent cursor is a (versionstamp, change index) pair that advances
    /// past a change only once its delivery is on the bus, and is persisted
    /// after each catch-up page and when a pass stops. A message not yet on the
    /// bus is put on it after a disconnect/restart. After a stop by
    /// cancellation or a bus with no receivers, no message is put on the bus
    /// twice; after a process exit mid-page, that page's already-sent messages
    /// are put on the bus again. Deliveries already on the bus are not
    /// replayed: they are lost if the process exits, or every receiver is
    /// dropped, before a consumer reads them.
    /// (LIVE alone is at-most-once with no replay; see the `live-queries`
    /// skill, "Delivery guarantees".)
    ///
    /// `ready_tx` fires once, after the first subscribe and the first catch-up
    /// page query succeed and before that pass puts anything on the bus. A
    /// failure before then (cursor load, first-run snapshot, first subscribe,
    /// first page query) is logged with its source chain and returned as
    /// `Err`, dropping the sender so `Coalition::new` fails; later failures
    /// reconnect with capped exponential backoff. While a delivery waits on a
    /// full bus the wake stream is still read. Cancellation of `token` ends the
    /// loop, including while a delivery waits on a full bus. Messages are
    /// NEVER deleted here — the log is aged out by the coalition's retention
    /// sweep.
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
        let name = self.name.as_str();

        // Cursor: resume from the persisted position, or on first run snapshot
        // the current latest versionstamp so a brand-new agent starts "from
        // now" instead of replaying pre-existing backlog.
        let loaded = load_cursor(db, name)
            .await
            .map_err(|e| startup_failure(name, e))?;
        let mut cursor = match loaded {
            Some(c) => c,
            None => {
                let latest = latest_versionstamp(db, name)
                    .await
                    .map_err(|e| startup_failure(name, e))?;
                let start = Cursor::at(latest + 1);
                save_cursor(db, name, start)
                    .await
                    .map_err(|e| startup_failure(name, e))?;
                start
            }
        };

        let mut ready_tx = Some(ready_tx);
        let mut backoff = RECONNECT_BACKOFF_START;

        loop {
            // (Re)subscribe the LIVE wake-up.
            let (stream, live_id) = match subscribe_wake(db, &owner, name).await {
                Ok(s) => s,
                Err(e) if ready_tx.is_some() => return Err(startup_failure(name, e)),
                Err(e) => {
                    tracing::error!("resubscribe failed for {name}: {}", chain(&e));
                    if cancel_or_sleep(&token, backoff).await {
                        return Ok(());
                    }
                    backoff = next_backoff(backoff);
                    continue;
                }
            };

            // Every (re)subscribe starts with one catch-up pass; each later
            // wake schedules another.
            let mut wakes = Wakes::new(stream);
            let exit = loop {
                if wakes.pending {
                    wakes.pending = false;
                    let pass = catch_up::<T>(
                        db,
                        &owner,
                        name,
                        &mut cursor,
                        &inbox_tx,
                        &token,
                        &mut wakes,
                        &mut ready_tx,
                    )
                    .await;
                    match pass {
                        Ok(ControlFlow::Continue(())) => backoff = RECONNECT_BACKOFF_START,
                        Ok(ControlFlow::Break(stop)) => break Exit::Stop(stop),
                        Err(e) => break Exit::CatchUp(e),
                    }
                }
                if let Some(end) = wakes.end.take() {
                    break Exit::Wake(end);
                }
                tokio::select! {
                    // `biased`: cancellation takes strict priority so a hot
                    // stream can't starve shutdown.
                    biased;
                    _ = token.cancelled() => break Exit::Stop(Stop::Cancelled),
                    item = wakes.stream.next() => wakes.record(item),
                }
            };

            let Wakes { stream, end, .. } = wakes;
            end_live(db, name, live_id, stream).await;
            match exit {
                Exit::Stop(stop) => {
                    stop.log(name);
                    return Ok(());
                }
                Exit::CatchUp(e) if ready_tx.is_some() => {
                    if let Some(end) = end {
                        end.log(name);
                    }
                    return Err(startup_failure(name, e));
                }
                Exit::CatchUp(e) => {
                    tracing::error!("catch_up failed for {name}: {}", chain(&e));
                    if let Some(end) = end {
                        end.log(name);
                    }
                }
                Exit::Wake(end) => end.log(name),
            }
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
    inbox: Receiver<Delivery<T>>,
    _payload: PhantomData<T>,
}

impl<T: SurrealValue + Send + Sync + Unpin + 'static> Coalition<T> {
    /// Create `N` agents and spawn their listen loops under an internal
    /// `TaskTracker`, each with its own `child_token()`.
    ///
    /// Waits for every `listen_loop` to signal that its LIVE query is
    /// registered and its first catch-up page query succeeded before
    /// returning. Each wait is bounded by [`READY_TIMEOUT`]; a stalled or
    /// dropped listen loop yields an error instead of hanging.
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
        let (inbox_tx, inbox_rx) = async_channel::bounded::<Delivery<T>>(INBOX_CAPACITY);

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

        // Wait until every listen_loop has signalled ready (LIVE query
        // registered, first catch-up page query succeeded). Each await is
        // bounded by READY_TIMEOUT.
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

    /// A receiver on the shared bounded `async_channel` delivery bus. Each
    /// delivery goes to one receiver clone. `recv()` returns the queued
    /// deliveries and then `Err` once all agents have shut down;
    /// `recv_blocking()` does the same while blocking the calling thread.
    pub fn inbox(&self) -> Receiver<Delivery<T>> {
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

/// An agent's position in the durable log: the changeset at `versionstamp`,
/// from its change at `change_index` on. Ordered by versionstamp, then index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, SurrealValue)]
struct Cursor {
    versionstamp: i64,
    change_index: i64,
}

impl Cursor {
    /// The start of the changeset at `versionstamp`.
    fn at(versionstamp: i64) -> Self {
        Self {
            versionstamp,
            change_index: 0,
        }
    }
}

impl std::fmt::Display for Cursor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}#{}", self.versionstamp, self.change_index)
    }
}

/// Log `e` with its source chain as a `listen_loop` failure before readiness
/// for `agent`, and return it.
fn startup_failure(agent: &str, e: Error) -> Error {
    tracing::error!("listen_loop for {agent} failed before ready: {}", chain(&e));
    e
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

/// Load the persisted cursor for `agent`, if any. A row without a
/// `change_index` resumes at index 0 of its versionstamp.
async fn load_cursor(db: &Surreal<any::Any>, agent: &str) -> Result<Option<Cursor>> {
    let row: Option<Value> =
        db.select((CURSOR_TABLE, agent))
            .await
            .map_err(|source| Error::CursorLoad {
                agent: agent.to_string(),
                source: Box::new(source),
            })?;
    let Some(row) = row else { return Ok(None) };
    let Some(versionstamp) = versionstamp_of(&row) else {
        return Ok(None);
    };
    let change_index = match &row {
        Value::Object(o) => match o.get("change_index") {
            Some(Value::Number(n)) => n.to_int().unwrap_or(0),
            _ => 0,
        },
        _ => 0,
    };
    Ok(Some(Cursor {
        versionstamp,
        change_index,
    }))
}

/// Persist `agent`'s cursor (UPSERT so the first save creates the row). The
/// return is read back as `Value` to avoid coupling to the record's `id` field.
async fn save_cursor(db: &Surreal<any::Any>, agent: &str, cursor: Cursor) -> Result<()> {
    let _: Option<Value> = db
        .upsert((CURSOR_TABLE, agent))
        .content(cursor)
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
    let v = show_changes(db, agent, 0, 1_000_000).await?;
    Ok(max_versionstamp(&v))
}

/// Run `SHOW CHANGES FOR TABLE message SINCE <since> LIMIT <limit>` and return
/// statement 0's result. Errors map to [`Error::CatchUp`] for `agent`.
async fn show_changes(
    db: &Surreal<any::Any>,
    agent: &str,
    since: i64,
    limit: usize,
) -> Result<Value> {
    let q = format!("SHOW CHANGES FOR TABLE {MESSAGE_TABLE} SINCE {since} LIMIT {limit}");
    let catch_up_err = |source| Error::CatchUp {
        agent: agent.to_string(),
        source: Box::new(source),
    };
    db.query(&q)
        .await
        .map_err(catch_up_err)?
        .take(0)
        .map_err(catch_up_err)
}

/// Why a [`catch_up`] pass stopped before reaching the live edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    /// `token` was cancelled.
    Cancelled,
    /// Every bus receiver is dropped, so a delivery cannot reach the bus.
    NoReceivers,
}

impl Stop {
    /// Log that `agent`'s listen loop ends for this reason.
    fn log(self, agent: &str) {
        match self {
            Stop::Cancelled => tracing::info!("listen_loop for {agent} received shutdown"),
            Stop::NoReceivers => tracing::info!("delivery bus has no receivers; {agent} stops"),
        }
    }
}

/// How a wake stream stopped yielding wakes.
enum WakeEnd {
    /// The stream yielded an error.
    Error(surrealdb::Error),
    /// The stream ended.
    Ended,
}

impl WakeEnd {
    /// Log this end of `agent`'s wake stream.
    fn log(&self, agent: &str) {
        match self {
            WakeEnd::Error(error) => {
                tracing::error!("live wake stream error for {agent}: {}", chain(error));
            }
            WakeEnd::Ended => tracing::info!("live wake stream ended for {agent}, reconnecting"),
        }
    }
}

/// An agent's LIVE wake-up stream and what has been read from it.
struct Wakes {
    stream: QueryStream<Notification<Value>>,
    /// A wake (or a fresh subscribe) has not yet been served by a catch-up pass.
    pending: bool,
    /// The stream's error or end, once read.
    end: Option<WakeEnd>,
}

impl Wakes {
    /// A freshly subscribed stream, with one catch-up pass pending.
    fn new(stream: QueryStream<Notification<Value>>) -> Self {
        Self {
            stream,
            pending: true,
            end: None,
        }
    }

    /// Record `item`, read from the stream, then every further item the
    /// stream has ready without waiting. A wake sets `pending`; an error or
    /// the end of the stream sets `end` and stops reading.
    fn record(&mut self, item: Option<std::result::Result<Notification<Value>, surrealdb::Error>>) {
        let mut item = item;
        loop {
            match item {
                Some(Ok(_wake)) => self.pending = true,
                Some(Err(error)) => {
                    self.end = Some(WakeEnd::Error(error));
                    return;
                }
                None => {
                    self.end = Some(WakeEnd::Ended);
                    return;
                }
            }
            match self.stream.next().now_or_never() {
                Some(next) => item = next,
                None => return,
            }
        }
    }
}

/// How a listen-loop subscription ended.
enum Exit {
    /// Cancellation, or a bus with no receivers.
    Stop(Stop),
    /// A catch-up pass failed.
    CatchUp(Error),
    /// The wake stream errored or ended.
    Wake(WakeEnd),
}

/// Put `delivery` on the bus, reading `wakes` while the bus is full. Returns
/// `Err` without putting it on the bus when `token` is cancelled first or
/// every receiver is dropped.
async fn hand_off<T>(
    inbox_tx: &Sender<Delivery<T>>,
    delivery: Delivery<T>,
    token: &CancellationToken,
    wakes: &mut Wakes,
) -> std::result::Result<(), Stop>
where
    T: SurrealValue + Send + Sync + Unpin + 'static,
{
    let send = inbox_tx.send(delivery);
    tokio::pin!(send);
    loop {
        tokio::select! {
            biased;
            _ = token.cancelled() => return Err(Stop::Cancelled),
            result = &mut send => return result.map_err(|_| Stop::NoReceivers),
            item = wakes.stream.next(), if wakes.end.is_none() => wakes.record(item),
        }
    }
}

/// Drain the durable log from `*cursor` and deliver every message addressed to
/// `owner` onto the bus, in log order. `SHOW CHANGES` is table-wide, so
/// non-owner changes are filtered out. After each page, the cursor is set to
/// the first change not yet handed to the bus, and persisted. `ready_tx`, if
/// still held, fires after the first page query succeeds and before any send.
/// Each send is raced against `token`, and `wakes` is read while a send waits
/// on a full bus. Cancellation, or a bus with no receivers, ends the pass with
/// `ControlFlow::Break` after persisting the cursor. Returns
/// `ControlFlow::Continue` once a short page signals the live edge is reached.
#[allow(clippy::too_many_arguments)]
async fn catch_up<T>(
    db: &Surreal<any::Any>,
    owner: &RecordId,
    agent: &str,
    cursor: &mut Cursor,
    inbox_tx: &Sender<Delivery<T>>,
    token: &CancellationToken,
    wakes: &mut Wakes,
    ready_tx: &mut Option<oneshot::Sender<()>>,
) -> Result<ControlFlow<Stop>>
where
    T: SurrealValue + Send + Sync + Unpin + 'static,
{
    loop {
        let v = show_changes(db, agent, cursor.versionstamp, CATCHUP_BATCH).await?;
        if let Some(tx) = ready_tx.take() {
            let _ = tx.send(());
        }
        let Value::Array(changesets) = v else { break };
        let page = changesets.len();
        if page == 0 {
            break;
        }

        let page_start = *cursor;
        let mut committed = *cursor;
        let mut stopped = None;
        'page: for changeset in changesets.iter() {
            let Value::Object(obj) = changeset else {
                continue;
            };
            let vs = versionstamp_of(changeset);
            let skip = match vs {
                Some(vs) if vs == page_start.versionstamp => {
                    usize::try_from(page_start.change_index).unwrap_or(0)
                }
                _ => 0,
            };
            let changes = match obj.get("changes") {
                Some(Value::Array(changes)) => changes.as_slice(),
                _ => &[],
            };
            for (index, change) in changes.iter().enumerate().skip(skip) {
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
                let span = tracing::info_span!("deliver", target = %agent, from = ?message.r#in);
                let delivery = Delivery {
                    recipient: agent.to_string(),
                    message,
                };
                match hand_off(inbox_tx, delivery, token, wakes).await {
                    Ok(()) => span.in_scope(|| tracing::info!("message delivered")),
                    Err(stop) => {
                        if let Some(vs) = vs {
                            let change_index = i64::try_from(index).unwrap_or(i64::MAX);
                            committed = committed.max(Cursor {
                                versionstamp: vs,
                                change_index,
                            });
                        }
                        stopped = Some(stop);
                        break 'page;
                    }
                }
            }
            if let Some(vs) = vs {
                committed = committed.max(Cursor::at(vs + 1));
            }
        }

        if committed > *cursor {
            *cursor = committed;
            save_cursor(db, agent, *cursor).await?;
        }
        if let Some(stop) = stopped {
            tracing::info!(
                "catch_up for {agent} stopped at cursor {}: {stop:?}",
                *cursor
            );
            return Ok(ControlFlow::Break(stop));
        }
        if page < CATCHUP_BATCH {
            break;
        }
    }
    Ok(ControlFlow::Continue(()))
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

    /// [`show_changes`] succeeds on a `message` table defined without a
    /// `CHANGEFEED`: the datastore answers `SHOW CHANGES` on it without error.
    #[tokio::test]
    async fn show_changes_succeeds_without_changefeed() {
        let db = mem_db().await;
        show_changes(&db, "frank", 1, 1)
            .await
            .expect("SHOW CHANGES on a table without a changefeed returns no error");
    }

    /// [`show_changes`] fails with [`Error::CatchUp`] for a negative `since`,
    /// which the parser rejects.
    #[tokio::test]
    async fn show_changes_fails_on_negative_since() {
        let db = mem_db().await;
        let err = show_changes(&db, "frank", -1, 1)
            .await
            .expect_err("SHOW CHANGES SINCE -1 must fail");
        assert!(
            matches!(err, Error::CatchUp { ref agent, .. } if agent == "frank"),
            "expected CatchUp for 'frank', got {err:?}"
        );
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
