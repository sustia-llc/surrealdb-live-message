use std::{collections::HashMap, marker::PhantomData, ops::ControlFlow, sync::Arc};

use async_channel::{Receiver, Sender};
use futures::{FutureExt, StreamExt};
use serde::{Deserialize, Serialize};
use surrealdb::engine::any;
use surrealdb::method::QueryStream;
use surrealdb::{Notification, Surreal};
use surrealdb_types::{Datetime, Object, RecordId, SurrealValue, Uuid, Value};
use tokio::sync::{RwLock, oneshot};
use tokio::time::{Duration, sleep, timeout};
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use tracing::Instrument;

use crate::error::{Error, Result, chain};
use crate::message::{MESSAGE_TABLE, Message};
use crate::settings::SETTINGS;
use crate::subsystems::sdb;

pub const AGENT_TABLE: &str = "agent";

/// Maximum time `Coalition::new` waits for each `listen_loop` to send its
/// ready signal.
const READY_TIMEOUT: Duration = Duration::from_secs(10);

/// Capacity of the shared MPMC delivery bus connecting agent listen loops to
/// downstream consumers via [`Coalition::inbox`].
const INBOX_CAPACITY: usize = 256;

/// Table holding each agent's durable-log position (`cursor:<agent>`): the
/// `versionstamp` of a changeset and the `change_index` within it of the first
/// change not yet on the bus.
pub const CURSOR_TABLE: &str = "cursor";

/// Max changesets pulled per `SHOW CHANGES` page during catch-up.
const CATCHUP_BATCH: usize = 1000;

