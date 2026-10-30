use std::{collections::HashMap, marker::PhantomData, ops::ControlFlow, sync::Arc};

use async_channel::{Receiver, Sender};
use futures::stream::{FuturesUnordered, Stream};
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

/// `LIMIT` of each `SHOW CHANGES` page.
const CATCHUP_BATCH: usize = 1000;

/// Reconnect backoff bounds for the LIVE wake-up subscription.
const RECONNECT_BACKOFF_START: Duration = Duration::from_millis(200);
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Maximum number of already-ready wakes [`Wakes::record`] reads after the
/// one it is given.
const WAKE_DRAIN_MAX: usize = 64;

/// Maximum time [`kill_live`] waits for a `KILL` of a live query, and the time
/// [`finish_subscribe`] waits for a reply before it logs a warn.
const LIVE_KILL_TIMEOUT: Duration = Duration::from_secs(2);

/// Outcome of [`subscribe_wake`]: the wake stream and its live-query id.
type Subscribed = Result<(QueryStream<Notification<Value>>, Uuid)>;

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
    /// Create (or reuse) the record `agent:<name>` and its durable-log cursor
    /// `cursor:<name>`.
    ///
    /// Validates `name` first: non-empty, ASCII alphanumeric or underscore,
    /// else [`Error::InvalidAgentName`]. An existing `agent` record and an
    /// existing `cursor` row are kept unchanged. When `cursor:<name>` is
    /// absent, it is created in the same transaction that checks for
    /// `agent:<name>`: for a new agent, 1 above the highest versionstamp in
    /// the whole `message` changefeed at the time of the read (1 for an empty
    /// feed), read in pages until a page holds fewer than 1000 changesets, in
    /// a database where `message` is the one table with a changefeed; for an
    /// agent whose record already existed, at versionstamp 0 with no changefeed
    /// read, so the next catch-up reads the whole retained changefeed, and
    /// that start is logged at warn with the agent and the position as fields.
    /// Both records are written in one transaction, so this does not write
    /// `agent:<name>` without `cursor:<name>`.
    ///
    /// Errors: [`Error::InvalidAgentName`] for a rejected `name`;
    /// [`Error::AgentCreate`] when a database call fails (the cursor or agent
    /// existence check, a changefeed page, the transaction, or the read of
    /// the agent record), with that call's error as its source, or when a
    /// changefeed page is not an array, is a full page with no integer
    /// versionstamp, or ends below its `SINCE` or at `i64::MAX`, in which case
    /// neither record is written;
    /// [`Error::AgentCreateEmpty`] when the agent record reads back as absent.
    pub async fn new(name: &str) -> Result<Self> {
        Self::create(name, None, &mut None).await
    }

    /// [`Agent::new`] with `bound` on each database call: under `Some`, a
    /// call also ends in [`Error::AgentCreateTimeout`] or
    /// [`Error::Cancelled`] (see [`bounded`]). `walked` holds the result of
    /// [`latest_versionstamp`]: a `Some` is used with no changefeed read, and
    /// a read stores its result there.
    async fn create(
        name: &str,
        bound: Option<&CallBound<'_>>,
        walked: &mut Option<i64>,
    ) -> Result<Self> {
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(Error::InvalidAgentName {
                name: name.to_string(),
            });
        }

        let db = bounded(name, bound, sdb::SurrealDBWrapper::connection()).await?;
        Self::create_on(db, name, bound, walked, |since| {
            query_changes(db, since, CATCHUP_BATCH)
        })
        .await
    }

    /// The records of [`Agent::new`] on `db`, reading each changefeed page
    /// through `fetch_page`. `walked` is as in [`Agent::create`].
    async fn create_on<F, Fut>(
        db: &Surreal<any::Any>,
        name: &str,
        bound: Option<&CallBound<'_>>,
        walked: &mut Option<i64>,
        fetch_page: F,
    ) -> Result<Self>
    where
        F: FnMut(i64) -> Fut,
        Fut: Future<Output = std::result::Result<Value, surrealdb::Error>>,
    {
        let create_err = |source| Error::AgentCreate {
            agent: name.to_string(),
            source: Box::new(source),
        };
        let agent_id = RecordId::new(AGENT_TABLE, name);
        let cursor_id = RecordId::new(CURSOR_TABLE, name);
        let stored_id = |id: RecordId| async move {
            db.query("SELECT VALUE id FROM ONLY $id")
                .bind(("id", id))
                .await?
                .take::<Option<RecordId>>(0)
        };

        let cursor_present = bounded(name, bound, stored_id(cursor_id.clone()))
            .await?
            .map_err(create_err)?;
        let cursor_absent = cursor_present.is_none();
        let replay = Cursor::at(0);
        let start = if cursor_absent {
            let agent_present = bounded(name, bound, stored_id(agent_id.clone()))
                .await?
                .map_err(create_err)?;
            match (agent_present, *walked) {
                (Some(_), _) => replay,
                (None, Some(latest)) => Cursor::at(latest + 1),
                (None, None) => {
                    let latest = latest_versionstamp(name, bound, fetch_page).await?;
                    *walked = Some(latest);
                    Cursor::at(latest + 1)
                }
            }
        } else {
            replay
        };

        let cursor_stmts = if cursor_absent {
            "LET $replayed = $existed AND ((SELECT VALUE id FROM ONLY $cursor) = NONE); \
             LET $initial = IF $existed { $replay } ELSE { $start }; \
             IF (SELECT VALUE id FROM ONLY $cursor) = NONE { CREATE $cursor CONTENT $initial };"
        } else {
            "LET $replayed = false;"
        };
        let write = async {
            let mut response = db
                .query(format!(
                    "BEGIN TRANSACTION; \
                     LET $existed = (SELECT VALUE id FROM ONLY $agent) != NONE; \
                     IF !$existed {{ \
                         CREATE $agent CONTENT {{ name: $name, created: time::now() }} \
                     }}; \
                     {cursor_stmts} \
                     RETURN $replayed; \
                     COMMIT TRANSACTION;"
                ))
                .bind(("agent", agent_id.clone()))
                .bind(("cursor", cursor_id))
                .bind(("name", name.to_string()))
                .bind(("start", start))
                .bind(("replay", replay))
                .await?
                .check()?;
            // The `RETURN` result is the one before `COMMIT`'s.
            let returned = response.num_statements().saturating_sub(2);
            response.take::<Option<bool>>(returned)
        };
        let replayed = bounded(name, bound, write).await?.map_err(create_err)?;
        if replayed == Some(true) {
            tracing::warn!(
                agent = name,
                versionstamp = replay.versionstamp,
                change_index = replay.change_index,
                "created a missing cursor for an existing agent at the start of the retained \
                 changefeed"
            );
        }

        let read_back = async { db.select::<Option<Agent>>(agent_id).await };
        bounded(name, bound, read_back)
            .await?
            .map_err(create_err)?
            .ok_or_else(|| Error::AgentCreateEmpty {
                agent: name.to_string(),
            })
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
    /// signal (cursor load, which fails with [`Error::CursorLoad`] when
    /// `cursor:<agent>` is missing; first subscribe; first page query) is
    /// logged with its source chain and sent as `Err` on `ready_tx`, and the
    /// loop ends. Each of those awaits is raced against `token`; a cancel
    /// before the ready signal sends [`Error::Cancelled`] naming this agent on
    /// `ready_tx`, before any cleanup await, and the loop ends. A subscribe in
    /// flight at a cancel does not hold up the loop: a spawned task finishes
    /// it (see [`race_subscribe`]). The loop
    /// ends when its ready send fails, or when `token` is cancelled while it
    /// waits for `start`. Later failures reconnect with capped exponential
    /// backoff. While a delivery waits on a
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

        // Cursor: resume from the position `Agent::new` guarantees exists.
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
            let Some(subscribed) = subscribe_unless_cancelled(&ctx, &mut ready_tx).await else {
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
                            let fetched = fetch_page(&ctx, cursor).await;
                            // The first page decides the startup outcome, which
                            // is sent on `ready_tx` before any cleanup await.
                            let page = match (fetched, ready_tx.take()) {
                                (Ok(ControlFlow::Continue(page)), None) => page,
                                (Ok(ControlFlow::Continue(page)), Some(tx)) => {
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
                                    page
                                }
                                (Ok(ControlFlow::Break(stop)), None) => {
                                    return Ok(ControlFlow::Break(stop));
                                }
                                (Ok(ControlFlow::Break(_)), Some(tx)) => {
                                    cancel_startup(name, tx);
                                    return Ok(ControlFlow::Break(Stop::StartupReported));
                                }
                                (Err(e), None) => return Err(e),
                                (Err(e), Some(tx)) => {
                                    fail_startup(name, tx, e);
                                    return Ok(ControlFlow::Break(Stop::StartupReported));
                                }
                            };
                            if page.is_empty() {
                                return Ok(ControlFlow::Continue(()));
                            }
                            let full = page.len() >= CATCHUP_BATCH;
                            let page_start = cursor;
                            if let ControlFlow::Break(stop) =
                                deliver_page(&ctx, &mut cursor, &mut wakes, page).await?
                            {
                                return Ok(ControlFlow::Break(stop));
                            }
                            if !full {
                                return Ok(ControlFlow::Continue(()));
                            }
                            if cursor == page_start {
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
                    stop.log(name);
                    return;
                }
                Exit::CatchUp(e) => {
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
    /// Each database call of [`Agent::new`] (each changefeed page among them)
    /// is bounded by the 10 s ready timeout, which returns
    /// [`Error::AgentCreateTimeout`] for that agent, and raced against the
    /// coalition's token. The changefeed is read at most once, for the first
    /// new agent, and that result starts each later new agent. After the last
    /// `Agent::new` call, waits for every `listen_loop` to signal that its
    /// LIVE query is registered and its first catch-up page query succeeded.
    /// Each wait is bounded by the ready timeout. Each loop holds at its ready
    /// point, and puts nothing on the bus, until every loop has signalled
    /// ready; this then sends the start signal and returns `Ok`. On any
    /// failure it cancels every spawned loop and waits for each to end before
    /// it returns the error. The first error a loop sends (including
    /// [`Error::Cancelled`]) is returned as soon as these waits see it, in
    /// preference to [`Error::ReadyTimeout`] and [`Error::ListenLoopDropped`]; among
    /// those two, the one for the earliest name in `names` is returned.
    ///
    /// If the coalition's token is cancelled before every loop is ready, this
    /// returns [`Error::Cancelled`]: with `agent: None` when the token is
    /// cancelled before any database call (it is checked first) or when it is
    /// found cancelled after the readiness waits end in `Ok` or
    /// `ReadyTimeout`; with `agent: Some(name)` when the cancel
    /// interrupts that agent's `Agent::new` call or its loop's startup.
    /// Dropping the returned future before it completes cancels the token,
    /// which ends every spawned loop and the retention sweep; the drop does
    /// not wait for them.
    pub async fn new(names: Vec<String>) -> Result<Self> {
        Self::new_with_ready_timeout(names, READY_TIMEOUT).await
    }

    /// Like [`Coalition::new`], with `ready_timeout` in place of the 10 s
    /// ready timeout for each database call of `Agent::new` and each readiness
    /// wait.
    pub async fn new_with_ready_timeout(
        names: Vec<String>,
        ready_timeout: Duration,
    ) -> Result<Self> {
        Self::build(names, ready_timeout, CancellationToken::new()).await
    }

    /// Like [`Coalition::new`], with a root token that is a child of
    /// `parent`: cancelling `parent` cancels the coalition. A `parent` that is
    /// already cancelled returns [`Error::Cancelled`] with `agent: None`
    /// before any database call.
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
            return Err(Error::Cancelled { agent: None });
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

        let bound = CallBound {
            per_call: ready_timeout,
            token: &cancellation_token,
        };
        let mut ready_rxs = Vec::with_capacity(names.len());
        let mut walked = None;
        for name in names {
            let agent = match Agent::create(&name, Some(&bound), &mut walked).await {
                Ok(agent) => agent,
                Err(err) => {
                    abort_build(&cancellation_token, &task_tracker).await;
                    return Err(err);
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
        // (LIVE query registered, first catch-up page query succeeded), or
        // until the first error a loop sent. Each wait is bounded by
        // ready_timeout.
        let outcomes: FuturesUnordered<_> = ready_rxs
            .into_iter()
            .enumerate()
            .map(|(index, (name, rx))| async move {
                (index, await_ready(name, rx, ready_timeout).await)
            })
            .collect();
        let outcome = first_ready_error(outcomes).await;
        if let Err(err) = cancel_override(outcome, cancellation_token.is_cancelled()) {
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
    /// retention sweep have ended. A subscribe in flight at the cancel is
    /// finished by a task that this does not wait for, so its `KILL` can run
    /// after this returns.
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

/// The bound `Coalition::new` puts on each database call of [`Agent::create`].
struct CallBound<'a> {
    per_call: Duration,
    token: &'a CancellationToken,
}

/// Await `call`, a step of creating `agent`. With no `bound` this is the
/// call's output. With `bound`, the call is raced against `bound.token`, which
/// returns [`Error::Cancelled`] for `agent`, and limited to `bound.per_call`,
/// which returns [`Error::AgentCreateTimeout`] for `agent`.
async fn bounded<F: Future>(
    agent: &str,
    bound: Option<&CallBound<'_>>,
    call: F,
) -> Result<F::Output> {
    let Some(bound) = bound else {
        return Ok(call.await);
    };
    match timeout(bound.per_call, bound.token.run_until_cancelled(call)).await {
        Ok(Some(output)) => Ok(output),
        Ok(None) => Err(Error::Cancelled {
            agent: Some(agent.to_string()),
        }),
        Err(_elapsed) => Err(Error::AgentCreateTimeout {
            agent: agent.to_string(),
            timeout: bound.per_call,
        }),
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

/// Pick the error to return from `outcomes`, a stream of `(loop index,
/// readiness outcome)` pairs: the first error a listen loop sent that the
/// stream yields (any variant other than [`Error::ReadyTimeout`] or
/// [`Error::ListenLoopDropped`], so including [`Error::Cancelled`]), returned
/// as soon as it is yielded without reading further outcomes; else the
/// `ReadyTimeout` or `ListenLoopDropped` with the lowest loop index; else
/// `Ok(())`.
async fn first_ready_error(outcomes: impl Stream<Item = (usize, Result<()>)>) -> Result<()> {
    let mut outcomes = std::pin::pin!(outcomes);
    let mut fallback: Option<(usize, Error)> = None;
    while let Some((index, outcome)) = outcomes.next().await {
        match outcome {
            Ok(()) => {}
            Err(e @ (Error::ReadyTimeout { .. } | Error::ListenLoopDropped { .. })) => {
                if fallback.as_ref().is_none_or(|(first, _)| index < *first) {
                    fallback = Some((index, e));
                }
            }
            Err(e) => return Err(e),
        }
    }
    fallback.map_or(Ok(()), |(_, e)| Err(e))
}

/// The readiness `outcome` to return when the coalition token's cancel state
/// is `cancelled`: [`Error::Cancelled`] with `agent: None` in place of
/// `Ok(())` or [`Error::ReadyTimeout`] when `cancelled` is true; `outcome`
/// unchanged otherwise.
fn cancel_override(outcome: Result<()>, cancelled: bool) -> Result<()> {
    match outcome {
        Ok(()) | Err(Error::ReadyTimeout { .. }) if cancelled => {
            Err(Error::Cancelled { agent: None })
        }
        other => other,
    }
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
        agent: Some(agent.to_string()),
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

/// The persisted cursor for `agent`. A missing row returns
/// [`Error::CursorLoad`].
async fn initial_cursor(db: &Surreal<any::Any>, agent: &str) -> Result<Cursor> {
    load_cursor(db, agent)
        .await?
        .ok_or_else(|| Error::CursorLoad {
            agent: agent.to_string(),
            source: reply_error(format!("no {CURSOR_TABLE}:{agent} row")),
        })
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

/// What [`page_highest`] reads from one snapshot page: its highest integer
/// versionstamp, and whether it holds [`CATCHUP_BATCH`] changesets or more.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SnapshotPage {
    highest: Option<i64>,
    full: bool,
}

/// The [`SnapshotPage`] of `reply`, the `SHOW CHANGES` page read `SINCE since`
/// for a new `agent`'s snapshot.
///
/// A changeset that is not an object or has no integer versionstamp is logged
/// at error level and skipped. [`Error::AgentCreate`] for `agent` when `reply`
/// is not an array, when a full page has no integer versionstamp, or when its
/// highest versionstamp is below `since` or is `i64::MAX`, either of which
/// leaves no later page to read.
fn page_highest(agent: &str, since: i64, reply: &Value) -> Result<SnapshotPage> {
    let malformed = |message: String| Error::AgentCreate {
        agent: agent.to_string(),
        source: reply_error(message),
    };
    let Value::Array(changesets) = reply else {
        return Err(malformed(format!(
            "SHOW CHANGES reply is not an array: {reply:?}"
        )));
    };
    let full = changesets.len() >= CATCHUP_BATCH;
    let mut highest = None;
    for changeset in changesets.iter() {
        match versionstamp_of(changeset) {
            Some(vs) => highest = highest.max(Some(vs)),
            None => tracing::error!(
                "snapshot for {agent}: changeset is not an object with an integer versionstamp, \
                 skipped: {changeset:?}"
            ),
        }
    }
    match highest {
        Some(vs) if vs >= since && vs < i64::MAX => Ok(SnapshotPage { highest, full }),
        Some(vs) => Err(malformed(format!(
            "SHOW CHANGES SINCE {since} reply ends at versionstamp {vs}"
        ))),
        None if full => Err(malformed(format!(
            "SHOW CHANGES SINCE {since} reply has no integer versionstamp in {} changesets",
            changesets.len()
        ))),
        None => Ok(SnapshotPage { highest, full }),
    }
}

/// Highest versionstamp in the message changefeed, for a new agent's starting
/// cursor; 0 when the feed is empty. Reads pages through `fetch_page` from
/// `SINCE 0`, each next page `SINCE` 1 above the last page's highest, until a
/// page is not full, each read awaited through [`bounded`]. A failed
/// read maps to [`Error::AgentCreate`] for `agent`; see [`page_highest`] for a
/// malformed page.
async fn latest_versionstamp<F, Fut>(
    agent: &str,
    bound: Option<&CallBound<'_>>,
    mut fetch_page: F,
) -> Result<i64>
where
    F: FnMut(i64) -> Fut,
    Fut: Future<Output = std::result::Result<Value, surrealdb::Error>>,
{
    let mut latest = 0;
    let mut since = 0;
    loop {
        let reply = bounded(agent, bound, fetch_page(since))
            .await?
            .map_err(|source| Error::AgentCreate {
                agent: agent.to_string(),
                source: Box::new(source),
            })?;
        let page = page_highest(agent, since, &reply)?;
        if let Some(highest) = page.highest {
            latest = highest;
            since = highest + 1;
        }
        if !page.full {
            return Ok(latest);
        }
    }
}

/// Run `SHOW CHANGES FOR TABLE message SINCE <since> LIMIT <limit>` and return
/// statement 0's result. Errors map to [`Error::CatchUp`] for `agent`.
async fn show_changes(
    db: &Surreal<any::Any>,
    agent: &str,
    since: i64,
    limit: usize,
) -> Result<Value> {
    query_changes(db, since, limit)
        .await
        .map_err(|source| Error::CatchUp {
            agent: agent.to_string(),
            source: Box::new(source),
        })
}

/// Run `SHOW CHANGES FOR TABLE message SINCE <since> LIMIT <limit>` and return
/// statement 0's result, or the datastore error.
async fn query_changes(
    db: &Surreal<any::Any>,
    since: i64,
    limit: usize,
) -> std::result::Result<Value, surrealdb::Error> {
    let q = format!("SHOW CHANGES FOR TABLE {MESSAGE_TABLE} SINCE {since} LIMIT {limit}");
    db.query(&q).await?.take(0)
}

/// Why a catch-up pass stopped before reaching the live edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    /// `token` was cancelled.
    Cancelled,
    /// The bus is closed (every receiver dropped, or `close()` called on any
    /// receiver), so a delivery cannot reach the bus.
    NoReceivers,
    /// The ready send failed: the ready receiver is gone.
    NotStarted,
    /// The first page query was cancelled or failed; that outcome has
    /// already been logged and sent on the ready channel.
    StartupReported,
}

impl Stop {
    /// Log that `agent`'s listen loop ends for this reason; nothing for
    /// [`Stop::StartupReported`], whose outcome is already logged.
    fn log(self, agent: &str) {
        match self {
            Stop::Cancelled => tracing::info!("listen_loop for {agent} received shutdown"),
            Stop::NoReceivers => tracing::info!("delivery bus has no receivers; {agent} stops"),
            Stop::NotStarted => {
                tracing::info!("the ready receiver for {agent} is gone; {agent} stops");
            }
            Stop::StartupReported => {}
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
///
/// Errors: [`Error::LiveQuery`] when the statement fails;
/// [`Error::LiveQueryIdMissing`] when the id cannot be read; [`Error::Stream`]
/// when the stream cannot be opened, after a `KILL` of the live query (see
/// [`stream_or_kill`]).
async fn subscribe_wake(db: &Surreal<any::Any>, owner: &RecordId, agent: &str) -> Subscribed {
    let mut response = db
        .query("LIVE SELECT id FROM message WHERE out = $owner")
        .bind(("owner", owner.clone()))
        .await
        .map_err(|source| Error::LiveQuery {
            agent: agent.to_string(),
            source: Box::new(source),
        })?;
    let live_id = live_id_of(agent, response.take::<Value>(0))?;
    let opened = response.stream::<Notification<Value>>(0);
    let stream = stream_or_kill(db, agent, live_id, opened).await?;
    Ok((stream, live_id))
}

/// The live-query id in `taken`, the read of a `LIVE SELECT` result. A failed
/// read, or a value that is not a uuid, is [`Error::LiveQueryIdMissing`] with
/// that error or that value in its source.
fn live_id_of(agent: &str, taken: std::result::Result<Value, surrealdb::Error>) -> Result<Uuid> {
    let missing = |source| Error::LiveQueryIdMissing {
        agent: agent.to_string(),
        source,
    };
    match taken {
        Ok(Value::Uuid(live_id)) => Ok(live_id),
        Ok(other) => Err(missing(reply_error(format!(
            "live-query id is not a uuid: {other:?}"
        )))),
        Err(source) => Err(missing(Box::new(source))),
    }
}

/// The stream in `opened`. When `opened` is an error, ends live query
/// `live_id` through [`kill_live`], then returns [`Error::Stream`] with that
/// error as its source.
async fn stream_or_kill(
    db: &Surreal<any::Any>,
    agent: &str,
    live_id: Uuid,
    opened: std::result::Result<QueryStream<Notification<Value>>, surrealdb::Error>,
) -> Result<QueryStream<Notification<Value>>> {
    match opened {
        Ok(stream) => Ok(stream),
        Err(source) => {
            kill_live(db, agent, live_id).await;
            Err(Error::Stream {
                agent: agent.to_string(),
                source: Box::new(source),
            })
        }
    }
}

/// [`subscribe_wake`] for `ctx.owner`, raced against `ctx.token` by
/// [`race_subscribe`].
async fn subscribe_unless_cancelled<T: SurrealValue>(
    ctx: &LoopCtx<'_, T>,
    ready_tx: &mut Option<oneshot::Sender<Result<()>>>,
) -> Option<Subscribed> {
    let (db, owner, agent) = (ctx.db, ctx.owner.clone(), ctx.agent.to_string());
    let subscribe = async move { subscribe_wake(db, &owner, &agent).await };
    race_subscribe(ctx, ready_tx, subscribe).await
}

/// Race `subscribe` against `ctx.token`. Returns `None` when the token is
/// cancelled first; if `ready_tx` still holds the ready sender,
/// [`Error::Cancelled`] is sent on it before any further await, else the stop
/// is logged. A `subscribe` in flight at the cancel moves to a spawned
/// [`finish_subscribe`] task, which this does not wait for and which no
/// `TaskTracker` tracks, so `Coalition::shutdown` does not wait for it either.
async fn race_subscribe<T, F>(
    ctx: &LoopCtx<'_, T>,
    ready_tx: &mut Option<oneshot::Sender<Result<()>>>,
    subscribe: F,
) -> Option<Subscribed>
where
    T: SurrealValue,
    F: Future<Output = Subscribed> + Send + 'static,
{
    let cancelled = |ready_tx: &mut Option<oneshot::Sender<Result<()>>>| match ready_tx.take() {
        Some(tx) => cancel_startup(ctx.agent, tx),
        None => Stop::Cancelled.log(ctx.agent),
    };
    if ctx.token.is_cancelled() {
        cancelled(ready_tx);
        return None;
    }
    let mut subscribe = Box::pin(subscribe);
    tokio::select! {
        biased;
        subscribed = &mut subscribe => return Some(subscribed),
        () = ctx.token.cancelled() => {}
    }
    cancelled(ready_tx);
    tokio::spawn(finish_subscribe(ctx.db, ctx.agent.to_string(), subscribe).in_current_span());
    None
}

/// Await `subscribe`, in flight when `agent`'s loop was cancelled, then end
/// the returned live query with [`end_live`]. A failed `subscribe` is logged
/// at warn with the agent and the error chain; a failed `KILL` call is logged
/// by [`kill_live`]. With no reply after [`LIVE_KILL_TIMEOUT`], this logs a
/// warn with the agent and keeps waiting, with no bound. If this is dropped
/// before it finishes, [`UnfinishedSubscribe`] logs a warn with the agent.
async fn finish_subscribe<F>(db: &Surreal<any::Any>, agent: String, subscribe: F)
where
    F: Future<Output = Subscribed>,
{
    let unfinished = UnfinishedSubscribe {
        agent: &agent,
        armed: true,
    };
    let mut subscribe = std::pin::pin!(subscribe);
    let subscribed = match timeout(LIVE_KILL_TIMEOUT, &mut subscribe).await {
        Ok(subscribed) => subscribed,
        Err(_elapsed) => {
            tracing::warn!(
                "subscribe for {agent} has no reply {LIVE_KILL_TIMEOUT:?} after a cancel; the \
                 live query, if registered, is not yet ended"
            );
            subscribe.await
        }
    };
    match subscribed {
        Ok((stream, live_id)) => end_live(db, &agent, live_id, stream).await,
        Err(e) => tracing::warn!("subscribe for {agent} failed after a cancel: {}", chain(&e)),
    }
    unfinished.disarm();
}

/// Logs a warn for `agent` on drop, unless [`UnfinishedSubscribe::disarm`]
/// consumed it.
struct UnfinishedSubscribe<'a> {
    agent: &'a str,
    armed: bool,
}

impl UnfinishedSubscribe<'_> {
    /// Drop this with no log.
    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for UnfinishedSubscribe<'_> {
    fn drop(&mut self) {
        if self.armed {
            tracing::warn!(
                "subscribe finisher for {} dropped before it finished; the live query, if \
                 registered, may not be ended",
                self.agent
            );
        }
    }
}

/// End live query `live_id` through [`kill_live`], then drop `stream`.
async fn end_live(
    db: &Surreal<any::Any>,
    agent: &str,
    live_id: Uuid,
    stream: QueryStream<Notification<Value>>,
) {
    kill_live(db, agent, live_id).await;
    drop(stream);
}

/// `KILL` live query `live_id` through `db` within [`LIVE_KILL_TIMEOUT`],
/// logging a failure or timeout at warn.
async fn kill_live(db: &Surreal<any::Any>, agent: &str, live_id: Uuid) {
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
        warn_kill_live(&e);
    }
}

/// Log a [`kill_live`] failure at warn with its full source chain.
fn warn_kill_live(e: &Error) {
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

    /// `sink`'s logged text, for a test that installed it as the subscriber's
    /// writer.
    fn logged(sink: &LogSink) -> String {
        String::from_utf8(sink.0.lock().expect("log sink lock").clone()).expect("utf-8 logs")
    }

    /// A subscriber writing plain text to a new [`LogSink`].
    fn capturing_subscriber() -> (impl tracing::Subscriber + Send + Sync, LogSink) {
        let sink = LogSink::default();
        let writer = sink.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        (subscriber, sink)
    }

    /// An [`Error::LiveQueryIdMissing`] for `agent`.
    fn id_missing(agent: &str) -> Error {
        Error::LiveQueryIdMissing {
            agent: agent.to_string(),
            source: reply_error("no id".to_string()),
        }
    }

    /// `pairs` as a `Value::Object`.
    fn object(pairs: Vec<(&str, Value)>) -> Value {
        Value::Object(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }

    /// A changeset with `versionstamp` and no changes.
    fn changeset(versionstamp: i64) -> Value {
        object(vec![
            ("versionstamp", versionstamp.into_value()),
            ("changes", Value::Array(Vec::<Value>::new().into())),
        ])
    }

    /// `changesets` as a `SHOW CHANGES` reply.
    fn reply_of(changesets: Vec<Value>) -> Value {
        Value::Array(changesets.into_iter().collect())
    }

    /// A full `SHOW CHANGES` reply: [`CATCHUP_BATCH`] changesets from versionstamp
    /// `first` on.
    fn full_page(first: i64) -> Value {
        let last = first + i64::try_from(CATCHUP_BATCH).expect("batch fits i64");
        reply_of((first..last).map(changeset).collect())
    }

    /// [`page_highest`] on a well-formed page returns its highest
    /// versionstamp in any order, `None` for an empty page, and reports a
    /// page as full only at [`CATCHUP_BATCH`] changesets.
    #[test]
    fn page_highest_reads_a_well_formed_page() {
        let short = |highest| SnapshotPage {
            highest,
            full: false,
        };
        let page = reply_of(vec![changeset(7), changeset(9), changeset(8)]);
        assert_eq!(
            page_highest("frank", 0, &page).expect("page"),
            short(Some(9))
        );
        assert_eq!(
            page_highest("frank", 9, &page).expect("page"),
            short(Some(9))
        );
        assert_eq!(
            page_highest("frank", 0, &reply_of(vec![])).expect("empty page"),
            short(None)
        );

        let top = i64::try_from(CATCHUP_BATCH).expect("batch fits i64");
        assert_eq!(
            page_highest("frank", 0, &full_page(1)).expect("full page"),
            SnapshotPage {
                highest: Some(top),
                full: true
            }
        );
        let one_short = reply_of((1..top).map(changeset).collect());
        assert_eq!(
            page_highest("frank", 0, &one_short).expect("page one below full"),
            short(Some(top - 1))
        );
    }

    /// [`page_highest`] on a reply that is not an array returns
    /// [`Error::AgentCreate`] for the agent, naming the reply.
    #[test]
    fn page_highest_rejects_a_non_array_reply() {
        for reply in [Value::None, 5i64.into_value(), changeset(5)] {
            let result = page_highest("frank", 0, &reply);
            let Err(err @ Error::AgentCreate { .. }) = &result else {
                panic!("expected AgentCreate for {reply:?}, got {result:?}");
            };
            let rendered = chain(err);
            assert!(
                rendered.contains("frank") && rendered.contains("not an array"),
                "the error must name the agent and the reply; got {rendered:?}"
            );
        }
    }

    /// [`page_highest`] logs each changeset with no integer versionstamp at
    /// error level and skips it. With no integer versionstamp in a full page
    /// it returns [`Error::AgentCreate`]; in a short page it returns no
    /// position.
    #[test]
    fn page_highest_skips_malformed_changesets_and_rejects_a_full_malformed_page() {
        let no_stamp = object(vec![("changes", Value::Array(Vec::<Value>::new().into()))]);
        let text_stamp = object(vec![("versionstamp", "7".to_string().into_value())]);
        let mixed = reply_of(vec![
            no_stamp.clone(),
            changeset(4),
            5i64.into_value(),
            text_stamp.clone(),
        ]);
        let short_malformed = reply_of(vec![no_stamp.clone(), text_stamp]);
        let full_malformed = reply_of(vec![no_stamp; CATCHUP_BATCH]);

        let (subscriber, sink) = capturing_subscriber();
        let (highest, short, rejected) = tracing::subscriber::with_default(subscriber, || {
            (
                page_highest("frank", 0, &mixed),
                page_highest("frank", 0, &short_malformed),
                page_highest("frank", 0, &full_malformed),
            )
        });

        assert_eq!(highest.expect("mixed page").highest, Some(4));
        assert_eq!(
            short.expect("a short malformed page has no error"),
            SnapshotPage {
                highest: None,
                full: false
            }
        );
        assert!(
            matches!(rejected, Err(Error::AgentCreate { ref agent, .. }) if agent == "frank"),
            "a full malformed page must return AgentCreate, got {rejected:?}"
        );
        let logs = logged(&sink);
        let errors = logs.lines().filter(|l| l.contains("ERROR")).count();
        assert_eq!(
            errors,
            5 + CATCHUP_BATCH,
            "expected 3 lines for the mixed page, 2 for the short malformed one and one per \
             changeset of the full one; got {errors}"
        );
    }

    /// [`page_highest`] returns [`Error::AgentCreate`] for a page that ends
    /// below `since` or at `i64::MAX`, either of which has no later page.
    #[test]
    fn page_highest_rejects_a_page_with_no_later_page() {
        for (since, vs) in [(10, 9), (0, -1), (0, i64::MAX)] {
            let result = page_highest("frank", since, &reply_of(vec![changeset(vs)]));
            assert!(
                matches!(result, Err(Error::AgentCreate { .. })),
                "SINCE {since} with versionstamp {vs} must return AgentCreate, got {result:?}"
            );
        }
    }

    /// A boxed read of one changefeed page.
    type PageRead =
        std::pin::Pin<Box<dyn Future<Output = std::result::Result<Value, surrealdb::Error>>>>;

    /// `pages` in order as a `fetch_page` for [`latest_versionstamp`], with
    /// each `since` it is called with pushed on `seen`, each read taking
    /// `delay`, and empty replies after the last page.
    fn scripted_pages(
        pages: Vec<Value>,
        seen: &std::cell::RefCell<Vec<i64>>,
        delay: Duration,
    ) -> impl FnMut(i64) -> PageRead {
        let mut pages = pages.into_iter();
        move |since| {
            seen.borrow_mut().push(since);
            let page = pages.next().unwrap_or_else(|| reply_of(vec![]));
            Box::pin(async move {
                sleep(delay).await;
                Ok(page)
            })
        }
    }

    /// [`latest_versionstamp`] reads past full pages, each next `SINCE` 1
    /// above the last highest, and ends on a short or an empty page: pages of
    /// 1000, 1000 and 3 changesets take 3 reads, a feed of exactly one full
    /// page takes 2, an empty feed takes 1, and a feed that gains a changeset
    /// before each read still ends. A short page with no integer versionstamp
    /// logs at error and ends the read at the last position.
    #[tokio::test(flavor = "current_thread")]
    async fn latest_versionstamp_pages_until_a_short_reply() {
        let seen = std::cell::RefCell::new(Vec::new());
        let pages = vec![
            full_page(1),
            full_page(1001),
            reply_of(vec![changeset(2001), changeset(2002), changeset(2003)]),
        ];
        let latest =
            latest_versionstamp("frank", None, scripted_pages(pages, &seen, Duration::ZERO))
                .await
                .expect("snapshot");
        assert_eq!(latest, 2003, "the highest versionstamp of the last page");
        assert_eq!(*seen.borrow(), vec![0, 1001, 2001], "SINCE of each page");

        let seen = std::cell::RefCell::new(Vec::new());
        let latest = latest_versionstamp(
            "frank",
            None,
            scripted_pages(vec![full_page(1)], &seen, Duration::ZERO),
        )
        .await
        .expect("one full page");
        assert_eq!(latest, 1000, "a feed of exactly one full page");
        assert_eq!(
            *seen.borrow(),
            vec![0, 1001],
            "an empty reply ends the read"
        );

        let seen = std::cell::RefCell::new(Vec::new());
        let latest =
            latest_versionstamp("frank", None, scripted_pages(vec![], &seen, Duration::ZERO))
                .await
                .expect("empty feed");
        assert_eq!(latest, 0, "an empty feed");
        assert_eq!(*seen.borrow(), vec![0], "one read of an empty feed");

        let reads = std::cell::Cell::new(0);
        let tail = std::cell::Cell::new(1000);
        let latest = latest_versionstamp("frank", None, |since| {
            reads.set(reads.get() + 1);
            assert!(
                reads.get() <= 2,
                "a feed that gains a changeset before each read took {} reads",
                reads.get()
            );
            let page = reply_of((since.max(1)..=tail.get()).map(changeset).collect());
            tail.set(tail.get() + 1);
            async move { Ok(page) }
        })
        .await
        .expect("growing feed");
        assert_eq!(latest, 1001, "the growing feed's tail at the last read");

        let seen = std::cell::RefCell::new(Vec::new());
        let pages = vec![full_page(1), reply_of(vec![object(vec![])])];
        let (subscriber, sink) = capturing_subscriber();
        let guard = tracing::subscriber::set_default(subscriber);
        let latest =
            latest_versionstamp("frank", None, scripted_pages(pages, &seen, Duration::ZERO)).await;
        drop(guard);
        assert_eq!(
            latest.expect("a short page with no versionstamp is not an error"),
            1000,
            "the position before that page"
        );
        assert_eq!(
            *seen.borrow(),
            vec![0, 1001],
            "the short page ends the read"
        );
        let logs = logged(&sink);
        assert_eq!(
            logs.lines().filter(|l| l.contains("ERROR")).count(),
            1,
            "one error line for the changeset with no versionstamp; got {logs:?}"
        );
    }

    /// [`latest_versionstamp`] under a bound that one page meets and the whole
    /// read does not: 5 reads of 60 ms under a 200 ms bound take at least
    /// 300 ms and return the position.
    #[tokio::test]
    async fn latest_versionstamp_bounds_each_page_not_the_whole_read() {
        let per_call = Duration::from_millis(200);
        let token = CancellationToken::new();
        let bound = CallBound {
            per_call,
            token: &token,
        };
        let seen = std::cell::RefCell::new(Vec::new());
        let pages = [1, 1001, 2001, 3001].map(full_page).to_vec();
        let began = tokio::time::Instant::now();
        let latest = latest_versionstamp(
            "frank",
            Some(&bound),
            scripted_pages(pages, &seen, Duration::from_millis(60)),
        )
        .await;
        let elapsed = began.elapsed();
        assert_eq!(
            latest.expect("a read longer than one bound must succeed"),
            4000
        );
        assert_eq!(
            *seen.borrow(),
            vec![0, 1001, 2001, 3001, 4001],
            "SINCE of each page"
        );
        assert!(
            elapsed >= Duration::from_millis(300),
            "5 reads of 60 ms must take at least 300 ms; they took {elapsed:?}"
        );
    }

    /// [`latest_versionstamp`] under a bound returns
    /// [`Error::AgentCreateTimeout`] for a page that outlasts the bound, and
    /// [`Error::Cancelled`] for a token cancelled during a page, both after
    /// an earlier page succeeded, each within 5 s.
    #[tokio::test]
    async fn latest_versionstamp_ends_on_a_stalled_or_cancelled_page() {
        let limit = Duration::from_secs(5);
        let per_call = Duration::from_millis(50);
        let stalled_second_page = |calls: &std::cell::Cell<u32>| {
            calls.set(calls.get() + 1);
            calls.get() > 1
        };

        let token = CancellationToken::new();
        let bound = CallBound {
            per_call,
            token: &token,
        };
        let calls = std::cell::Cell::new(0);
        let read = latest_versionstamp("frank", Some(&bound), |_| {
            let stall = stalled_second_page(&calls);
            async move {
                if stall {
                    std::future::pending::<()>().await;
                }
                Ok(full_page(1))
            }
        });
        let result = timeout(limit, read).await;
        assert!(
            matches!(
                &result,
                Ok(Err(Error::AgentCreateTimeout { agent, timeout }))
                    if agent == "frank" && *timeout == per_call
            ),
            "expected AgentCreateTimeout for frank, got {result:?}"
        );

        let token = CancellationToken::new();
        let bound = CallBound {
            per_call: Duration::from_secs(30),
            token: &token,
        };
        let calls = std::cell::Cell::new(0);
        let cancel = token.clone();
        let read = latest_versionstamp("frank", Some(&bound), |_| {
            let stall = stalled_second_page(&calls);
            let cancel = cancel.clone();
            async move {
                if stall {
                    cancel.cancel();
                    std::future::pending::<()>().await;
                }
                Ok(full_page(1))
            }
        });
        let result = timeout(limit, read).await;
        assert!(
            matches!(&result, Ok(Err(Error::Cancelled { agent: Some(a) })) if a == "frank"),
            "expected Cancelled for frank, got {result:?}"
        );
    }

    /// `db` with the `agent` and `cursor` tables defined.
    async fn define_agent_tables(db: &Surreal<any::Any>) {
        db.query(format!(
            "DEFINE TABLE {AGENT_TABLE}; DEFINE TABLE {CURSOR_TABLE};"
        ))
        .await
        .and_then(|r| r.check())
        .expect("define agent and cursor tables");
    }

    /// Whether `table:name` exists on `db`.
    async fn row_exists(db: &Surreal<any::Any>, table: &str, name: &str) -> bool {
        db.select::<Option<Value>>((table, name))
            .await
            .expect("read row")
            .is_some()
    }

    /// [`Agent::create_on`] with a snapshot reply that is not an array, or a
    /// full one that holds no integer versionstamp, returns
    /// [`Error::AgentCreate`] and writes neither record; with a well-formed
    /// feed it writes both, the cursor 1 above the feed's highest
    /// versionstamp.
    #[tokio::test]
    async fn create_on_writes_nothing_after_a_malformed_snapshot() {
        let db = mem_db().await;
        define_agent_tables(&db).await;

        for (name, reply) in [
            ("nonarray", 5i64.into_value()),
            ("unstamped", reply_of(vec![object(vec![]); CATCHUP_BATCH])),
        ] {
            let result = Agent::create_on(&db, name, None, &mut None, |_| {
                let reply = reply.clone();
                async move { Ok(reply) }
            })
            .await;
            assert!(
                matches!(&result, Err(Error::AgentCreate { agent, .. }) if agent == name),
                "expected AgentCreate for {name}, got {result:?}"
            );
            let (agent_row, cursor_row) = (
                row_exists(&db, AGENT_TABLE, name).await,
                row_exists(&db, CURSOR_TABLE, name).await,
            );
            assert_eq!(
                (agent_row, cursor_row),
                (false, false),
                "a malformed snapshot must write no record for {name}"
            );
        }

        let seen = std::cell::RefCell::new(Vec::new());
        let pages = vec![reply_of(vec![changeset(40)])];
        Agent::create_on(
            &db,
            "wellformed",
            None,
            &mut None,
            scripted_pages(pages, &seen, Duration::ZERO),
        )
        .await
        .expect("create on a well-formed feed");
        let cursor = load_cursor(&db, "wellformed").await.expect("load cursor");
        assert_eq!(cursor, Some(Cursor::at(41)));
        assert!(row_exists(&db, AGENT_TABLE, "wellformed").await);
    }

    /// Create `agent:<name>` on `db`.
    async fn create_agent_row(db: &Surreal<any::Any>, name: &str) {
        db.query(format!(
            "CREATE {AGENT_TABLE}:{name} CONTENT {{ name: '{name}', created: time::now() }}"
        ))
        .await
        .and_then(|r| r.check())
        .expect("create agent row");
    }

    /// [`Agent::create_on`] for an existing agent with no cursor reads no
    /// changefeed page and creates the cursor at versionstamp 0.
    #[tokio::test]
    async fn create_on_reads_no_page_for_an_existing_agent() {
        let db = mem_db().await;
        define_agent_tables(&db).await;
        create_agent_row(&db, "old").await;

        let seen = std::cell::RefCell::new(Vec::new());
        let mut walked = None;
        Agent::create_on(
            &db,
            "old",
            None,
            &mut walked,
            scripted_pages(vec![full_page(1)], &seen, Duration::ZERO),
        )
        .await
        .expect("create on an existing agent");

        assert_eq!(*seen.borrow(), Vec::<i64>::new(), "reads of the changefeed");
        assert_eq!(walked, None, "no read means no result for a later agent");
        let cursor = load_cursor(&db, "old").await.expect("load cursor");
        assert_eq!(cursor, Some(Cursor::at(0)));
    }

    /// [`Agent::create_on`] for an agent whose record appears after the
    /// existence read decides by the transaction's own check: the cursor
    /// starts at versionstamp 0, not after the feed the walk read.
    #[tokio::test]
    async fn create_on_replays_for_an_agent_created_during_the_walk() {
        let db = mem_db().await;
        define_agent_tables(&db).await;

        let db_ref = &db;
        Agent::create_on(&db, "racer", None, &mut None, |_| async move {
            create_agent_row(db_ref, "racer").await;
            Ok(reply_of(vec![changeset(40)]))
        })
        .await
        .expect("create on an agent created during the walk");

        let cursor = load_cursor(&db, "racer").await.expect("load cursor");
        assert_eq!(cursor, Some(Cursor::at(0)));
    }

    /// Two new agents through one `walked` state make the reads of one walk,
    /// and each cursor starts 1 above that walk's result. A failed walk names
    /// the agent that ran it and leaves no result.
    #[tokio::test]
    async fn create_on_walks_once_for_new_agents_sharing_a_state() {
        let db = mem_db().await;
        define_agent_tables(&db).await;

        let seen = std::cell::RefCell::new(Vec::new());
        let pages = vec![full_page(1), reply_of(vec![changeset(1001)])];
        let mut fetch = scripted_pages(pages, &seen, Duration::ZERO);
        let mut walked = None;
        for name in ["first", "second"] {
            Agent::create_on(&db, name, None, &mut walked, &mut fetch)
                .await
                .expect("create a new agent");
        }

        assert_eq!(*seen.borrow(), vec![0, 1001], "the reads of one walk");
        for name in ["first", "second"] {
            let cursor = load_cursor(&db, name).await.expect("load cursor");
            assert_eq!(cursor, Some(Cursor::at(1002)), "cursor of {name}");
        }

        let mut walked = None;
        let result = Agent::create_on(&db, "third", None, &mut walked, |_| async {
            Ok(5i64.into_value())
        })
        .await;
        assert!(
            matches!(&result, Err(Error::AgentCreate { agent, .. }) if agent == "third"),
            "the walk's error must name the agent that ran it, got {result:?}"
        );
        assert_eq!(walked, None, "a failed walk leaves no result");
    }

    /// [`live_id_of`] returns the uuid; for a failed read it carries the
    /// read's error in its source chain, and for another value that value.
    #[test]
    fn live_id_of_keeps_the_failed_read_or_the_unexpected_value() {
        let id = Uuid::new_v4();
        assert_eq!(
            live_id_of("frank", Ok(Value::Uuid(id))).expect("uuid"),
            id,
            "a uuid is the id"
        );

        let failed = Err(surrealdb::Error::internal("take refused".to_string()));
        let unexpected = Ok("not-a-uuid".to_string().into_value());
        for (taken, needle) in [(failed, "take refused"), (unexpected, "not-a-uuid")] {
            let result = live_id_of("frank", taken);
            let Err(err @ Error::LiveQueryIdMissing { agent, .. }) = &result else {
                panic!("expected LiveQueryIdMissing, got {result:?}");
            };
            assert_eq!(agent, "frank");
            let rendered = chain(err);
            assert!(
                rendered.contains(needle),
                "the source must carry {needle:?}; rendered {rendered:?}"
            );
        }
    }

    /// [`stream_or_kill`] with a failed stream open ends the live query, whose
    /// count on the datastore returns to its earlier value, and returns
    /// [`Error::Stream`] with the open's error in its source; with an open
    /// stream it leaves the live query registered.
    #[tokio::test]
    async fn stream_or_kill_ends_the_live_query_of_a_failed_open() {
        let db = mem_db().await;
        let owner = RecordId::new(AGENT_TABLE, "frank");
        let before = message_lives(&db).await.len();

        let (stream, live_id) = subscribe_wake(&db, &owner, "frank")
            .await
            .expect("subscribe");
        let kept = stream_or_kill(&db, "frank", live_id, Ok(stream))
            .await
            .expect("an open stream is returned");
        let during = message_lives(&db).await.len();
        assert_eq!(during, before + 1, "an open stream keeps its live query");

        let opened = Err(surrealdb::Error::internal("no stream".to_string()));
        let result = stream_or_kill(&db, "frank", live_id, opened).await;
        let Err(err @ Error::Stream { agent, .. }) = &result else {
            panic!("expected Stream, got {:?}", result.err());
        };
        assert_eq!(agent, "frank");
        assert!(
            chain(err).contains("no stream"),
            "the source must carry the open's error; got {}",
            chain(err)
        );
        let after = message_lives(&db).await;
        assert_eq!(
            after.len(),
            before,
            "a failed open must end live query {live_id}; datastore lists {after:?}"
        );
        drop(kept);
    }

    /// A [`LoopCtx`] for `frank` on `db` under `token`.
    fn frank_ctx(
        db: &'static Surreal<any::Any>,
        token: CancellationToken,
    ) -> LoopCtx<'static, String> {
        let (inbox_tx, _inbox_rx) = async_channel::bounded(1);
        LoopCtx {
            db,
            owner: RecordId::new(AGENT_TABLE, "frank"),
            agent: "frank",
            inbox_tx,
            token,
        }
    }

    /// Time a cancelled [`race_subscribe`] may take to return in the tests
    /// below; below [`LIVE_KILL_TIMEOUT`], and the held reply never arrives
    /// within it.
    const RACE_RETURN_LIMIT: Duration = Duration::from_secs(1);

    /// A subscribe interrupted by a cancel, whose reply arrives after the
    /// cancel: the server has registered the live query and the reply is held
    /// back. After the cancel, [`race_subscribe`] returns `None` within
    /// [`RACE_RETURN_LIMIT`] with the reply still held, sends
    /// [`Error::Cancelled`] on the ready channel, and the live query is still
    /// registered; once the reply is released the live-query count returns to
    /// its earlier value.
    #[tokio::test]
    async fn subscribe_cancelled_before_its_reply_is_ended_by_the_finisher() {
        let db: &'static Surreal<any::Any> = Box::leak(Box::new(mem_db().await));
        let token = CancellationToken::new();
        let ctx = frank_ctx(db, token.clone());
        let owner = ctx.owner.clone();
        let before = message_lives(db).await.len();

        let (registered_tx, registered_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel::<()>();
        let subscribe = async move {
            let subscribed = subscribe_wake(db, &owner, "frank").await;
            registered_tx.send(()).expect("test holds the receiver");
            let _ = release_rx.await;
            subscribed
        };
        let (ready_tx, mut ready_rx) = oneshot::channel();
        let mut ready = Some(ready_tx);

        let (raced, during) = timeout(RACE_RETURN_LIMIT, async {
            tokio::join!(race_subscribe(&ctx, &mut ready, subscribe), async {
                registered_rx.await.expect("subscribe reports registration");
                let during = message_lives(db).await.len();
                token.cancel();
                during
            })
        })
        .await
        .expect("a cancelled race_subscribe must return with the reply still held");

        assert!(raced.is_none(), "a cancel must return None");
        assert_eq!(during, before + 1, "precondition: the server registered it");
        assert!(
            matches!(
                ready_rx.try_recv(),
                Ok(Err(Error::Cancelled { agent: Some(ref a) })) if a == "frank"
            ),
            "the ready channel must hold Cancelled for frank"
        );
        let held = message_lives(db).await.len();
        assert_eq!(
            held,
            before + 1,
            "precondition: the reply has not arrived, so the live query is still registered"
        );

        let _ = release_tx.send(());
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut after = message_lives(db).await.len();
        while after != before && tokio::time::Instant::now() < deadline {
            sleep(Duration::from_millis(10)).await;
            after = message_lives(db).await.len();
        }
        assert_eq!(
            after, before,
            "the finisher must end the live query once the reply arrives"
        );
    }

    /// Text of the warn line for a [`finish_subscribe`] with no reply within
    /// [`LIVE_KILL_TIMEOUT`].
    const NO_REPLY_WARNING: &str = "has no reply";

    /// Text of the warn line for a [`finish_subscribe`] dropped before it
    /// finished.
    const DROPPED_WARNING: &str = "dropped before it finished";

    /// A reply held past [`LIVE_KILL_TIMEOUT`]: [`finish_subscribe`] logs at
    /// warn with the agent, with the reply still held and the live query
    /// still registered, then ends the live query once the reply arrives, and
    /// logs no drop.
    #[tokio::test(flavor = "current_thread")]
    async fn finish_subscribe_warns_on_a_late_reply_and_still_ends_the_live_query() {
        let db: &'static Surreal<any::Any> = Box::leak(Box::new(mem_db().await));
        let owner = RecordId::new(AGENT_TABLE, "frank");
        let before = message_lives(db).await.len();
        let (stream, live_id) = subscribe_wake(db, &owner, "frank")
            .await
            .expect("subscribe");
        let (release_tx, release_rx) = oneshot::channel::<()>();
        let held = async move {
            let _ = release_rx.await;
            Ok((stream, live_id))
        };

        let (subscriber, sink) = capturing_subscriber();
        let guard = tracing::subscriber::set_default(subscriber);
        let finisher = tokio::spawn(finish_subscribe(db, "frank".to_string(), held));
        sleep(LIVE_KILL_TIMEOUT + Duration::from_millis(300)).await;
        let warned = logged(&sink);
        let during = message_lives(db).await.len();
        let _ = release_tx.send(());
        finisher
            .await
            .expect("the finisher ends once the reply arrives");
        drop(guard);

        assert!(
            warned
                .lines()
                .any(|l| l.contains("WARN") && l.contains("frank") && l.contains(NO_REPLY_WARNING)),
            "no warn line for the held reply; got {warned:?}"
        );
        assert_eq!(
            during,
            before + 1,
            "precondition: the live query is registered"
        );
        let after = message_lives(db).await;
        assert_eq!(
            after.len(),
            before,
            "the live query must be ended after a late reply; datastore lists {after:?}"
        );
        let logs = logged(&sink);
        assert!(
            !logs.contains(DROPPED_WARNING),
            "a finisher that finished must log no drop; got {logs:?}"
        );
    }

    /// Run one drop of an unfinished [`finish_subscribe`] for `frank` under a
    /// capturing subscriber and return the logs. `drive` gets the future.
    fn logs_of_finisher_drop(
        subscribe: impl Future<Output = Subscribed> + Send + 'static,
        drive: impl FnOnce(tokio::runtime::Runtime, futures::future::BoxFuture<'static, ()>),
    ) -> String {
        let (subscriber, sink) = capturing_subscriber();
        let _guard = tracing::subscriber::set_default(subscriber);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let db: &'static Surreal<any::Any> = Box::leak(Box::new(Surreal::init()));
        let finisher = finish_subscribe(db, "frank".to_string(), subscribe);
        drive(runtime, finisher.boxed());
        logged(&sink)
    }

    /// A [`finish_subscribe`] dropped before it finishes logs at warn with the
    /// agent, for each way it can be dropped: an abort, a panic in the
    /// subscribe it awaits, and the exit of its runtime.
    #[test]
    fn finish_subscribe_logs_when_dropped_before_it_finishes() {
        let abort = logs_of_finisher_drop(std::future::pending(), |runtime, finisher| {
            runtime.block_on(async {
                let handle = tokio::spawn(finisher);
                tokio::task::yield_now().await;
                handle.abort();
                let joined = handle.await;
                assert!(
                    joined.as_ref().is_err_and(|e| e.is_cancelled()),
                    "precondition: the finisher must be aborted, got {joined:?}"
                );
            });
        });
        let panic = logs_of_finisher_drop(async { panic!("subscribe panicked") }, |runtime, f| {
            runtime.block_on(async {
                let joined = tokio::spawn(f).await;
                assert!(
                    joined.as_ref().is_err_and(|e| e.is_panic()),
                    "precondition: the finisher must panic, got {joined:?}"
                );
            });
        });
        let exit = logs_of_finisher_drop(std::future::pending(), |runtime, finisher| {
            runtime.spawn(finisher);
            runtime.block_on(tokio::task::yield_now());
            drop(runtime);
        });
        let silent: Vec<&str> = [("abort", abort), ("panic", panic), ("runtime exit", exit)]
            .iter()
            .filter(|(_, logs)| {
                !logs.lines().any(|l| {
                    l.contains("WARN") && l.contains("frank") && l.contains(DROPPED_WARNING)
                })
            })
            .map(|(cause, _)| *cause)
            .collect();
        assert_eq!(silent, Vec::<&str>::new(), "drops with no warn line");
    }

    /// Stall every later query on `db` for 3 s: a `SLEEP` holds the session's
    /// read lock, a queued `use_db` waits for its write lock, and a later
    /// query waits behind that writer. The returned task ends with the stall.
    async fn hold_session(db: &'static Surreal<any::Any>) -> tokio::task::JoinHandle<()> {
        let held = async move {
            let (slept, used) = tokio::join!(db.query("SLEEP 3s"), async {
                tokio::task::yield_now().await;
                db.use_db("test").await
            });
            slept.and_then(|r| r.check()).expect("sleep query");
            used.expect("use_db");
        };
        let task = tokio::spawn(held);
        sleep(Duration::from_millis(100)).await;
        task
    }

    /// A [`finish_subscribe`] aborted while its `KILL` waits logs at warn with
    /// the agent, and the live query stays registered.
    #[tokio::test(flavor = "current_thread")]
    async fn finish_subscribe_logs_a_drop_during_its_kill() {
        let db: &'static Surreal<any::Any> = Box::leak(Box::new(mem_db().await));
        let owner = RecordId::new(AGENT_TABLE, "frank");
        let (stream, live_id) = subscribe_wake(db, &owner, "frank")
            .await
            .expect("subscribe");
        let held = hold_session(db).await;

        let (subscriber, sink) = capturing_subscriber();
        let guard = tracing::subscriber::set_default(subscriber);
        let finisher = tokio::spawn(finish_subscribe(db, "frank".to_string(), async move {
            Ok((stream, live_id))
        }));
        sleep(Duration::from_millis(100)).await;
        finisher.abort();
        let joined = finisher.await;
        drop(guard);

        assert!(
            joined.as_ref().is_err_and(|e| e.is_cancelled()),
            "precondition: the finisher must be aborted during its KILL, got {joined:?}"
        );
        let logs = logged(&sink);
        assert!(
            logs.lines()
                .any(|l| l.contains("WARN") && l.contains("frank") && l.contains(DROPPED_WARNING)),
            "no drop warn line for an abort during the KILL; got {logs:?}"
        );
        held.await.expect("join");
        let lives = message_lives(db).await;
        assert_eq!(
            lives,
            vec![live_id.to_string()],
            "an aborted KILL leaves the live query registered"
        );
    }

    /// A `KILL` that stalls past [`LIVE_KILL_TIMEOUT`] (see [`hold_session`]):
    /// [`finish_subscribe`] logs it at warn with the agent and the live id,
    /// and the live query is still registered.
    #[tokio::test(flavor = "current_thread")]
    async fn finish_subscribe_logs_a_timed_out_kill() {
        let db: &'static Surreal<any::Any> = Box::leak(Box::new(mem_db().await));
        let owner = RecordId::new(AGENT_TABLE, "frank");
        let (stream, live_id) = subscribe_wake(db, &owner, "frank")
            .await
            .expect("subscribe");
        let held = hold_session(db).await;

        let (subscriber, sink) = capturing_subscriber();
        let guard = tracing::subscriber::set_default(subscriber);
        let began = tokio::time::Instant::now();
        finish_subscribe(db, "frank".to_string(), async { Ok((stream, live_id)) }).await;
        let elapsed = began.elapsed();
        drop(guard);

        assert!(
            elapsed >= LIVE_KILL_TIMEOUT,
            "precondition: the KILL must stall; finish_subscribe took {elapsed:?}"
        );
        let logs = logged(&sink);
        assert!(
            logs.lines().any(|l| l.contains("WARN")
                && l.contains("frank")
                && l.contains(&live_id.to_string())
                && l.contains("did not finish within")),
            "no warn line for the timed-out KILL; got {logs:?}"
        );
        held.await.expect("join");
        let lives = message_lives(db).await;
        assert_eq!(
            lives,
            vec![live_id.to_string()],
            "a timed-out KILL leaves the live query registered"
        );
    }

    /// [`finish_subscribe`] logs a failed subscribe at warn with the agent and
    /// the error chain, and a failed `KILL` of the returned live query at warn
    /// with the agent and the error chain, through a client with no
    /// connection, on which the `KILL` fails. Neither logs a late reply or a
    /// drop.
    #[tokio::test(flavor = "current_thread")]
    async fn finish_subscribe_logs_a_failed_subscribe_and_a_failed_kill() {
        let db = mem_db().await;
        let unconnected = Surreal::<any::Any>::init();
        let owner = RecordId::new(AGENT_TABLE, "frank");
        let (stream, live_id) = subscribe_wake(&db, &owner, "frank")
            .await
            .expect("subscribe");
        let failed: Subscribed = Err(Error::LiveQuery {
            agent: "frank".to_string(),
            source: Box::new(surrealdb::Error::internal("reply refused".to_string())),
        });

        let (subscriber, sink) = capturing_subscriber();
        let guard = tracing::subscriber::set_default(subscriber);
        finish_subscribe(&db, "frank".to_string(), async { failed }).await;
        finish_subscribe(&unconnected, "frank".to_string(), async {
            Ok((stream, live_id))
        })
        .await;
        drop(guard);

        let logs = logged(&sink);
        let warns: Vec<&str> = logs.lines().filter(|l| l.contains("WARN")).collect();
        assert!(
            warns
                .iter()
                .any(|l| l.contains("frank") && l.contains("reply refused")),
            "no warn line with the agent and the subscribe's error chain; got {logs:?}"
        );
        assert!(
            warns.iter().any(|l| l.contains("frank")
                && l.contains("failed to KILL live query")
                && l.contains(&live_id.to_string())),
            "no warn line for the failed KILL; got {logs:?}"
        );
        assert!(
            !logs.contains(NO_REPLY_WARNING) && !logs.contains(DROPPED_WARNING),
            "a finisher with a prompt reply must log no late reply or drop; got {logs:?}"
        );
    }

    /// `outcomes` as a stream of `(index, outcome)` pairs in vector order.
    async fn first_ready_error_of(outcomes: Vec<Result<()>>) -> Result<()> {
        first_ready_error(futures::stream::iter(outcomes.into_iter().enumerate())).await
    }

    /// [`first_ready_error`] returns a listen loop's sent error over an
    /// earlier `ReadyTimeout` or `ListenLoopDropped`, falls back to the one
    /// with the lowest index when no loop sent one, and returns `Ok(())` when
    /// every loop signalled ready.
    #[tokio::test]
    async fn first_ready_error_prefers_a_sent_error() {
        let timeout_err = || Error::ReadyTimeout {
            agent: "ann".to_string(),
            timeout: Duration::from_secs(10),
        };
        let dropped = || Error::ListenLoopDropped {
            agent: "cy".to_string(),
        };
        let sent = || id_missing("bo");

        let picked = first_ready_error_of(vec![
            Err(timeout_err()),
            Ok(()),
            Err(sent()),
            Err(dropped()),
        ])
        .await;
        assert!(
            matches!(&picked, Err(Error::LiveQueryIdMissing { agent, .. }) if agent == "bo"),
            "a sent error must win over an earlier timeout, got {picked:?}"
        );
        let picked = first_ready_error_of(vec![Ok(()), Err(dropped()), Err(timeout_err())]).await;
        assert!(
            matches!(&picked, Err(Error::ListenLoopDropped { agent }) if agent == "cy"),
            "with no sent error the first fallback must be returned, got {picked:?}"
        );
        let picked = first_ready_error(futures::stream::iter(vec![
            (1, Err(dropped())),
            (0, Err(timeout_err())),
        ]))
        .await;
        assert!(
            matches!(&picked, Err(Error::ReadyTimeout { agent, .. }) if agent == "ann"),
            "the fallback with the lowest index must be returned, got {picked:?}"
        );
        let picked = first_ready_error_of(vec![Ok(()), Ok(())]).await;
        assert!(picked.is_ok(), "all ready must be Ok, got {picked:?}");
    }

    /// [`cancel_override`] under a cancelled token turns `Ok(())` and
    /// `ReadyTimeout` into `Cancelled { agent: None }` and keeps
    /// `ListenLoopDropped` and a sent error; under a live token it keeps every
    /// outcome.
    #[test]
    fn cancel_override_relabels_only_ok_and_ready_timeout() {
        let timeout_err = || {
            Err(Error::ReadyTimeout {
                agent: "ann".to_string(),
                timeout: Duration::from_secs(10),
            })
        };
        let dropped = || {
            Err(Error::ListenLoopDropped {
                agent: "cy".to_string(),
            })
        };
        let sent = || Err(id_missing("bo"));

        let picked = cancel_override(Ok(()), true);
        assert!(
            matches!(picked, Err(Error::Cancelled { agent: None })),
            "Ok under a cancelled token must become Cancelled, got {picked:?}"
        );
        let picked = cancel_override(timeout_err(), true);
        assert!(
            matches!(picked, Err(Error::Cancelled { agent: None })),
            "ReadyTimeout under a cancelled token must become Cancelled, got {picked:?}"
        );
        let picked = cancel_override(dropped(), true);
        assert!(
            matches!(&picked, Err(Error::ListenLoopDropped { agent }) if agent == "cy"),
            "ListenLoopDropped under a cancelled token must be kept, got {picked:?}"
        );
        let picked = cancel_override(sent(), true);
        assert!(
            matches!(&picked, Err(Error::LiveQueryIdMissing { agent, .. }) if agent == "bo"),
            "a sent error under a cancelled token must be kept, got {picked:?}"
        );
        let picked = cancel_override(Ok(()), false);
        assert!(
            picked.is_ok(),
            "Ok under a live token must be kept, got {picked:?}"
        );
        let picked = cancel_override(timeout_err(), false);
        assert!(
            matches!(&picked, Err(Error::ReadyTimeout { agent, .. }) if agent == "ann"),
            "ReadyTimeout under a live token must be kept, got {picked:?}"
        );
    }

    /// [`first_ready_error`] over [`await_ready`] outcomes returns a sent
    /// error while another loop has not signalled and its 30 s ready timeout
    /// has not elapsed.
    #[tokio::test]
    async fn first_ready_error_returns_a_sent_error_without_waiting() {
        let (_silent_tx, silent_rx) = oneshot::channel();
        let (sent_tx, sent_rx) = oneshot::channel();
        sent_tx
            .send(Err(id_missing("bo")))
            .expect("send startup error");
        let outcomes: FuturesUnordered<_> = [("ann", silent_rx), ("bo", sent_rx)]
            .into_iter()
            .enumerate()
            .map(|(index, (name, rx))| async move {
                (
                    index,
                    await_ready(name.to_string(), rx, Duration::from_secs(30)).await,
                )
            })
            .collect();
        let picked = timeout(Duration::from_secs(1), first_ready_error(outcomes)).await;
        assert!(
            matches!(&picked, Ok(Err(Error::LiveQueryIdMissing { agent, .. })) if agent == "bo"),
            "a sent error must be returned within 1 s while 'ann' is silent, got {picked:?}"
        );
    }

    /// [`first_ready_error`] returns [`Error::Cancelled`] over an earlier
    /// `ReadyTimeout` and an earlier `ListenLoopDropped`, and the first sent
    /// error when a `Cancelled` and another sent error are both present.
    #[tokio::test]
    async fn first_ready_error_prefers_cancelled_over_timeout_and_dropped() {
        let cancelled = || Error::Cancelled {
            agent: Some("di".to_string()),
        };
        let picked = first_ready_error_of(vec![
            Err(Error::ReadyTimeout {
                agent: "ann".to_string(),
                timeout: Duration::from_secs(10),
            }),
            Err(Error::ListenLoopDropped {
                agent: "cy".to_string(),
            }),
            Err(cancelled()),
        ])
        .await;
        assert!(
            matches!(&picked, Err(Error::Cancelled { agent: Some(agent) }) if agent == "di"),
            "Cancelled must win over an earlier ReadyTimeout and ListenLoopDropped, got {picked:?}"
        );
        let picked = first_ready_error_of(vec![Err(id_missing("bo")), Err(cancelled())]).await;
        assert!(
            matches!(&picked, Err(Error::LiveQueryIdMissing { agent, .. }) if agent == "bo"),
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

        let (subscriber, sink) = capturing_subscriber();
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

        let logs = logged(&sink);
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

    /// [`kill_live`]'s warn line for [`Error::LiveKill`] carries the inner
    /// surrealdb error text, not only the variant's own message.
    #[test]
    fn kill_live_warning_includes_the_kill_error_source() {
        let inner = "kill refused by datastore";
        let err = Error::LiveKill {
            agent: "frank".to_string(),
            live_id: Uuid::new_v4(),
            source: Box::new(surrealdb::Error::internal(inner.to_string())),
        };

        let (subscriber, sink) = capturing_subscriber();
        tracing::subscriber::with_default(subscriber, || warn_kill_live(&err));

        let logs = logged(&sink);
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
        tx.send(Err(id_missing("frank")))
            .expect("send startup error");
        let err = await_ready("frank".to_string(), rx, Duration::from_secs(30))
            .await
            .expect_err("a sent startup error must be returned");
        assert!(
            matches!(err, Error::LiveQueryIdMissing { ref agent, .. } if agent == "frank"),
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