/// Reconnect backoff bounds for the LIVE wake-up subscription.
const RECONNECT_BACKOFF_START: Duration = Duration::from_millis(200);
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Maximum number of already-ready wakes [`Wakes::record`] reads after the
/// one it is given.
const WAKE_DRAIN_MAX: usize = 64;

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
    /// per-agent cursor is a (versionstamp, change index) pair; see
    /// [`deliver_page`] for how it moves, including the malformed-data rule
    /// (log at error, then move on). The cursor is persisted after each
    /// catch-up page and when a pass stops; if that save fails, the persisted
    /// cursor stays at its previous saved position. A message not yet on the
    /// bus is put on it after a disconnect/restart. Each `update` op of a
    /// message record addressed to this agent is one delivery, so a message
    /// record that is updated after it is created is put on the bus again.
    /// For message records that are only created: after a stop whose cursor
    /// save succeeds, no message is put on the bus twice; after a process exit
    /// mid-page, or a failed save, the messages sent since the last saved
    /// position are put on the bus again. Deliveries already on the bus are
    /// not replayed: they are lost if the process exits, or every receiver is
    /// dropped, before a consumer reads them.
    ///
    /// `ready_tx` receives `Ok(())` once, after the first subscribe and the
    /// first catch-up page query succeed. The loop then waits for `start`
    /// before that pass puts anything on the bus. A failure before the ready
    /// signal (cursor load, first-run snapshot, first cursor save, first
    /// subscribe, first page query) is logged with its source chain and sent
    /// as `Err` on `ready_tx`, and the loop ends. Each of those awaits is raced
    /// against `token`; a cancel before the ready signal sends
    /// [`Error::Cancelled`] on `ready_tx`, and the loop ends. A subscribe in
    /// flight at a cancel is given up to [`LIVE_KILL_TIMEOUT`] to finish, and
    /// its live query is then ended. The loop ends when its ready send fails,
    /// or when `token` is cancelled while it waits for `start`. Later failures
    /// reconnect with capped exponential backoff. While a delivery waits on a
    /// full bus the wake stream is still read. The loop ends when `token` is
    /// cancelled (seen at the next bus send, page query, subscribe, or wait
    /// for a wake), including by `Coalition::shutdown` or the coalition's
    /// `Drop`, or when a send finds the bus closed: every receiver dropped, or
    /// `close()` called on any receiver. The loop never deletes messages; the
    /// coalition's retention sweep ages them out.
    async fn listen_loop<T>(
        self,
        token: CancellationToken,
        ready_tx: oneshot::Sender<Result<()>>,
        start: StartGate,
        inbox_tx: Sender<Delivery<T>>,
    ) where
        T: SurrealValue + Send + Sync + Unpin + 'static,
    {
        tracing::info!("listen_loop starting for {}", self.name);
        let name = self.name.as_str();
        let Some(db) = token
            .run_until_cancelled(sdb::SurrealDBWrapper::connection())
            .await
        else {
            return cancel_startup(name, ready_tx);
        };
        let ctx = LoopCtx {
            db,
            owner: RecordId::new(AGENT_TABLE, self.name.clone()),
            agent: name,
            inbox_tx,
            token,
        };

        // Cursor: resume from the persisted position, or on first run snapshot
        // the current latest versionstamp so a brand-new agent starts "from
        // now" instead of replaying pre-existing backlog.
        let mut cursor = match ctx
            .token
            .run_until_cancelled(initial_cursor(db, name))
            .await
        {
            Some(Ok(c)) => c,
            Some(Err(e)) => return fail_startup(name, ready_tx, e),
            None => return cancel_startup(name, ready_tx),
        };

        let mut ready_tx = Some(ready_tx);
        let mut backoff = RECONNECT_BACKOFF_START;

        loop {
            // (Re)subscribe the LIVE wake-up.
            let Some(subscribed) = subscribe_unless_cancelled(&ctx).await else {
                match ready_tx.take() {
                    Some(tx) => cancel_startup(name, tx),
                    None => Stop::Cancelled.log(name),
                }
                return;
            };
            let (stream, live_id) = match subscribed {
                Ok(s) => s,
                Err(e) => {
                    if let Some(tx) = ready_tx.take() {
                        return fail_startup(name, tx, e);
                    }
                    tracing::error!("resubscribe failed for {name}: {}", chain(&e));
                    if cancel_or_sleep(&ctx.token, backoff).await {
                        return;
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
                    let pass = async {
                        loop {
                            let page = match fetch_page(&ctx, cursor).await? {
                                ControlFlow::Continue(page) => page,
                                ControlFlow::Break(stop) => return Ok(ControlFlow::Break(stop)),
                            };
                            if let Some(tx) = ready_tx.take() {
                                if tx.send(Ok(())).is_err() {
                                    return Ok(ControlFlow::Break(Stop::NotStarted));
                                }
                                tokio::select! {
                                    biased;
                                    () = ctx.token.cancelled() => {
                                        return Ok(ControlFlow::Break(Stop::Cancelled));
                                    }
                                    () = start.opened() => {}
                                }
                            }
                            if page.is_empty() {
                                return Ok(ControlFlow::Continue(()));
                            }
                            let full = page.len() >= CATCHUP_BATCH;
                            let start = cursor;
                            if let ControlFlow::Break(stop) =
                                deliver_page(&ctx, &mut cursor, &mut wakes, page).await?
                            {
                                return Ok(ControlFlow::Break(stop));
                            }
                            if !full {
                                return Ok(ControlFlow::Continue(()));
                            }
                            if cursor == start {
                                tracing::error!(
                                    "catch_up for {name}: a full page left the cursor at {cursor}; \
                                     ending the pass"
                                );
                                return Ok(ControlFlow::Continue(()));
                            }
                        }
                    }
                    .await;
                    match pass {
                        Ok(ControlFlow::Continue(())) => {
                            backoff = RECONNECT_BACKOFF_START;
                            // A wake read during the pass set `pending` again.
                            continue;
                        }
                        Ok(ControlFlow::Break(stop)) => break Exit::Stop(stop),
                        Err(e) => break Exit::CatchUp(e),
                    }
                }
                if wakes.end.is_some() {
                    break Exit::Wake;
                }
                tokio::select! {
                    // `biased`: cancellation takes strict priority so a hot
                    // stream can't starve shutdown.
                    biased;
                    _ = ctx.token.cancelled() => break Exit::Stop(Stop::Cancelled),
                    item = wakes.stream.next() => wakes.record(item),
                }
            };

            let Wakes { stream, end, .. } = wakes;
            end_live(ctx.db, name, live_id, stream).await;
            if let Some(end) = end {
                end.log(name);
            }
            match exit {
                Exit::Stop(stop) => {
                    // Before the ready signal, only a cancel stops a pass.
                    match ready_tx.take() {
                        Some(tx) => cancel_startup(name, tx),
                        None => stop.log(name),
                    }
                    return;
                }
                Exit::CatchUp(e) => {
                    if let Some(tx) = ready_tx.take() {
                        return fail_startup(name, tx, e);
                    }
                    tracing::error!("catch_up failed for {name}: {}", chain(&e));
                }
                Exit::Wake => {}
            }
            if cancel_or_sleep(&ctx.token, backoff).await {
                return;
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
/// Exposes `cancellation_token()` so callers wire their own top-level
/// shutdown, and `shutdown().await`, which cancels the root token and waits
/// for every listen loop and the retention sweep to end. Dropping a
/// `Coalition` cancels its root token without waiting. The library installs
/// no signal handling.
pub struct Coalition<T: SurrealValue + Send + Sync + Unpin + 'static> {
    agents: Arc<RwLock<HashMap<String, Agent>>>,
    task_tracker: TaskTracker,
    cancellation_token: CancellationToken,
    inbox: Receiver<Delivery<T>>,
    _payload: PhantomData<T>,
}

impl<T: SurrealValue + Send + Sync + Unpin + 'static> Coalition<T> {
    /// Create `N` agents and spawn their listen loops under an internal
    /// `TaskTracker`, each with its own `child_token()`. A name that appears
    /// more than once returns [`Error::DuplicateAgentName`] before anything is
    /// created or spawned.
    ///
    /// Waits for every `listen_loop` to signal that its LIVE query is
    /// registered and its first catch-up page query succeeded. Each wait is
    /// bounded by [`READY_TIMEOUT`]. Each loop holds at its ready point, and
    /// puts nothing on the bus, until every loop has signalled ready; this
    /// then sends the start signal and returns `Ok`. On any failure it cancels
    /// every spawned loop and waits for each to end before it returns the
    /// error. When loops fail, an error a loop sent (including
    /// [`Error::Cancelled`]) is returned in preference to
    /// [`Error::ReadyTimeout`] and [`Error::ListenLoopDropped`].
    ///
    /// If the coalition's token is cancelled before every loop is ready, this
    /// returns [`Error::Cancelled`]; the token is checked before any database
    /// call, and each agent-record call is raced against it. Dropping the
    /// returned future before it completes cancels the token, which ends
    /// every spawned loop and the retention sweep; the drop does not wait for
    /// them.
    pub async fn new(names: Vec<String>) -> Result<Self> {
        Self::new_with_ready_timeout(names, READY_TIMEOUT).await
    }

    /// Like [`Coalition::new`], with `ready_timeout` in place of
    /// [`READY_TIMEOUT`] for each readiness wait.
    pub async fn new_with_ready_timeout(
        names: Vec<String>,
        ready_timeout: Duration,
    ) -> Result<Self> {
        Self::build(names, ready_timeout, CancellationToken::new()).await
    }

    /// Like [`Coalition::new`], with a root token that is a child of
    /// `parent`: cancelling `parent` cancels the coalition. A `parent` that is
    /// already cancelled returns [`Error::Cancelled`] for the first name in
    /// `names` (an empty name when `names` is empty) before any database call.
    pub async fn new_with_parent(names: Vec<String>, parent: &CancellationToken) -> Result<Self> {
        Self::build(names, READY_TIMEOUT, parent.child_token()).await
    }

    async fn build(
        names: Vec<String>,
        ready_timeout: Duration,
        cancellation_token: CancellationToken,
    ) -> Result<Self> {
        reject_duplicate_names(&names)?;
        if cancellation_token.is_cancelled() {
            return Err(Error::Cancelled {
                agent: names.first().cloned().unwrap_or_default(),
            });
        }
        // A dropped `build` future cancels every task it spawned.
        let guard = cancellation_token.clone().drop_guard();
        let agents = Arc::new(RwLock::new(HashMap::new()));
        let task_tracker = TaskTracker::new();
        let start = StartGate::default();

        // Shared MPMC delivery bus. Each agent's listen_loop holds a clone of
        // the sender; the receiver lives on the Coalition and is handed out via
        // inbox(). Dropping our own sender below leaves only agent-held senders,
        // so the bus closes once all agents shut down.
        let (inbox_tx, inbox_rx) = async_channel::bounded::<Delivery<T>>(INBOX_CAPACITY);

        let mut ready_rxs = Vec::with_capacity(names.len());
        for name in names {
            let agent = match cancellation_token
                .run_until_cancelled(Agent::new(&name))
                .await
            {
                Some(Ok(agent)) => agent,
                Some(Err(err)) => {
                    abort_build(&cancellation_token, &task_tracker).await;
                    return Err(err);
                }
                None => {
                    abort_build(&cancellation_token, &task_tracker).await;
                    return Err(Error::Cancelled { agent: name });
                }
            };
            agents.write().await.insert(name.clone(), agent.clone());

            let token = cancellation_token.child_token();
            let (ready_tx, ready_rx) = oneshot::channel();
            ready_rxs.push((name.clone(), ready_rx));

            let span = tracing::info_span!("agent", name = %name);
            task_tracker.spawn(
                agent
                    .listen_loop::<T>(token, ready_tx, start.clone(), inbox_tx.clone())
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

        // Wait, concurrently, until every listen_loop has signalled ready
        // (LIVE query registered, first catch-up page query succeeded). Each
        // wait is bounded by ready_timeout.
        let outcomes = futures::future::join_all(
            ready_rxs
                .into_iter()
                .map(|(name, rx)| await_ready(name, rx, ready_timeout)),
        )
        .await;
        if let Err(err) = first_ready_error(outcomes) {
            abort_build(&cancellation_token, &task_tracker).await;
            return Err(err);
        }

        start.open();
        guard.disarm();
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

    /// Cancel the root token, then wait until every listen loop and the
    /// retention sweep have ended.
    pub async fn shutdown(&self) {
        self.cancellation_token.cancel();
        self.task_tracker.close();
        self.task_tracker.wait().await;
    }
}

impl<T: SurrealValue + Send + Sync + Unpin + 'static> Drop for Coalition<T> {
    /// Cancel the root token without waiting for the tasks to end.
    fn drop(&mut self) {
        self.cancellation_token.cancel();
    }
}

/// Return [`Error::DuplicateAgentName`] for the first name in `names` that
/// appears earlier in `names`, else `Ok(())`.
fn reject_duplicate_names(names: &[String]) -> Result<()> {
    let mut unique = std::collections::HashSet::with_capacity(names.len());
    match names.iter().find(|n| !unique.insert(n.as_str())) {
        Some(name) => Err(Error::DuplicateAgentName { name: name.clone() }),
        None => Ok(()),
    }
}

/// Pick the error to return from the readiness outcomes of every listen loop:
/// the first error a listen loop sent (any variant other than
/// [`Error::ReadyTimeout`] or [`Error::ListenLoopDropped`], so including
/// [`Error::Cancelled`]); else the first `ReadyTimeout` or
/// `ListenLoopDropped`; else `Ok(())`.
fn first_ready_error(outcomes: Vec<Result<()>>) -> Result<()> {
    let mut fallback = None;
    for outcome in outcomes {
        match outcome {
            Ok(()) => {}
            Err(e @ (Error::ReadyTimeout { .. } | Error::ListenLoopDropped { .. })) => {
                fallback.get_or_insert(e);
            }
            Err(e) => return Err(e),
        }
    }
    fallback.map_or(Ok(()), Err)
}

/// Tear down a coalition whose construction failed: cancel `token`, close
/// `tracker`, and wait for every task it spawned to end.
async fn abort_build(token: &CancellationToken, tracker: &TaskTracker) {
    token.cancel();
    tracker.close();
    tracker.wait().await;
}

/// Await one listen-loop's readiness signal, bounded by `ready_timeout`, mapping
/// the four outcomes:
///
/// - `Ok(())` received → `Ok(())`
/// - `Err(e)` received → `Err(e)`, the listen loop's startup error
/// - sender dropped without sending → [`Error::ListenLoopDropped`]
/// - timeout elapsed first → [`Error::ReadyTimeout`]
///
/// Pure async logic over a `oneshot` — no DB — so every outcome is
/// unit-testable deterministically (see the tests below). The integration test
/// can't exercise `ReadyTimeout` reliably: [`tokio::time::timeout`] polls the
/// inner future before the timer, so a real LIVE-query registration that wins
/// the same park cycle resolves the receiver and yields `Ok` regardless of how
/// small the timeout is.
async fn await_ready(
    name: String,
    rx: oneshot::Receiver<Result<()>>,
    ready_timeout: Duration,
) -> Result<()> {
    match timeout(ready_timeout, rx).await {
        Ok(Ok(signal)) => signal,
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
/// for `agent`, and send it on `ready_tx`.
fn fail_startup(agent: &str, ready_tx: oneshot::Sender<Result<()>>, e: Error) {
    tracing::error!("listen_loop for {agent} failed before ready: {}", chain(&e));
    let _ = ready_tx.send(Err(e));
}

/// Log that `agent`'s listen loop was cancelled before readiness, and send
/// [`Error::Cancelled`] on `ready_tx`.
fn cancel_startup(agent: &str, ready_tx: oneshot::Sender<Result<()>>) {
    tracing::info!("listen_loop for {agent} cancelled before ready");
    let _ = ready_tx.send(Err(Error::Cancelled {
        agent: agent.to_string(),
    }));
}

/// A one-time signal from `Coalition::build` to its listen loops: open once
/// every loop has signalled ready. Clones share one state.
#[derive(Clone, Default)]
struct StartGate(CancellationToken);

impl StartGate {
    /// Open the gate; every current and later [`StartGate::opened`] returns.
    fn open(&self) {
        self.0.cancel();
    }

    /// Wait until the gate is open.
    async fn opened(&self) {
        self.0.cancelled().await;
    }
}

/// The persisted cursor for `agent`, or on first run one at the changeset
/// after the latest one retained in the feed, persisted.
async fn initial_cursor(db: &Surreal<any::Any>, agent: &str) -> Result<Cursor> {
    if let Some(c) = load_cursor(db, agent).await? {
        return Ok(c);
    }
    let start = Cursor::at(latest_versionstamp(db, agent).await? + 1);
    save_cursor(db, agent, start).await?;
    Ok(start)
}

/// A `surrealdb::Error` carrying `message`, for errors this crate detects in
/// a datastore reply.
fn reply_error(message: String) -> Box<surrealdb::Error> {
    Box::new(surrealdb::Error::internal(message))
}

/// Pull the `versionstamp` out of a `cursor` record (or a `SHOW CHANGES`
/// changeset) `Value::Object`.
fn versionstamp_of(v: &Value) -> Option<i64> {
    match v {
        Value::Object(o) => object_versionstamp(o),
        _ => None,
    }
}

/// The integer `versionstamp` field of `o`, if present.
fn object_versionstamp(o: &Object) -> Option<i64> {
    match o.get("versionstamp") {
        Some(Value::Number(n)) => n.to_int(),
        _ => None,
    }
}

/// Load the persisted cursor for `agent`, if any. Returns
/// [`Error::CursorLoad`] for a row that does not parse (see [`parse_cursor`]).
async fn load_cursor(db: &Surreal<any::Any>, agent: &str) -> Result<Option<Cursor>> {
    let row: Option<Value> =
        db.select((CURSOR_TABLE, agent))
            .await
            .map_err(|source| Error::CursorLoad {
                agent: agent.to_string(),
                source: Box::new(source),
            })?;
    row.map(|row| parse_cursor(agent, &row)).transpose()
}

/// `index` when it is non-negative and at most `max`, else `None`.
fn position_within(index: i64, max: u64) -> Option<i64> {
    u64::try_from(index)
        .ok()
        .filter(|i| *i <= max)
        .map(|_| index)
}

/// Parse a `cursor` row: an object with a non-negative integer `versionstamp`
/// and an optional non-negative integer `change_index` (absent → 0) that fits
/// `usize`. Any other shape returns [`Error::CursorLoad`] for `agent`.
fn parse_cursor(agent: &str, row: &Value) -> Result<Cursor> {
    let malformed = || Error::CursorLoad {
        agent: agent.to_string(),
        source: reply_error(format!("malformed cursor row: {row:?}")),
    };
    let Value::Object(o) = row else {
        return Err(malformed());
    };
    let non_negative = |v: Option<i64>| v.filter(|n| *n >= 0).ok_or_else(malformed);
    let versionstamp = match o.get("versionstamp") {
        Some(Value::Number(n)) => non_negative(n.to_int())?,
        _ => return Err(malformed()),
    };
    let change_index = match o.get("change_index") {
        None | Some(Value::None) | Some(Value::Null) => 0,
        Some(Value::Number(n)) => n
            .to_int()
            .and_then(|i| position_within(i, u64::try_from(usize::MAX).unwrap_or(u64::MAX)))
            .ok_or_else(malformed)?,
        Some(_) => return Err(malformed()),
    };
    Ok(Cursor {
        versionstamp,
        change_index,
    })
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

/// Why a catch-up pass stopped before reaching the live edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    /// `token` was cancelled.
    Cancelled,
    /// The bus is closed (every receiver dropped, or `close()` called on any
    /// receiver), so a delivery cannot reach the bus.
    NoReceivers,
    /// The ready signal found no receiver: `Coalition::new` has ended.
    NotStarted,
}

impl Stop {
    /// Log that `agent`'s listen loop ends for this reason.
    fn log(self, agent: &str) {
        match self {
            Stop::Cancelled => tracing::info!("listen_loop for {agent} received shutdown"),
            Stop::NoReceivers => tracing::info!("delivery bus has no receivers; {agent} stops"),
            Stop::NotStarted => {
                tracing::info!("Coalition::new ended before {agent}'s ready signal; {agent} stops");
            }
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

    /// Record `item`, read from the stream, then up to
    /// [`WAKE_DRAIN_MAX`] further items the stream has ready without waiting.
    /// A wake sets `pending`; an error or the end of the stream sets `end` and
    /// stops reading.
    fn record(&mut self, item: Option<std::result::Result<Notification<Value>, surrealdb::Error>>) {
        let mut next = Some(item);
        let mut drained = 0;
        while let Some(item) = next.take() {
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
            if drained == WAKE_DRAIN_MAX {
                return;
            }
            drained += 1;
            next = self.stream.next().now_or_never();
        }
    }
}

/// How a listen-loop subscription ended.
enum Exit {
    /// Cancellation, or a bus with no receivers.
    Stop(Stop),
    /// A catch-up pass failed.
    CatchUp(Error),
    /// The wake stream errored or ended; the end is in [`Wakes::end`].
    Wake,
}

/// What one listen loop works with for its whole run.
struct LoopCtx<'a, T: SurrealValue> {
    db: &'static Surreal<any::Any>,
    owner: RecordId,
    agent: &'a str,
    inbox_tx: Sender<Delivery<T>>,
    token: CancellationToken,
}

/// Put `delivery` on the bus, reading `wakes` while the bus is full. Returns
/// `Err` without putting it on the bus when `ctx.token` is cancelled first or
/// the bus is closed.
async fn hand_off<T>(
    ctx: &LoopCtx<'_, T>,
    delivery: Delivery<T>,
    wakes: &mut Wakes,
) -> std::result::Result<(), Stop>
where
    T: SurrealValue + Send + Sync + Unpin + 'static,
{
    let send = ctx.inbox_tx.send(delivery);
    tokio::pin!(send);
    loop {
        tokio::select! {
            biased;
            _ = ctx.token.cancelled() => return Err(Stop::Cancelled),
            result = &mut send => return result.map_err(|_| Stop::NoReceivers),
            item = wakes.stream.next(), if wakes.end.is_none() => wakes.record(item),
        }
    }
}

/// Fetch the catch-up page that starts at `cursor`: `SHOW CHANGES … SINCE
/// <cursor.versionstamp> LIMIT CATCHUP_BATCH`, raced against `ctx.token`.
/// Returns the page's changesets, `ControlFlow::Break(Stop::Cancelled)` when
/// the token is cancelled first, or [`Error::CatchUp`] when the query fails or
/// its reply is not an array.
async fn fetch_page<T: SurrealValue>(
    ctx: &LoopCtx<'_, T>,
    cursor: Cursor,
) -> Result<ControlFlow<Stop, Vec<Value>>> {
    let reply = tokio::select! {
        biased;
        _ = ctx.token.cancelled() => return Ok(ControlFlow::Break(Stop::Cancelled)),
        v = show_changes(ctx.db, ctx.agent, cursor.versionstamp, CATCHUP_BATCH) => v?,
    };
    match reply {
        Value::Array(changesets) => Ok(ControlFlow::Continue(changesets.into_iter().collect())),
        other => Err(Error::CatchUp {
            agent: ctx.agent.to_string(),
            source: reply_error(format!("SHOW CHANGES reply is not an array: {other:?}")),
        }),
    }
}

/// Change-op keys that `SHOW CHANGES` emits for a change that is not a
/// message record.
const NON_MESSAGE_OPS: [&str; 2] = ["delete", "define_table"];

/// Deliver one catch-up page (`page`, fetched from `*cursor`) onto the bus,
/// in log order, and persist the cursor.
///
/// Cursor rule: the cursor moves past a changeset once every change in it
/// has been handled, to the start of the next versionstamp; inside the
/// changeset at `cursor.versionstamp`, changes before `cursor.change_index`
/// are not read again. A change is handled when it is a `delete` or
/// `define_table` op, when its `update` record's `out` is another agent (`out`
/// is compared before any decode), or when its delivery is on the bus. A
/// changeset with no integer versionstamp has no position of its own: it is
/// skipped, and the cursor passes it only by moving to a later changeset's
/// position.
///
/// Malformed-data rule: a changeset that is not an object or has no integer
/// versionstamp, a `changes` field that is missing or not an array, a change
/// that is not an object or has no `update`, `delete` or `define_table` key, a
/// record that is not an object, a record whose `out` is missing or not a
/// record id, and a message for `ctx.owner` that does not decode as
/// `Message<T>` are each logged at error level (agent, versionstamp, what was
/// malformed) and skipped.
///
/// Each send is raced against `ctx.token`, and `wakes` is read while a send
/// waits on a full bus. Cancellation, or a closed bus, returns
/// `ControlFlow::Break` with the cursor saved at the change not put on the
/// bus, so the next pass starts at that change; if that save fails, the error
/// is logged and `ControlFlow::Break` is still returned. `*cursor` changes
/// only once a save succeeds; a save failure on a complete page returns that
/// error.
async fn deliver_page<T>(
    ctx: &LoopCtx<'_, T>,
    cursor: &mut Cursor,
    wakes: &mut Wakes,
    page: Vec<Value>,
) -> Result<ControlFlow<Stop>>
where
    T: SurrealValue + Send + Sync + Unpin + 'static,
{
    let agent = ctx.agent;
    let page_start = *cursor;
    let mut committed = *cursor;
    let mut stopped = None;
    'page: for mut changeset in page {
        let obj = match &mut changeset {
            Value::Object(obj) => obj,
            other => {
                tracing::error!(
                    "catch_up for {agent}: changeset is not an object with an integer \
                     versionstamp, skipped: {other:?}"
                );
                continue;
            }
        };
        let Some(vs) = object_versionstamp(obj) else {
            tracing::error!(
                "catch_up for {agent}: changeset is not an object with an integer \
                 versionstamp, skipped: {obj:?}"
            );
            continue;
        };
        let skip = if vs == page_start.versionstamp {
            usize::try_from(page_start.change_index).expect(
                "invariant: parse_cursor accepts only a change_index that fits usize, and \
                 catch-up stores usize positions",
            )
        } else {
            0
        };
        let changes: &mut [Value] = match obj.get_mut("changes") {
            Some(Value::Array(changes)) => changes.as_mut_slice(),
            other => {
                tracing::error!(
                    "catch_up for {agent}: changeset {vs} has no `changes` array, skipped: {other:?}"
                );
                &mut []
            }
        };
        for (index, change) in changes.iter_mut().enumerate().skip(skip) {
            let Value::Object(op) = change else {
                tracing::error!(
                    "catch_up for {agent}: change {index} of changeset {vs} is not an object, \
                     skipped: {change:?}"
                );
                continue;
            };
            // A RELATE-created edge surfaces as an "update" op carrying the
            // full record.
            let Some(record) = op.get_mut("update") else {
                if !NON_MESSAGE_OPS.iter().any(|k| op.get(*k).is_some()) {
                    let keys: Vec<&String> = op.keys().collect();
                    tracing::error!(
                        "catch_up for {agent}: change {index} of changeset {vs} has an unknown \
                         op, skipped: keys {keys:?}"
                    );
                }
                continue;
            };
            let Value::Object(fields) = &*record else {
                tracing::error!(
                    "catch_up for {agent}: record of change {index} in changeset {vs} is not \
                     an object, skipped: {record:?}"
                );
                continue;
            };
            match fields.get("out") {
                Some(Value::RecordId(out)) if *out == ctx.owner => {}
                Some(Value::RecordId(_)) => continue, // table-wide feed — keep only ours
                other => {
                    tracing::error!(
                        "catch_up for {agent}: record of change {index} in changeset {vs} has no \
                         record-id `out`, skipped: {other:?}"
                    );
                    continue;
                }
            }
            let message = match Message::<T>::from_value(std::mem::take(record)) {
                Ok(m) => m,
                Err(e) => {
                    tracing::error!(
                        "catch_up for {agent}: message in change {index} of changeset {vs} does \
                         not decode, skipped: {}",
                        chain(&e)
                    );
                    continue;
                }
            };
            let span = tracing::info_span!("deliver", target = %agent, from = ?message.r#in);
            let delivery = Delivery {
                recipient: agent.to_string(),
                message,
            };
            match hand_off(ctx, delivery, wakes).await {
                Ok(()) => span.in_scope(|| tracing::info!("message delivered")),
                Err(stop) => {
                    committed = committed.max(Cursor {
                        versionstamp: vs,
                        change_index: i64::try_from(index).expect(
                            "invariant: index is below a Vec's length, which is at most \
                             isize::MAX and so fits i64",
                        ),
                    });
                    stopped = Some(stop);
                    break 'page;
                }
            }
        }
        committed = committed.max(Cursor::at(vs + 1));
    }

    if committed > *cursor {
        match save_cursor(ctx.db, agent, committed).await {
            Ok(()) => *cursor = committed,
            Err(e) => match stopped {
                Some(stop) => {
                    tracing::error!(
                        "catch_up for {agent} could not save cursor {committed} on {stop:?}: {}",
                        chain(&e)
                    );
                    return Ok(ControlFlow::Break(stop));
                }
                None => return Err(e),
            },
        }
    }
    if let Some(stop) = stopped {
        tracing::info!(
            "catch_up for {agent} stopped at cursor {}: {stop:?}",
            *cursor
        );
        return Ok(ControlFlow::Break(stop));
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

/// [`subscribe_wake`] for `ctx.owner`, raced against `ctx.token`. Returns
/// `None` when the token is cancelled first. A subscribe in flight at the
/// cancel is given up to [`LIVE_KILL_TIMEOUT`] to finish, and its live query
/// is then ended with [`end_live`].
async fn subscribe_unless_cancelled<T: SurrealValue>(
    ctx: &LoopCtx<'_, T>,
) -> Option<Result<(QueryStream<Notification<Value>>, Uuid)>> {
    if ctx.token.is_cancelled() {
        return None;
    }
    let mut subscribe = std::pin::pin!(subscribe_wake(ctx.db, &ctx.owner, ctx.agent));
    tokio::select! {
        biased;
        subscribed = &mut subscribe => return Some(subscribed),
        () = ctx.token.cancelled() => {}
    }
    if let Ok(Ok((stream, live_id))) = timeout(LIVE_KILL_TIMEOUT, subscribe).await {
        end_live(ctx.db, ctx.agent, live_id, stream).await;
    }
    None
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

    /// [`first_ready_error`] returns a listen loop's sent error over an
    /// earlier `ReadyTimeout` or `ListenLoopDropped`, falls back to the first
    /// of those when no loop sent one, and returns `Ok(())` when every loop
    /// signalled ready.
    #[test]
    fn first_ready_error_prefers_a_sent_error() {
        let timeout_err = || Error::ReadyTimeout {
            agent: "ann".to_string(),
            timeout: Duration::from_secs(10),
        };
        let dropped = || Error::ListenLoopDropped {
            agent: "cy".to_string(),
        };
        let sent = || Error::LiveQueryIdMissing {
            agent: "bo".to_string(),
        };

        let picked = first_ready_error(vec![
            Err(timeout_err()),
            Ok(()),
            Err(sent()),
            Err(dropped()),
        ]);
        assert!(
            matches!(&picked, Err(Error::LiveQueryIdMissing { agent }) if agent == "bo"),
            "a sent error must win over an earlier timeout, got {picked:?}"
        );
        let picked = first_ready_error(vec![Ok(()), Err(dropped()), Err(timeout_err())]);
        assert!(
            matches!(&picked, Err(Error::ListenLoopDropped { agent }) if agent == "cy"),
            "with no sent error the first fallback must be returned, got {picked:?}"
        );
        let picked = first_ready_error(vec![Ok(()), Ok(())]);
        assert!(picked.is_ok(), "all ready must be Ok, got {picked:?}");
    }

    /// [`first_ready_error`] returns [`Error::Cancelled`] over an earlier
    /// `ReadyTimeout` and an earlier `ListenLoopDropped`, and the first sent
    /// error when a `Cancelled` and another sent error are both present.
    #[test]
    fn first_ready_error_prefers_cancelled_over_timeout_and_dropped() {
        let cancelled = || Error::Cancelled {
            agent: "di".to_string(),
        };
        let picked = first_ready_error(vec![
            Err(Error::ReadyTimeout {
                agent: "ann".to_string(),
                timeout: Duration::from_secs(10),
            }),
            Err(Error::ListenLoopDropped {
                agent: "cy".to_string(),
            }),
            Err(cancelled()),
        ]);
        assert!(
            matches!(&picked, Err(Error::Cancelled { agent }) if agent == "di"),
            "Cancelled must win over an earlier ReadyTimeout and ListenLoopDropped, got {picked:?}"
        );
        let picked = first_ready_error(vec![
            Err(Error::LiveQueryIdMissing {
                agent: "bo".to_string(),
            }),
            Err(cancelled()),
        ]);
        assert!(
            matches!(&picked, Err(Error::LiveQueryIdMissing { agent }) if agent == "bo"),
            "the first sent error must win over a later Cancelled, got {picked:?}"
        );
    }

    /// [`reject_duplicate_names`] returns [`Error::DuplicateAgentName`] for
    /// the first repeated name and `Ok(())` when every name is distinct.
    #[test]
    fn reject_duplicate_names_finds_the_first_repeat() {
        let names = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let repeated = reject_duplicate_names(&names(&["ann", "bo", "cy", "bo", "ann"]));
        assert!(
            matches!(&repeated, Err(Error::DuplicateAgentName { name }) if name == "bo"),
            "expected DuplicateAgentName for 'bo', got {repeated:?}"
        );
        let distinct = reject_duplicate_names(&names(&["ann", "bo", "cy"]));
        assert!(
            distinct.is_ok(),
            "distinct names must be accepted, got {distinct:?}"
        );
    }

    /// [`deliver_page`] on a crafted page: `delete` and `define_table` ops and a
    /// message for another agent are skipped without an error line; an
    /// unknown op key and a record without a record-id `out` are each logged
    /// at error level and skipped; the message for the owner is put on the
    /// bus; the cursor moves to the start of the next versionstamp.
    #[tokio::test(flavor = "current_thread")]
    async fn deliver_page_skips_non_messages_and_logs_malformed_changes() {
        let db: &'static Surreal<any::Any> = Box::leak(Box::new(mem_db().await));
        let owner = RecordId::new(AGENT_TABLE, "frank");
        let other = RecordId::new(AGENT_TABLE, "grace");
        let (stream, _live_id) = subscribe_wake(db, &owner, "frank")
            .await
            .expect("subscribe");
        let mut wakes = Wakes::new(stream);
        let (inbox_tx, inbox_rx) = async_channel::bounded::<Delivery<String>>(4);
        let ctx = LoopCtx {
            db,
            owner: owner.clone(),
            agent: "frank",
            inbox_tx,
            token: CancellationToken::new(),
        };

        let object = |pairs: Vec<(&str, Value)>| {
            Value::Object(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
        };
        let message = |out: Option<RecordId>, text: &str| {
            Message::<String> {
                id: None,
                r#in: Some(other.clone()),
                out,
                payload: text.to_string(),
                created: None,
            }
            .into_value()
        };
        let changes = vec![
            object(vec![(
                "delete",
                object(vec![("id", owner.clone().into_value())]),
            )]),
            object(vec![("define_table", object(vec![]))]),
            object(vec![("weird_op", 1i64.into_value())]),
            object(vec![("update", message(Some(other.clone()), "for grace"))]),
            object(vec![("update", message(None, "no out"))]),
            object(vec![("update", message(Some(owner.clone()), "for frank"))]),
        ];
        let page = vec![object(vec![
            ("versionstamp", 5i64.into_value()),
            ("changes", Value::Array(changes.into_iter().collect())),
        ])];

        let sink = LogSink::default();
        let writer = sink.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);
        let mut cursor = Cursor::at(5);
        let flow = deliver_page(&ctx, &mut cursor, &mut wakes, page)
            .await
            .expect("deliver_page");
        drop(guard);

        assert_eq!(flow, ControlFlow::Continue(()), "the page must complete");
        assert_eq!(cursor, Cursor::at(6), "cursor after the page");
        let delivered = inbox_rx.try_recv().expect("one delivery on the bus");
        assert_eq!(delivered.message.payload, "for frank");
        assert!(
            inbox_rx.try_recv().is_err(),
            "only the owner's message may reach the bus"
        );

        let logs =
            String::from_utf8(sink.0.lock().expect("log sink lock").clone()).expect("utf-8 logs");
        let errors: Vec<&str> = logs.lines().filter(|l| l.contains("ERROR")).collect();
        assert_eq!(
            errors.len(),
            2,
            "expected exactly the unknown-op and missing-out errors; got {errors:?}"
        );
        assert!(
            errors.iter().any(|l| l.contains("change 2")
                && l.contains("unknown op")
                && l.contains("weird_op")),
            "no error line for the unknown op; got {errors:?}"
        );
        assert!(
            errors
                .iter()
                .any(|l| l.contains("change 4") && l.contains("no record-id `out`")),
            "no error line for the record without `out`; got {errors:?}"
        );
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
        tx.send(Ok(())).expect("send ready");
        await_ready("frank".to_string(), rx, Duration::from_secs(30))
            .await
            .expect("a signalled ready must succeed");
    }

    /// A startup error sent on the ready channel is returned as is.
    #[tokio::test]
    async fn await_ready_returns_the_sent_error() {
        let (tx, rx) = oneshot::channel();
        tx.send(Err(Error::LiveQueryIdMissing {
            agent: "frank".to_string(),
        }))
        .expect("send startup error");
        let err = await_ready("frank".to_string(), rx, Duration::from_secs(30))
            .await
            .expect_err("a sent startup error must be returned");
        assert!(
            matches!(err, Error::LiveQueryIdMissing { ref agent } if agent == "frank"),
            "expected the sent LiveQueryIdMissing for 'frank', got {err:?}"
        );
    }

    /// [`parse_cursor`] reads `versionstamp` and `change_index`, defaults a
    /// missing `change_index` to 0, and rejects a non-object row, a missing,
    /// non-integer or negative `versionstamp`, and a non-integer or negative
    /// `change_index`; and [`position_within`] with `max = u32::MAX` accepts
    /// 0 and `u32::MAX` and rejects `u32::MAX + 1` and -1.
    #[test]
    fn parse_cursor_accepts_valid_rows_and_rejects_malformed_ones() {
        let max = u64::from(u32::MAX);
        let top = i64::from(u32::MAX);
        assert_eq!(position_within(0, max), Some(0));
        assert_eq!(position_within(top, max), Some(top));
        assert_eq!(position_within(top + 1, max), None, "one above max");
        assert_eq!(position_within(-1, max), None, "negative");

        let row = |pairs: Vec<(&str, Value)>| {
            Value::Object(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
        };
        let int = |n: i64| n.into_value();
        let text = |s: &str| s.to_string().into_value();
        let full = row(vec![("versionstamp", int(7)), ("change_index", int(2))]);
        assert_eq!(
            parse_cursor("frank", &full).expect("full row"),
            Cursor {
                versionstamp: 7,
                change_index: 2
            }
        );
        let old = row(vec![("versionstamp", int(7))]);
        assert_eq!(
            parse_cursor("frank", &old).expect("row without change_index"),
            Cursor::at(7)
        );
        for bad in [
            row(vec![("versionstamp", text("7"))]),
            row(vec![]),
            row(vec![("versionstamp", int(7)), ("change_index", text("x"))]),
            row(vec![("versionstamp", int(-1))]),
            row(vec![("versionstamp", int(7)), ("change_index", int(-1))]),
            int(7),
        ] {
            let err =
                parse_cursor("frank", &bad).expect_err("a malformed cursor row must not parse");
            assert!(
                matches!(err, Error::CursorLoad { ref agent, .. } if agent == "frank"),
                "expected CursorLoad for {bad:?}, got {err:?}"
            );
        }
    }
}
