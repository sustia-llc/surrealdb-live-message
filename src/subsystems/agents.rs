use std::{collections::HashMap, marker::PhantomData, ops::ControlFlow, sync::Arc};

use async_channel::{Receiver, Sender};
use futures::stream::{FuturesUnordered, Stream};
use futures::{FutureExt, StreamExt};
use serde::{Deserialize, Serialize};
use surrealdb::engine::any;
use surrealdb::method::QueryStream;
use surrealdb::{Notification, Surreal};
use surrealdb_types::{Datetime, Number, Object, RecordId, SurrealValue, Uuid, Value};
use tokio::sync::{RwLock, oneshot};
use tokio::time::{Duration, sleep, timeout};
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use tracing::Instrument;

use crate::error::{Error, Result, chain};
use crate::message::{MESSAGE_TABLE, Message};
use crate::settings::SETTINGS;
use crate::subsystems::sdb;

pub const AGENT_TABLE: &str = "agent";

/// Maximum time `Coalition::new` waits for each database call of `Agent::new`,
/// and for each `listen_loop` to send its ready signal.
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

/// Maximum time [`save_cursor_bounded`] waits for a cursor save.
const CURSOR_SAVE_TIMEOUT: Duration = Duration::from_secs(2);

/// Smallest `Float` size, 2^53, that [`integral`] does not read as an integer.
const FLOAT_INTEGER_BOUND: f64 = 9_007_199_254_740_992.0;

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
    /// agent whose record already existed, at versionstamp 0, with no changefeed
    /// read if the existence check finds it, so the next catch-up reads the whole retained changefeed, and
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
    /// `out` pointer. Returns [`Error::Send`] when the recipient lookup or the
    /// `RELATE` fails, as a call or as a statement.
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
            .and_then(|r| r.check())
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
    /// cursor stays at its previous saved position. A save that does not
    /// finish within [`CURSOR_SAVE_TIMEOUT`] stops waiting. On a complete page
    /// the in-memory cursor still moves. A later page's save, a retry on an
    /// empty page, or one save at the loop's stop writes it. A save never
    /// moves the row back. A full page that leaves the cursor in place
    /// fails the pass with [`Error::CatchUp`], which reconnects as below. A
    /// message not yet on the
    /// bus is put on it after a disconnect/restart. Each `update` op of a
    /// message record addressed to this agent is one delivery, so a message
    /// record that is updated after it is created is put on the bus again.
    /// For message records that are only created: after a stop whose cursor
    /// save succeeds, with no earlier save failed or timed out, no message is
    /// put on the bus twice; after a process exit mid-page, a failed save, or a timed-out
    /// save, the messages sent since the position in the `cursor` row are put
    /// on the bus again at the next start. Deliveries already on the bus are
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
        let cursor = match ctx
            .token
            .run_until_cancelled(initial_cursor(db, name))
            .await
        {
            Some(Ok(c)) => c,
            Some(Err(e)) => return fail_startup(name, ready_tx, e),
            None => return cancel_startup(name, ready_tx),
        };

        Self::listen_on(&ctx, cursor, ready_tx, start, |at| fetch_page(&ctx, at)).await;
    }

    /// The part of [`Agent::listen_loop`] after the cursor load, from `cursor`,
    /// with each catch-up page read through `read_page`.
    async fn listen_on<T, F, Fut>(
        ctx: &LoopCtx<'_, T>,
        cursor: Cursor,
        ready_tx: oneshot::Sender<Result<()>>,
        start: StartGate,
        mut read_page: F,
    ) where
        T: SurrealValue + Send + Sync + Unpin + 'static,
        F: FnMut(Cursor) -> Fut,
        Fut: Future<Output = Result<ControlFlow<Stop, Vec<Value>>>>,
    {
        let name = ctx.agent;
        let mut ready_tx = Some(ready_tx);
        let mut backoff = RECONNECT_BACKOFF_START;
        let mut progress = LoopProgress::at(cursor);

        loop {
            // (Re)subscribe the LIVE wake-up.
            let Some(subscribed) = subscribe_unless_cancelled(ctx, &mut ready_tx).await else {
                return stop_save(ctx, &progress).await;
            };
            let (stream, live_id) = match subscribed {
                Ok(s) => s,
                Err(e) => {
                    if let Some(tx) = ready_tx.take() {
                        return fail_startup(name, tx, e);
                    }
                    tracing::error!("resubscribe failed for {name}: {}", chain(&e));
                    if cancel_or_sleep(&ctx.token, backoff).await {
                        return stop_save(ctx, &progress).await;
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
                            let fetched = read_page(progress.cursor).await;
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
                                retry_save(ctx, &mut progress).await;
                                return Ok(ControlFlow::Continue(()));
                            }
                            let full = page.len() >= CATCHUP_BATCH;
                            let page_start = progress.cursor;
                            if let ControlFlow::Break(stop) =
                                deliver_page(ctx, &mut progress, &mut wakes, page).await?
                            {
                                return Ok(ControlFlow::Break(stop));
                            }
                            if !full {
                                return Ok(ControlFlow::Continue(()));
                            }
                            if progress.cursor == page_start {
                                return Err(Error::CatchUp {
                                    agent: name.to_string(),
                                    source: reply_error(format!(
                                        "a full page left the cursor at {page_start}"
                                    )),
                                });
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
            if matches!(exit, Exit::Stop(_)) {
                stop_save(ctx, &progress).await;
            }
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
                return stop_save(ctx, &progress).await;
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
    /// is bounded by `READY_TIMEOUT`, which returns
    /// [`Error::AgentCreateTimeout`] for that agent, and raced against the
    /// coalition's token. The changefeed is read at most once, for the first
    /// new agent, and that result starts each later new agent. After the last
    /// `Agent::new` call, waits for every `listen_loop` to signal that its
    /// LIVE query is registered and its first catch-up page query succeeded.
    /// Each wait is bounded by the ready timeout. Each loop holds at its ready
    /// point, and puts nothing on the bus, until every loop has signalled
    /// ready; this then sends the start signal and returns `Ok`. On any
    /// failure it cancels every spawned loop and waits for each to end before
    /// it returns the error. The first error a loop sent that these waits see
    /// (including [`Error::Cancelled`]) ends the waits and is returned after
    /// that teardown, in preference to [`Error::ReadyTimeout`] and
    /// [`Error::ListenLoopDropped`]; among those two, the one for the earliest
    /// name in `names` is returned.
    ///
    /// For names that pass validation, a token cancelled before every loop is
    /// ready returns, unless the waits see another sent error or
    /// `ListenLoopDropped`, [`Error::Cancelled`]: with `agent: None` when the
    /// token is cancelled before any database call or when it is found
    /// cancelled after the readiness waits end in `Ok` or `ReadyTimeout`; with `agent: Some(name)` when the cancel
    /// interrupts that agent's `Agent::new` call or its loop's startup.
    /// Dropping the returned future before it completes cancels the token,
    /// which ends every spawned loop and the retention sweep; the drop does
    /// not wait for them.
    pub async fn new(names: Vec<String>) -> Result<Self> {
        Self::new_with_ready_timeout(names, READY_TIMEOUT).await
    }

    /// Like [`Coalition::new`], with `ready_timeout` in place of
    /// `READY_TIMEOUT` for each database call of `Agent::new` and each
    /// readiness wait.
    pub async fn new_with_ready_timeout(
        names: Vec<String>,
        ready_timeout: Duration,
    ) -> Result<Self> {
        Self::build(names, ready_timeout, CancellationToken::new()).await
    }

    /// Like [`Coalition::new`], with a root token that is a child of
    /// `parent`: cancelling `parent` cancels the coalition. With no repeated
    /// name, a `parent` that is already cancelled returns [`Error::Cancelled`]
    /// with `agent: None` before any database call.
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
    /// retention sweep have ended. After the cancel, a loop makes at most one
    /// cursor save, which waits up to 2 s, and waits up to 2 s for its `KILL`.
    /// A subscribe in flight at the
    /// cancel is finished by a task that this does not wait for, so its `KILL`
    /// can run after this returns.
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

/// The `versionstamp` field of `o`, if [`integral`] reads it as an integer.
fn object_versionstamp(o: &Object) -> Option<i64> {
    match o.get("versionstamp") {
        Some(Value::Number(n)) => integral(n),
        _ => None,
    }
}

/// The value of `n` if `n` is an integer: an `Int`, a `Float` with no fraction
/// below [`FLOAT_INTEGER_BOUND`] in size, or a `Decimal` with no fraction that
/// fits `i64`. Otherwise `None`.
fn integral(n: &Number) -> Option<i64> {
    let exact = match n {
        Number::Int(_) => true,
        Number::Float(v) => v.fract() == 0.0 && v.abs() < FLOAT_INTEGER_BOUND,
        Number::Decimal(v) => v.is_integer(),
    };
    exact.then(|| n.to_int()).flatten()
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

/// Parse a `cursor` row: an object with a `versionstamp` number and an optional
/// `change_index` number (absent or `NONE` → 0), each a non-negative integer
/// that [`integral`] reads, with `change_index` fitting `usize`. Any other
/// shape, `change_index: NULL` among them, returns [`Error::CursorLoad`] for
/// `agent`.
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
        Some(Value::Number(n)) => non_negative(integral(n))?,
        _ => return Err(malformed()),
    };
    let change_index = match o.get("change_index") {
        None | Some(Value::None) => 0,
        Some(Value::Number(n)) => integral(n)
            .and_then(|i| position_within(i, u64::try_from(usize::MAX).unwrap_or(u64::MAX)))
            .ok_or_else(malformed)?,
        Some(_) => return Err(malformed()),
    };
    Ok(Cursor {
        versionstamp,
        change_index,
    })
}

/// Persist `agent`'s cursor. The statement creates an absent row and writes a
/// row that holds a lower position. It leaves a row at an equal or higher
/// position, and that is not a failure. A failed call or statement returns
/// [`Error::CursorSave`].
async fn save_cursor(db: &Surreal<any::Any>, agent: &str, cursor: Cursor) -> Result<()> {
    db.query(
        "UPSERT $row CONTENT $cursor WHERE versionstamp < $cursor.versionstamp \
         OR (versionstamp = $cursor.versionstamp AND change_index < $cursor.change_index)",
    )
    .bind(("row", RecordId::new(CURSOR_TABLE, agent)))
    .bind(("cursor", cursor))
    .await
    .and_then(|r| r.check())
    .map_err(|source| Error::CursorSave {
        agent: agent.to_string(),
        source: Box::new(source),
    })?;
    Ok(())
}

/// Why a cursor save did not finish: the call failed, or it passed
/// [`CURSOR_SAVE_TIMEOUT`].
#[derive(Debug, thiserror::Error)]
enum SaveFailure {
    #[error("{}", chain(.0))]
    Call(Error),
    #[error("did not finish within {:?}", CURSOR_SAVE_TIMEOUT)]
    TimedOut,
}

/// [`save_cursor`], or [`SaveFailure::TimedOut`] when it does not finish within
/// [`CURSOR_SAVE_TIMEOUT`].
async fn save_cursor_bounded(
    db: &Surreal<any::Any>,
    agent: &str,
    cursor: Cursor,
) -> std::result::Result<(), SaveFailure> {
    match timeout(CURSOR_SAVE_TIMEOUT, save_cursor(db, agent, cursor)).await {
        Ok(saved) => saved.map_err(SaveFailure::Call),
        Err(_elapsed) => Err(SaveFailure::TimedOut),
    }
}

/// The catch-up state of one listen loop: the in-memory `cursor`, the last
/// position known `saved`, whether the one save after a stop was tried, and the
/// start cursor and count of the last page that logged changesets with no
/// position.
struct LoopProgress {
    cursor: Cursor,
    saved: Cursor,
    stop_save_tried: bool,
    logged_at: Option<(Cursor, usize)>,
}

impl LoopProgress {
    /// The state of a loop that starts from the saved position `cursor`.
    fn at(cursor: Cursor) -> Self {
        Self {
            cursor,
            saved: cursor,
            stop_save_tried: false,
            logged_at: None,
        }
    }

    /// `true` when the cursor is past the known-saved position and the one save
    /// after a stop was not tried.
    fn save_pending(&self) -> bool {
        self.cursor > self.saved && !self.stop_save_tried
    }
}

/// Save `progress.cursor` once, within [`CURSOR_SAVE_TIMEOUT`], if a save is
/// pending. A success updates `progress.saved`. A failure or a timeout is logged
/// at error level with the position, and the caller continues. A retry that ends
/// with `ctx.token` cancelled counts as the one save after a stop.
async fn retry_save<T: SurrealValue>(ctx: &LoopCtx<'_, T>, progress: &mut LoopProgress) {
    if !progress.save_pending() {
        return;
    }
    let (agent, cursor) = (ctx.agent, progress.cursor);
    let saved = save_cursor_bounded(ctx.db, agent, cursor).await;
    progress.stop_save_tried |= ctx.token.is_cancelled();
    match saved {
        Ok(()) => progress.saved = cursor,
        Err(failure) => {
            tracing::error!("catch_up for {agent}: retry of cursor {cursor} failed: {failure}");
        }
    }
}

/// Save `progress.cursor` once, within [`CURSOR_SAVE_TIMEOUT`], if it is past
/// `progress.saved` and no save was tried at this stop. A failure or a timeout
/// is logged at error level.
async fn stop_save<T: SurrealValue>(ctx: &LoopCtx<'_, T>, progress: &LoopProgress) {
    if !progress.save_pending() {
        return;
    }
    let (agent, cursor) = (ctx.agent, progress.cursor);
    match save_cursor_bounded(ctx.db, agent, cursor).await {
        Ok(()) => tracing::info!("listen_loop for {agent} saved cursor {cursor} at its stop"),
        Err(failure) => tracing::error!(
            "listen_loop for {agent} could not save cursor {cursor} at its stop: {failure}"
        ),
    }
}

/// The count of one page's changesets with no position, and the first of them.
struct NoPosition<V> {
    count: usize,
    first: Option<V>,
}

impl<V: std::fmt::Debug> NoPosition<V> {
    /// A count of zero.
    fn new() -> Self {
        Self {
            count: 0,
            first: None,
        }
    }

    /// Count `changeset`, and keep it if it is the first.
    fn add(&mut self, changeset: V) {
        self.count += 1;
        self.first.get_or_insert(changeset);
    }

    /// Log the count and the first changeset as one error line, if the count
    /// is above zero.
    fn log(&self, path: &str, agent: &str) {
        if let Some(first) = &self.first {
            tracing::error!(
                "{path} for {agent}: changesets with no position skipped: {}; the first: {first:?}",
                self.count
            );
        }
    }
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
/// A changeset that is not an object or has no integer versionstamp is skipped.
/// The page's skipped changesets give one error line, with their count and the
/// first of them. [`Error::AgentCreate`] for `agent` when `reply`
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
    let mut no_position = NoPosition::new();
    for changeset in changesets.iter() {
        match versionstamp_of(changeset) {
            Some(vs) => highest = highest.max(Some(vs)),
            None => no_position.add(changeset),
        }
    }
    no_position.log("snapshot", agent);
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

/// Deliver one catch-up page (`page`, fetched from `progress.cursor`) onto the bus,
/// in log order, and persist the cursor.
///
/// Cursor rule: the cursor moves past a changeset once every change in it
/// has been handled, to the start of the next versionstamp; inside the
/// changeset at `cursor.versionstamp`, changes before `cursor.change_index`
/// are not read again. A change is handled when it is a `delete` or
/// `define_table` op, when its `update` record's `out` is another agent (`out`
/// is compared before any decode), or when its delivery is on the bus. A
/// changeset that is not an object, has no integer versionstamp, or has
/// versionstamp `i64::MAX` has no position of its own: it is skipped with no
/// delivery, and the cursor passes it only by moving to a later changeset's
/// position.
///
/// Malformed-data rule: a `changes` field that is missing or not an array, a
/// change that is not an object or has no `update`, `delete` or `define_table`
/// key, a record that is not an object, a record whose `out` is missing or not
/// a record id, and a message for `ctx.owner` that does not decode as
/// `Message<T>` are each logged at error level (agent, versionstamp, what was
/// malformed) and skipped.
///
/// A page's changesets with no position give one error line, with their count
/// and the first of them. `progress.logged_at` holds the start cursor and the
/// count of the last page that logged this line. A page with the same start
/// cursor and count does not log it.
///
/// Each send is raced against `ctx.token`, and `wakes` is read while a send
/// waits on a full bus. Cancellation, or a closed bus, returns
/// `ControlFlow::Break` with the cursor saved at the change not put on the
/// bus, so the next pass starts at that change; if that save fails or takes
/// longer than [`CURSOR_SAVE_TIMEOUT`], the error is logged and
/// `ControlFlow::Break` is still returned. A failed save call on a complete
/// page returns that error, and `progress.cursor` stays. A save on a complete
/// page that takes longer than [`CURSOR_SAVE_TIMEOUT`] is logged at error level
/// with the position, `progress.cursor` moves to that position, and the pass
/// continues. The cursor is saved when it is past `progress.saved`.
async fn deliver_page<T>(
    ctx: &LoopCtx<'_, T>,
    progress: &mut LoopProgress,
    wakes: &mut Wakes,
    page: Vec<Value>,
) -> Result<ControlFlow<Stop>>
where
    T: SurrealValue + Send + Sync + Unpin + 'static,
{
    let agent = ctx.agent;
    let page_start = progress.cursor;
    let mut committed = progress.cursor;
    let mut stopped = None;
    let mut no_position = NoPosition::new();
    'page: for mut changeset in page {
        let Value::Object(obj) = &mut changeset else {
            no_position.add(changeset);
            continue;
        };
        let Some(vs) = object_versionstamp(obj).filter(|vs| *vs < i64::MAX) else {
            no_position.add(changeset);
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

    let log_key = (page_start, no_position.count);
    if no_position.count > 0 && progress.logged_at != Some(log_key) {
        no_position.log("catch_up", agent);
        progress.logged_at = Some(log_key);
    }

    if committed > progress.saved {
        let saved = save_cursor_bounded(ctx.db, agent, committed).await;
        progress.stop_save_tried |= stopped.is_some() || ctx.token.is_cancelled();
        match (saved, stopped) {
            (Ok(()), _) => {
                progress.cursor = committed;
                progress.saved = committed;
            }
            (Err(failure), Some(stop)) => {
                tracing::error!(
                    "catch_up for {agent} could not save cursor {committed} on {stop:?}: {failure}"
                );
                return Ok(ControlFlow::Break(stop));
            }
            (Err(SaveFailure::Call(e)), None) => return Err(e),
            (Err(failure @ SaveFailure::TimedOut), None) => {
                tracing::error!(
                    "catch_up for {agent}: saving cursor {committed} {failure}; the pass continues"
                );
                progress.cursor = committed;
            }
        }
    }
    if let Some(stop) = stopped {
        tracing::info!(
            "catch_up for {agent} stopped at cursor {}: {stop:?}",
            progress.cursor
        );
        return Ok(ControlFlow::Break(stop));
    }
    Ok(ControlFlow::Continue(()))
}

/// Periodic age-out sweep: delete message rows older than the retention window.
/// One task per coalition (table-wide); the durable log is otherwise never
/// pruned. Runs every `retention/4` (min 60s) until cancelled. See [`sweep_on`].
async fn retention_sweep(token: CancellationToken, retention: Duration) {
    let interval = (retention / 4).max(Duration::from_secs(60));
    sweep_on(
        token,
        retention,
        interval,
        sdb::SurrealDBWrapper::connection(),
    )
    .await;
}

/// Await `connect`, then delete the message rows older than `retention` every
/// `interval`, until `token` is cancelled. Each of those awaits is raced against
/// `token`. A failed `DELETE` is logged at warn.
async fn sweep_on<C>(token: CancellationToken, retention: Duration, interval: Duration, connect: C)
where
    C: Future<Output = &'static Surreal<any::Any>>,
{
    let Some(db) = token.run_until_cancelled(connect).await else {
        return;
    };
    let secs = retention.as_secs();
    let q = format!("DELETE {MESSAGE_TABLE} WHERE created < time::now() - {secs}s");
    while !cancel_or_sleep(&token, interval).await {
        let delete = async { db.query(&q).await.and_then(|r| r.check()) };
        match token.run_until_cancelled(delete).await {
            Some(Ok(_)) => {}
            Some(Err(e)) => tracing::warn!("retention sweep failed: {}", chain(&e)),
            None => break,
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

/// The stream in `opened`. When `opened` is an error, sends a `KILL` of live
/// query `live_id` through [`kill_live`], then returns [`Error::Stream`] with
/// that error as its source.
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

/// A future that awaits `subscribe`, in flight when `agent`'s loop was
/// cancelled, then sends a `KILL` of the returned live query with [`end_live`].
/// A failed `subscribe` is logged at warn with the agent and the error chain;
/// a failed `KILL` call is logged by [`kill_live`]. With no reply after
/// [`LIVE_KILL_TIMEOUT`], the future logs a warn with the agent and keeps
/// waiting, with no bound. If the future is dropped before it finishes, polled
/// or not, [`UnfinishedSubscribe`] logs a warn with the agent.
fn finish_subscribe<F>(
    db: &Surreal<any::Any>,
    agent: String,
    subscribe: F,
) -> impl Future<Output = ()>
where
    F: Future<Output = Subscribed>,
{
    let unfinished = UnfinishedSubscribe { agent, armed: true };
    async move {
        let agent = unfinished.agent.as_str();
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
            Ok((stream, live_id)) => end_live(db, agent, live_id, stream).await,
            Err(e) => tracing::warn!("subscribe for {agent} failed after a cancel: {}", chain(&e)),
        }
        unfinished.disarm();
    }
}

/// Logs a warn for `agent` on drop, unless [`UnfinishedSubscribe::disarm`]
/// consumed it.
struct UnfinishedSubscribe {
    agent: String,
    armed: bool,
}

impl UnfinishedSubscribe {
    /// Drop this with no log.
    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for UnfinishedSubscribe {
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

/// Send a `KILL` of live query `live_id` through [`kill_live`], then drop
/// `stream`.
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

    /// [`page_highest`] skips a changeset stamped 7.5: the page has no highest
    /// versionstamp.
    #[test]
    fn page_highest_skips_a_fractional_stamp() {
        let fractional = object(vec![("versionstamp", Value::Number(Number::Float(7.5)))]);
        let page = page_highest("frank", 0, &reply_of(vec![fractional])).expect("short page");
        assert_eq!(
            page.highest, None,
            "a stamp of 7.5 read as {:?}",
            page.highest
        );
    }

    /// [`page_highest`] logs one error line per page for its changesets with
    /// no integer versionstamp, with their count and the first of them, and
    /// skips them. With no integer versionstamp in a full page it returns
    /// [`Error::AgentCreate`]; in a short page it returns no position.
    #[test]
    fn page_highest_skips_malformed_changesets_and_rejects_a_full_malformed_page() {
        let no_stamp = object(vec![("changes", Value::Array(Vec::<Value>::new().into()))]);
        let first_bad = object(vec![("marker", "first-bad".to_string().into_value())]);
        let text_stamp = object(vec![("versionstamp", "7".to_string().into_value())]);
        let mixed = reply_of(vec![
            first_bad,
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
        let errors: Vec<&str> = logs.lines().filter(|l| l.contains("ERROR")).collect();
        assert_eq!(
            errors.len(),
            3,
            "expected one error line per page; got {} lines",
            errors.len()
        );
        assert!(
            errors[0].contains("skipped: 3;") && errors[0].contains("first-bad"),
            "the mixed page's line must hold the count and the first changeset; got {}",
            errors[0]
        );
        assert!(
            !errors[0].contains("\"7\""),
            "the mixed page's line must hold only the first changeset; got {}",
            errors[0]
        );
        let full_count = format!("skipped: {CATCHUP_BATCH};");
        assert!(
            errors[2].contains(&full_count),
            "the full page's line must hold its count; got {}",
            errors[2]
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

    /// A [`finish_subscribe`] future dropped with no poll logs at warn with the
    /// agent, when dropped directly and when queued on a runtime that exits.
    #[test]
    fn finish_subscribe_logs_when_dropped_with_no_poll() {
        let direct = logs_of_finisher_drop(std::future::pending(), |_runtime, finisher| {
            drop(finisher);
        });
        let queued = logs_of_finisher_drop(std::future::pending(), |runtime, finisher| {
            runtime.spawn(finisher);
            drop(runtime);
        });
        let silent: Vec<&str> = [("direct drop", direct), ("queued at exit", queued)]
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

    /// A [`finish_subscribe`] dropped after its first poll logs at warn with
    /// the agent, on an abort, a panic in the subscribe it awaits, and the
    /// exit of its runtime.
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

    /// [`hold_session_for`] 3 s.
    async fn hold_session(db: &'static Surreal<any::Any>) -> tokio::task::JoinHandle<()> {
        hold_session_for(db, 3).await
    }

    /// Stall every later query on `db` for `secs` seconds: a `SLEEP` holds the
    /// session's read lock, a queued `use_db` waits for its write lock, and a
    /// later query waits behind that writer. The returned task ends with the
    /// stall.
    async fn hold_session_for(
        db: &'static Surreal<any::Any>,
        secs: u64,
    ) -> tokio::task::JoinHandle<()> {
        let held = async move {
            let (slept, used) = tokio::join!(db.query(format!("SLEEP {secs}s")), async {
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
        let mut progress = LoopProgress::at(Cursor::at(5));
        let flow = deliver_page(&ctx, &mut progress, &mut wakes, page)
            .await
            .expect("deliver_page");
        drop(guard);

        assert_eq!(flow, ControlFlow::Continue(()), "the page must complete");
        assert_eq!(progress.cursor, Cursor::at(6), "cursor after the page");
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
    /// non-integer or -1 `versionstamp`, and a non-integer or -1
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

    /// `text` as a `Decimal` number.
    fn decimal(text: &str) -> Number {
        Number::from_decimal(text.parse().expect("decimal text"))
    }

    /// [`integral`] reads an `Int`, a finite `Float` with no fraction below
    /// 2^53 in size, and a `Decimal` with no fraction that fits `i64`; it
    /// reads no other tested number.
    #[test]
    fn integral_reads_only_exact_integers() {
        let cases = [
            (Number::Int(7), Some(7)),
            (Number::Int(i64::MAX), Some(i64::MAX)),
            (Number::Float(7.0), Some(7)),
            (Number::Float(-0.0), Some(0)),
            (Number::Float(-7.0), Some(-7)),
            (
                Number::Float(FLOAT_INTEGER_BOUND - 1.0),
                Some((1 << 53) - 1),
            ),
            (
                Number::Float(1.0 - FLOAT_INTEGER_BOUND),
                Some(1 - (1 << 53)),
            ),
            (decimal("7"), Some(7)),
            (decimal("7.00"), Some(7)),
            (Number::Float(7.5), None),
            (Number::Float(-0.5), None),
            (Number::Float(f64::NAN), None),
            (Number::Float(f64::INFINITY), None),
            (Number::Float(f64::NEG_INFINITY), None),
            (Number::Float(1e30), None),
            (Number::Float(FLOAT_INTEGER_BOUND), None),
            (Number::Float(-FLOAT_INTEGER_BOUND), None),
            (decimal("7.5"), None),
            (decimal("-0.5"), None),
            (decimal("10000000000000000000000000000"), None),
        ];
        for (number, want) in cases {
            let read = integral(&number);
            assert_eq!(read, want, "integral({number:?}) read {read:?}");
        }
    }

    /// [`parse_cursor`] reads an integral `Float` or `Decimal` in each field
    /// and takes an absent or NONE `change_index` as 0. It returns
    /// [`Error::CursorLoad`] for 7.5, -0.5, NaN and 1e30 in either field and
    /// for a NULL `change_index`.
    #[test]
    fn parse_cursor_takes_integral_numbers_and_rejects_the_rest() {
        let row = |versionstamp: Value, change_index: Option<Value>| {
            let mut pairs = vec![("versionstamp", versionstamp)];
            pairs.extend(change_index.map(|index| ("change_index", index)));
            object(pairs)
        };
        let float = |v: f64| Value::Number(Number::Float(v));
        let seven = || 7i64.into_value();
        for (read, want) in [
            (row(float(7.0), None), Cursor::at(7)),
            (row(Value::Number(decimal("7.0")), None), Cursor::at(7)),
            (row(seven(), Some(Value::None)), Cursor::at(7)),
            (
                row(seven(), Some(float(2.0))),
                Cursor {
                    versionstamp: 7,
                    change_index: 2,
                },
            ),
            (
                row(seven(), Some(Value::Number(decimal("2.0")))),
                Cursor {
                    versionstamp: 7,
                    change_index: 2,
                },
            ),
        ] {
            let parsed = parse_cursor("frank", &read);
            assert!(
                matches!(&parsed, Ok(cursor) if *cursor == want),
                "{read:?} must read as {want}, got {parsed:?}"
            );
        }

        let mut bad = vec![row(seven(), Some(Value::Null))];
        for number in [7.5, -0.5, f64::NAN, 1e30] {
            bad.push(row(float(number), None));
            bad.push(row(seven(), Some(float(number))));
        }
        for row in bad {
            let parsed = parse_cursor("frank", &row);
            assert!(
                matches!(&parsed, Err(Error::CursorLoad { agent, .. }) if agent == "frank"),
                "{row:?} must return CursorLoad, got {parsed:?}"
            );
        }
    }

    /// An update change: a message to `out` from `grace` with payload `text`.
    fn message_change(out: &RecordId, text: &str) -> Value {
        let record = Message::<String> {
            id: None,
            r#in: Some(RecordId::new(AGENT_TABLE, "grace")),
            out: Some(out.clone()),
            payload: text.to_string(),
            created: None,
        };
        object(vec![("update", record.into_value())])
    }

    /// A changeset with `versionstamp` and `changes`.
    fn stamped(versionstamp: Value, changes: Vec<Value>) -> Value {
        object(vec![
            ("versionstamp", versionstamp),
            ("changes", Value::Array(changes.into_iter().collect())),
        ])
    }

    /// The agent `frank` as a record id.
    fn frank() -> RecordId {
        RecordId::new(AGENT_TABLE, "frank")
    }

    /// A [`LoopCtx`] for `frank` on `db` with its token, a bus of `capacity`
    /// and its receiver, and frank's wake stream.
    async fn page_fixture(
        db: &'static Surreal<any::Any>,
        token: CancellationToken,
        capacity: usize,
    ) -> (LoopCtx<'static, String>, Receiver<Delivery<String>>, Wakes) {
        let (stream, _live_id) = subscribe_wake(db, &frank(), "frank")
            .await
            .expect("subscribe");
        let (inbox_tx, inbox) = async_channel::bounded(capacity);
        let ctx = LoopCtx {
            db,
            owner: frank(),
            agent: "frank",
            inbox_tx,
            token,
        };
        (ctx, inbox, Wakes::new(stream))
    }

    /// Text of the error line for the changesets of a page with no position.
    const NO_POSITION_LOG: &str = "changesets with no position skipped";

    /// The lines of `logs` that hold [`NO_POSITION_LOG`] at error level.
    fn no_position_lines(logs: &str) -> Vec<&str> {
        logs.lines()
            .filter(|l| l.contains("ERROR") && l.contains(NO_POSITION_LOG))
            .collect()
    }

    /// [`deliver_page`] delivers the message of a changeset stamped 7.0 and
    /// moves the cursor to 8. Changesets stamped 7.5, -0.5, NaN and 1e30 are
    /// skipped with no delivery.
    #[tokio::test(flavor = "current_thread")]
    async fn deliver_page_reads_an_integral_float_stamp_and_skips_the_rest() {
        let db: &'static Surreal<any::Any> = Box::leak(Box::new(mem_db().await));
        let (ctx, inbox, mut wakes) = page_fixture(db, CancellationToken::new(), 8).await;
        let float = |v: f64| Value::Number(Number::Float(v));
        let page = [7.0, 7.5, -0.5, f64::NAN, 1e30]
            .map(|stamp| {
                let change = message_change(&frank(), &format!("stamped {stamp}"));
                stamped(float(stamp), vec![change])
            })
            .to_vec();

        let mut progress = LoopProgress::at(Cursor::at(5));
        let flow = deliver_page(&ctx, &mut progress, &mut wakes, page)
            .await
            .expect("deliver_page");

        assert_eq!(flow, ControlFlow::Continue(()), "the page must complete");
        assert_eq!(progress.cursor, Cursor::at(8), "cursor after the page");
        let mut payloads = Vec::new();
        while let Ok(delivery) = inbox.try_recv() {
            payloads.push(delivery.message.payload);
        }
        assert_eq!(payloads, vec!["stamped 7".to_string()], "deliveries");
    }

    /// [`deliver_page`] logs one error line for a page's changesets with no
    /// position, a value that is not an object, an object with no stamp, and
    /// a stamp of `i64::MAX`, with their count and the first of them.
    #[tokio::test(flavor = "current_thread")]
    async fn deliver_page_logs_one_line_for_a_page_with_no_position() {
        let db: &'static Surreal<any::Any> = Box::leak(Box::new(mem_db().await));
        let (ctx, _inbox, mut wakes) = page_fixture(db, CancellationToken::new(), 8).await;
        let page = vec![
            "first-bad".to_string().into_value(),
            object(vec![("marker", "second-bad".to_string().into_value())]),
            changeset(i64::MAX),
            changeset(7),
        ];

        let (subscriber, sink) = capturing_subscriber();
        let guard = tracing::subscriber::set_default(subscriber);
        let mut progress = LoopProgress::at(Cursor::at(5));
        let flow = deliver_page(&ctx, &mut progress, &mut wakes, page)
            .await
            .expect("deliver_page");
        drop(guard);

        assert_eq!(flow, ControlFlow::Continue(()), "the page must complete");
        let logs = logged(&sink);
        let errors: Vec<&str> = logs.lines().filter(|l| l.contains("ERROR")).collect();
        assert_eq!(errors.len(), 1, "expected one error line; got {errors:?}");
        for needle in ["skipped: 3;", "first-bad", NO_POSITION_LOG] {
            assert!(
                errors[0].contains(needle),
                "the line must hold {needle:?}; got {}",
                errors[0]
            );
        }
        assert!(
            !errors[0].contains("second-bad"),
            "the line must hold only the first changeset; got {}",
            errors[0]
        );
    }

    /// [`deliver_page`] skips a changeset stamped `i64::MAX` with no delivery
    /// and no cursor move, and moves the cursor to `i64::MAX` past a changeset
    /// stamped `i64::MAX - 1`.
    #[tokio::test(flavor = "current_thread")]
    async fn deliver_page_skips_a_changeset_at_i64_max() {
        let db: &'static Surreal<any::Any> = Box::leak(Box::new(mem_db().await));
        let (ctx, inbox, mut wakes) = page_fixture(db, CancellationToken::new(), 8).await;
        let last = stamped(i64::MAX.into_value(), vec![message_change(&frank(), "max")]);

        let mut progress = LoopProgress::at(Cursor::at(5));
        let flow = deliver_page(&ctx, &mut progress, &mut wakes, vec![last.clone()])
            .await
            .expect("deliver_page");
        assert_eq!(flow, ControlFlow::Continue(()), "the page must complete");
        assert_eq!(
            progress.cursor,
            Cursor::at(5),
            "cursor after an i64::MAX changeset"
        );
        let delivered = inbox.try_recv().map(|d| d.message.payload);
        assert!(
            delivered.is_err(),
            "an i64::MAX changeset must not deliver; got {delivered:?}"
        );

        let below = stamped((i64::MAX - 1).into_value(), Vec::new());
        let flow = deliver_page(&ctx, &mut progress, &mut wakes, vec![below, last])
            .await
            .expect("deliver_page");
        assert_eq!(flow, ControlFlow::Continue(()), "the page must complete");
        assert_eq!(
            progress.cursor,
            Cursor::at(i64::MAX),
            "cursor past i64::MAX - 1"
        );
    }

    /// A save stalled on a complete page: within [`CURSOR_SAVE_TIMEOUT`] plus
    /// 500 ms, [`deliver_page`] logs one error line with the agent and the
    /// position and continues; the cursor moves, and the known-saved position
    /// does not.
    #[tokio::test(flavor = "current_thread")]
    async fn stalled_save_on_a_complete_page_moves_the_cursor() {
        let db: &'static Surreal<any::Any> = Box::leak(Box::new(mem_db().await));
        let (ctx, _inbox, mut wakes) = page_fixture(db, CancellationToken::new(), 8).await;
        let held = hold_session(db).await;

        let (subscriber, sink) = capturing_subscriber();
        let guard = tracing::subscriber::set_default(subscriber);
        let began = tokio::time::Instant::now();
        let mut progress = LoopProgress::at(Cursor::at(5));
        let page = vec![changeset(7)];
        let result = deliver_page(&ctx, &mut progress, &mut wakes, page).await;
        let elapsed = began.elapsed();
        drop(guard);
        held.await.expect("join");

        assert!(
            matches!(result, Ok(ControlFlow::Continue(()))),
            "the pass must continue after {elapsed:?}, got {result:?}"
        );
        assert_eq!(progress.cursor, Cursor::at(8), "cursor after the timeout");
        assert_eq!(progress.saved, Cursor::at(5), "known-saved position");
        assert!(
            elapsed < CURSOR_SAVE_TIMEOUT + Duration::from_millis(500),
            "the save held deliver_page for {elapsed:?}"
        );
        let logs = logged(&sink);
        let errors: Vec<&str> = logs.lines().filter(|l| l.contains("ERROR")).collect();
        assert_eq!(errors.len(), 1, "expected one error line; got {errors:?}");
        assert!(
            errors[0].contains("frank") && errors[0].contains("8#0"),
            "the line must hold the agent and the position; got {}",
            errors[0]
        );
    }

    /// A failed save call on a complete page: [`deliver_page`] returns
    /// [`Error::CursorSave`] for the agent, and neither position moves. The
    /// `cursor` table's `versionstamp` type refuses the write.
    #[tokio::test(flavor = "current_thread")]
    async fn failed_save_call_returns_the_error_and_keeps_the_cursor() {
        let db: &'static Surreal<any::Any> = Box::leak(Box::new(mem_db().await));
        db.query(format!(
            "DEFINE TABLE {CURSOR_TABLE} SCHEMAFULL; \
             DEFINE FIELD versionstamp ON {CURSOR_TABLE} TYPE string;"
        ))
        .await
        .and_then(|r| r.check())
        .expect("define a cursor table that refuses a save");
        let (ctx, _inbox, mut wakes) = page_fixture(db, CancellationToken::new(), 8).await;

        let mut progress = LoopProgress::at(Cursor::at(5));
        let page = vec![changeset(7)];
        let result = deliver_page(&ctx, &mut progress, &mut wakes, page).await;

        assert!(
            matches!(&result, Err(Error::CursorSave { agent, .. }) if agent == "frank"),
            "expected CursorSave for frank, got {result:?}"
        );
        assert_eq!(progress.cursor, Cursor::at(5), "cursor after a failed save");
        assert_eq!(progress.saved, Cursor::at(5), "known-saved position");
    }

    /// A page that does not move the cursor, read with the cursor past the
    /// known-saved position: [`deliver_page`] saves the cursor.
    #[tokio::test(flavor = "current_thread")]
    async fn next_page_saves_a_position_past_the_known_saved_one() {
        let db: &'static Surreal<any::Any> = Box::leak(Box::new(mem_db().await));
        define_agent_tables(db).await;
        let (ctx, _inbox, mut wakes) = page_fixture(db, CancellationToken::new(), 8).await;

        let mut progress = LoopProgress::at(Cursor::at(5));
        progress.cursor = Cursor::at(8);
        let page = vec![object(vec![])];
        let flow = deliver_page(&ctx, &mut progress, &mut wakes, page)
            .await
            .expect("deliver_page");

        assert_eq!(flow, ControlFlow::Continue(()), "the page must complete");
        let saved = load_cursor(db, "frank").await.expect("read cursor");
        assert_eq!(saved, Some(Cursor::at(8)), "persisted cursor");
        assert_eq!(progress.saved, Cursor::at(8), "known-saved position");
    }

    /// [`deliver_page`] from one position logs a page's no-position line once
    /// per count: counts 1, 1, 2, 2, 1 give 3 lines.
    #[tokio::test(flavor = "current_thread")]
    async fn a_count_change_at_the_same_position_logs_again() {
        let db: &'static Surreal<any::Any> = Box::leak(Box::new(mem_db().await));
        let (ctx, _inbox, mut wakes) = page_fixture(db, CancellationToken::new(), 8).await;

        let (subscriber, sink) = capturing_subscriber();
        let guard = tracing::subscriber::set_default(subscriber);
        let mut progress = LoopProgress::at(Cursor::at(5));
        for count in [1, 1, 2, 2, 1] {
            let page = vec![object(vec![]); count];
            let flow = deliver_page(&ctx, &mut progress, &mut wakes, page)
                .await
                .expect("deliver_page");
            assert_eq!(flow, ControlFlow::Continue(()), "the page must complete");
        }
        drop(guard);

        let logs = logged(&sink);
        let lines = no_position_lines(&logs);
        assert_eq!(lines.len(), 3, "expected 3 lines; got {lines:?}");
    }

    /// A save stalled on a stop, a cancel and a closed bus: [`deliver_page`]
    /// logs the timeout at error level and returns `ControlFlow::Break` within
    /// [`CURSOR_SAVE_TIMEOUT`] plus 500 ms, the cursor does not move, and the
    /// save counts as the stop's one save.
    #[tokio::test(flavor = "current_thread")]
    async fn stalled_save_on_a_stop_logs_and_breaks() {
        for want in [Stop::Cancelled, Stop::NoReceivers] {
            let db: &'static Surreal<any::Any> = Box::leak(Box::new(mem_db().await));
            let token = CancellationToken::new();
            let (ctx, inbox, mut wakes) = page_fixture(db, token.clone(), 8).await;
            match want {
                Stop::Cancelled => token.cancel(),
                _ => drop(inbox),
            }
            let held = hold_session(db).await;
            let page = vec![stamped(
                7i64.into_value(),
                vec![message_change(&frank(), "stopped")],
            )];

            let (subscriber, sink) = capturing_subscriber();
            let guard = tracing::subscriber::set_default(subscriber);
            let began = tokio::time::Instant::now();
            let mut progress = LoopProgress::at(Cursor::at(5));
            let result = deliver_page(&ctx, &mut progress, &mut wakes, page).await;
            let elapsed = began.elapsed();
            drop(guard);
            held.await.expect("join");

            assert!(
                matches!(result, Ok(ControlFlow::Break(stop)) if stop == want),
                "expected Break({want:?}) after {elapsed:?}, got {result:?}"
            );
            assert_eq!(progress.cursor, Cursor::at(5), "cursor after {want:?}");
            assert!(
                progress.stop_save_tried,
                "the save on {want:?} must count as the stop's one save"
            );
            assert!(
                elapsed < CURSOR_SAVE_TIMEOUT + Duration::from_millis(500),
                "the save held deliver_page for {elapsed:?} on {want:?}"
            );
            let logs = logged(&sink);
            assert!(
                logs.lines().any(|l| l.contains("ERROR")
                    && l.contains("could not save cursor 7#0")
                    && l.contains(&SaveFailure::TimedOut.to_string())),
                "no error line for the timed-out save on {want:?}; got {logs:?}"
            );
        }
    }

    /// One read of a catch-up page.
    type CatchUpRead = futures::future::BoxFuture<'static, Result<ControlFlow<Stop, Vec<Value>>>>;

    /// A `Coalition<String>` that holds one loop for `frank`, running
    /// [`Agent::listen_on`] on `db` from `cursor` with a bus of `capacity` and
    /// page reads through `read_page`. Returns after the first page read and
    /// the start signal.
    async fn scripted_coalition<F>(
        db: &'static Surreal<any::Any>,
        cursor: Cursor,
        capacity: usize,
        read_page: F,
    ) -> Coalition<String>
    where
        F: FnMut(Cursor) -> CatchUpRead + Send + 'static,
    {
        let (inbox_tx, inbox) = async_channel::bounded(capacity);
        let cancellation_token = CancellationToken::new();
        let ctx = LoopCtx {
            db,
            owner: frank(),
            agent: "frank",
            inbox_tx,
            token: cancellation_token.child_token(),
        };
        let (ready_tx, ready_rx) = oneshot::channel();
        let start = StartGate::default();
        let gate = start.clone();
        let task_tracker = TaskTracker::new();
        task_tracker.spawn(async move {
            Agent::listen_on(&ctx, cursor, ready_tx, gate, read_page).await;
        });
        ready_rx
            .await
            .expect("the loop sends a ready signal")
            .expect("the loop starts");
        start.open();
        Coalition {
            agents: Arc::default(),
            task_tracker,
            cancellation_token,
            inbox,
            _payload: PhantomData,
        }
    }

    /// `pages` in order as a `read_page` for [`scripted_coalition`], with
    /// each cursor it is called with sent on `seen`, and empty pages after
    /// the last.
    fn scripted_reads(
        pages: Vec<Result<Vec<Value>>>,
        seen: tokio::sync::mpsc::UnboundedSender<Cursor>,
    ) -> impl FnMut(Cursor) -> CatchUpRead + Send + 'static {
        let mut pages = pages.into_iter();
        move |at| {
            seen.send(at).expect("the test holds the receiver");
            let page = pages.next().unwrap_or_else(|| Ok(Vec::new()));
            Box::pin(std::future::ready(page.map(ControlFlow::Continue)))
        }
    }

    /// The next cursor on `seen`, within 5 s.
    async fn next_read(seen: &mut tokio::sync::mpsc::UnboundedReceiver<Cursor>) -> Cursor {
        timeout(Duration::from_secs(5), seen.recv())
            .await
            .expect("no page read within 5 s")
            .expect("the read channel is open")
    }

    /// A row in `message` for `frank`, which fires frank's wake stream.
    async fn wake_frank(db: &Surreal<any::Any>) {
        db.query("CREATE message CONTENT { out: agent:frank }")
            .await
            .and_then(|r| r.check())
            .expect("create a message row");
    }

    /// A catch-up read that fails, for a pass that must end the subscription.
    fn failed_read() -> Error {
        Error::CatchUp {
            agent: "frank".to_string(),
            source: reply_error("read refused".to_string()),
        }
    }

    /// A short page with no position, read six times by one listen loop: the
    /// first read from position 5 logs it; later reads from 5, across a wake
    /// and a resubscribe, do not, the last of them with a page that moves the
    /// cursor to 8; a read from the new position 8 logs it. The line count is
    /// taken after each read.
    #[tokio::test(flavor = "current_thread")]
    async fn no_position_is_logged_once_per_cursor_position() {
        let db: &'static Surreal<any::Any> = Box::leak(Box::new(mem_db().await));
        let bad = || object(vec![("marker", "no-stamp".to_string().into_value())]);
        let pages = vec![
            Ok(vec![bad()]),
            Ok(vec![bad()]),
            Err(failed_read()),
            Ok(vec![bad()]),
            Ok(vec![changeset(7), bad()]),
            Ok(vec![bad()]),
        ];
        let (seen_tx, mut seen) = tokio::sync::mpsc::unbounded_channel();

        let (subscriber, sink) = capturing_subscriber();
        let guard = tracing::subscriber::set_default(subscriber);
        let reads = scripted_reads(pages, seen_tx);
        let coalition = scripted_coalition(db, Cursor::at(5), 8, reads).await;
        let settle = Duration::from_millis(50);
        let lines_now = || no_position_lines(&logged(&sink)).len();
        let mut ats = vec![next_read(&mut seen).await];
        sleep(settle).await;
        let mut counts = vec![lines_now()];
        // Reads 2 and 3 follow one wake each; read 3 fails, so read 4 follows
        // the resubscribe; reads 5 and 6 follow one wake each.
        for wakes in [true, true, false, true, true] {
            if wakes {
                wake_frank(db).await;
            }
            ats.push(next_read(&mut seen).await);
            sleep(settle).await;
            counts.push(lines_now());
        }
        coalition.shutdown().await;
        drop(guard);

        let five = Cursor::at(5);
        assert_eq!(
            ats,
            vec![five, five, five, five, five, Cursor::at(8)],
            "the cursor of each read"
        );
        assert_eq!(
            counts,
            vec![1, 1, 1, 1, 1, 2],
            "no-position lines after each read; got {counts:?}"
        );
    }

    /// A failed save call on a stop in one listen loop: one error line holds
    /// the position and the call's error. The `cursor` table's `versionstamp`
    /// type refuses the write.
    #[tokio::test(flavor = "current_thread")]
    async fn failed_save_call_on_a_stop_is_logged_with_the_position() {
        let db: &'static Surreal<any::Any> = Box::leak(Box::new(mem_db().await));
        refuse_cursor_writes(db).await;
        let changes = vec![
            message_change(&frank(), "on the bus"),
            message_change(&frank(), "not on the bus"),
        ];
        let pages = vec![Ok(vec![stamped(7i64.into_value(), changes)])];
        let (seen_tx, _seen) = tokio::sync::mpsc::unbounded_channel();

        let (subscriber, sink) = capturing_subscriber();
        let guard = tracing::subscriber::set_default(subscriber);
        let reads = scripted_reads(pages, seen_tx);
        let coalition = scripted_coalition(db, Cursor::at(5), 1, reads).await;
        let inbox = coalition.inbox();
        timeout(Duration::from_secs(5), async {
            while !inbox.is_full() {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("precondition: the bus must fill");
        coalition.shutdown().await;
        drop(guard);

        let logs = logged(&sink);
        let lines: Vec<&str> = logs
            .lines()
            .filter(|l| l.contains("ERROR") && l.contains("could not save cursor 7#1"))
            .collect();
        assert_eq!(lines.len(), 1, "expected one line; got {logs:?}");
        assert!(
            lines[0].contains(CURSOR_SAVE_FAILED),
            "the line must hold the call's error; got {}",
            lines[0]
        );
    }

    /// Text of [`Error::CursorSave`].
    const CURSOR_SAVE_FAILED: &str = "failed to persist the durable-log cursor";

    /// Make every later write to `cursor` on `db` fail: `versionstamp` takes
    /// only a string.
    async fn refuse_cursor_writes(db: &Surreal<any::Any>) {
        db.query(format!(
            "DEFINE TABLE IF NOT EXISTS {CURSOR_TABLE}; \
             DEFINE FIELD OVERWRITE versionstamp ON {CURSOR_TABLE} TYPE string;"
        ))
        .await
        .and_then(|r| r.check())
        .expect("define a cursor table that refuses a save");
    }

    /// A timed-out save on a complete page, then a refused write, then a stop
    /// in one listen loop: one error line for the save at the stop holds the
    /// position and the call's error.
    #[tokio::test(flavor = "current_thread")]
    async fn failed_save_call_at_the_stop_is_logged_once() {
        let db: &'static Surreal<any::Any> = Box::leak(Box::new(mem_db().await));
        define_agent_tables(db).await;
        let (seen_tx, _seen) = tokio::sync::mpsc::unbounded_channel();

        let (subscriber, sink) = capturing_subscriber();
        let guard = tracing::subscriber::set_default(subscriber);
        let pages = in_order(vec![vec![changeset(7)]]);
        let reads = reads_with_a_first_stall(db, 3, pages, seen_tx);
        let coalition = scripted_coalition(db, Cursor::at(5), 8, reads).await;
        sleep(Duration::from_secs(3)).await;
        refuse_cursor_writes(db).await;
        coalition.shutdown().await;
        drop(guard);

        let logs = logged(&sink);
        let lines = stop_save_lines(&logs);
        assert_eq!(lines.len(), 1, "expected one stop line; got {logs:?}");
        for needle in ["ERROR", "8#0", CURSOR_SAVE_FAILED] {
            assert!(
                lines[0].contains(needle),
                "the stop line must hold {needle:?}; got {}",
                lines[0]
            );
        }
    }

    /// A full page that leaves the cursor in place, read by one listen loop:
    /// each pass fails with [`Error::CatchUp`] for the agent, the loop ends
    /// the live query and waits its backoff, 200 ms then 400 ms, and the next
    /// read comes from the same position on a new live query. The page gives
    /// one no-position line in 3 reads, and no cursor is saved.
    #[tokio::test(flavor = "current_thread")]
    async fn full_page_that_leaves_the_cursor_fails_the_pass() {
        let db: &'static Surreal<any::Any> = Box::leak(Box::new(mem_db().await));
        define_agent_tables(db).await;
        let full = vec![object(vec![]); CATCHUP_BATCH];
        let (seen_tx, mut seen) = tokio::sync::mpsc::unbounded_channel();
        let mut left = 3;
        let reads = move |at: Cursor| -> CatchUpRead {
            let page = if left > 0 { full.clone() } else { Vec::new() };
            left -= 1;
            let seen_tx = seen_tx.clone();
            Box::pin(async move {
                let lives = message_lives(db).await;
                let read = (at, lives, tokio::time::Instant::now());
                seen_tx.send(read).expect("the test holds the receiver");
                Ok(ControlFlow::Continue(page))
            })
        };

        let (subscriber, sink) = capturing_subscriber();
        let guard = tracing::subscriber::set_default(subscriber);
        let coalition = scripted_coalition(db, Cursor::at(5), 8, reads).await;
        let mut read = Vec::new();
        for _ in 0..4 {
            let next = timeout(Duration::from_secs(5), seen.recv()).await;
            let next = next
                .unwrap_or_else(|_| panic!("read {} of 4 did not come within 5 s", read.len() + 1));
            read.push(next.expect("the read channel is open"));
        }
        coalition.shutdown().await;
        drop(guard);

        let positions: Vec<Cursor> = read.iter().map(|(at, _, _)| *at).collect();
        assert_eq!(positions, vec![Cursor::at(5); 4], "the cursor of each read");
        let lives: Vec<&Vec<String>> = read.iter().map(|(_, lives, _)| lives).collect();
        assert!(
            lives.iter().all(|l| l.len() == 1),
            "each read must see one live query; they saw {lives:?}"
        );
        let distinct: std::collections::HashSet<_> = lives.iter().map(|l| &l[0]).collect();
        assert_eq!(
            distinct.len(),
            4,
            "each pass needs a new live query: {lives:?}"
        );
        for (pair, backoff) in read.windows(2).zip([200, 400, 800]) {
            let waited = pair[1].2 - pair[0].2;
            assert!(
                waited >= Duration::from_millis(backoff),
                "expected a {backoff} ms backoff between reads; waited {waited:?}"
            );
        }
        let saved = load_cursor(db, "frank").await.expect("read cursor");
        assert_eq!(saved, None, "a pass that does not move must save no cursor");

        let logs = logged(&sink);
        let lines = no_position_lines(&logs);
        assert_eq!(lines.len(), 1, "no-position lines in 3 reads: {lines:?}");
        let catch_up = failed_read().to_string();
        let failed = logs
            .lines()
            .filter(|l| {
                l.contains("ERROR")
                    && l.contains("catch_up failed for frank")
                    && l.contains(&catch_up)
                    && l.contains("left the cursor at 5#0")
            })
            .count();
        assert_eq!(failed, 3, "catch-up failure lines; got {logs:?}");
    }

    /// Text of the line for a save at a loop's stop.
    const STOP_SAVED_LOG: &str = "at its stop";

    /// The lines of `logs` for a save at a loop's stop, finished or not.
    fn stop_save_lines(logs: &str) -> Vec<&str> {
        logs.lines()
            .filter(|l| l.contains(STOP_SAVED_LOG))
            .collect()
    }

    /// `pages` in order, then empty pages, as a `page_at` for
    /// [`reads_with_a_first_stall`].
    fn in_order(pages: Vec<Vec<Value>>) -> impl FnMut(Cursor) -> Vec<Value> + Send + 'static {
        let mut pages = pages.into_iter();
        move |_at| pages.next().unwrap_or_default()
    }

    /// A `read_page` for [`scripted_coalition`] that returns `page_at` of each
    /// cursor, with each cursor sent on `seen`. The first read stalls every
    /// call on `db` for `stall_secs` before it returns.
    fn reads_with_a_first_stall(
        db: &'static Surreal<any::Any>,
        stall_secs: u64,
        mut page_at: impl FnMut(Cursor) -> Vec<Value> + Send + 'static,
        seen: tokio::sync::mpsc::UnboundedSender<Cursor>,
    ) -> impl FnMut(Cursor) -> CatchUpRead + Send + 'static {
        let mut first = true;
        move |at| {
            seen.send(at).expect("the test holds the receiver");
            let hold = std::mem::take(&mut first);
            let page = page_at(at);
            Box::pin(async move {
                if hold {
                    drop(hold_session_for(db, stall_secs).await);
                }
                Ok(ControlFlow::Continue(page))
            })
        }
    }

    /// A save stalled on a complete page, in one listen loop: the delivery
    /// arrives once, the next read comes from the position past that page, and
    /// that page's save persists its position.
    #[tokio::test(flavor = "current_thread")]
    async fn listen_loop_moves_past_a_page_whose_save_timed_out() {
        let db: &'static Surreal<any::Any> = Box::leak(Box::new(mem_db().await));
        define_agent_tables(db).await;
        let (seen_tx, mut seen) = tokio::sync::mpsc::unbounded_channel();
        // The feed by position: a read from 5 returns the message each time.
        let page_at = |at: Cursor| match at.versionstamp {
            5 => vec![stamped(
                7i64.into_value(),
                vec![message_change(&frank(), "once")],
            )],
            8 => vec![changeset(9)],
            _ => Vec::new(),
        };
        let reads = reads_with_a_first_stall(db, 3, page_at, seen_tx);
        let coalition = scripted_coalition(db, Cursor::at(5), 8, reads).await;
        let first_read = next_read(&mut seen).await;
        let inbox = coalition.inbox();
        let delivered = timeout(Duration::from_secs(5), inbox.recv()).await;
        sleep(CURSOR_SAVE_TIMEOUT).await;
        wake_frank(db).await;
        let second_read = next_read(&mut seen).await;
        wake_frank(db).await;
        let third_read = next_read(&mut seen).await;
        coalition.shutdown().await;

        let delivered = delivered
            .expect("precondition: a delivery within 5 s")
            .expect("the bus is open");
        assert_eq!(delivered.message.payload, "once");
        let repeated = inbox.try_recv().map(|d| d.message.payload);
        assert!(repeated.is_err(), "a repeated delivery: {repeated:?}");
        assert_eq!(
            [first_read, second_read, third_read],
            [Cursor::at(5), Cursor::at(8), Cursor::at(10)],
            "the cursor of each read"
        );
        let saved = load_cursor(db, "frank").await.expect("read cursor");
        assert_eq!(saved, Some(Cursor::at(10)), "persisted cursor");
    }

    /// An idle stop after a timed-out save on a complete page: the loop makes
    /// exactly one save at the stop, and it persists the cursor.
    #[tokio::test(flavor = "current_thread")]
    async fn idle_stop_after_a_timed_out_save_saves_once() {
        let db: &'static Surreal<any::Any> = Box::leak(Box::new(mem_db().await));
        define_agent_tables(db).await;
        let (seen_tx, _seen) = tokio::sync::mpsc::unbounded_channel();

        let (subscriber, sink) = capturing_subscriber();
        let guard = tracing::subscriber::set_default(subscriber);
        let pages = in_order(vec![vec![changeset(7)]]);
        let reads = reads_with_a_first_stall(db, 3, pages, seen_tx);
        let coalition = scripted_coalition(db, Cursor::at(5), 8, reads).await;
        sleep(Duration::from_secs(3)).await;
        let before = load_cursor(db, "frank").await.expect("read cursor");
        coalition.shutdown().await;
        drop(guard);

        assert_eq!(
            before, None,
            "precondition: the timed-out save did not land"
        );
        let saved = load_cursor(db, "frank").await.expect("read cursor");
        assert_eq!(saved, Some(Cursor::at(8)), "persisted cursor");
        let logs = logged(&sink);
        let lines = stop_save_lines(&logs);
        assert_eq!(lines.len(), 1, "expected one stop save; got {lines:?}");
    }

    /// An idle stop with every call stalled, after a timed-out save on a
    /// complete page: the loop's one save at the stop times out, it is logged
    /// at error level before the `KILL`, and [`Coalition::shutdown`] stays in
    /// its bound.
    #[tokio::test(flavor = "current_thread")]
    async fn stalled_stop_save_is_logged_once_before_the_kill() {
        let db: &'static Surreal<any::Any> = Box::leak(Box::new(mem_db().await));
        let (seen_tx, _seen) = tokio::sync::mpsc::unbounded_channel();

        let (subscriber, sink) = capturing_subscriber();
        let guard = tracing::subscriber::set_default(subscriber);
        let pages = in_order(vec![vec![changeset(7)]]);
        let reads = reads_with_a_first_stall(db, ALL_CALLS_STALL_SECS, pages, seen_tx);
        let coalition = scripted_coalition(db, Cursor::at(5), 8, reads).await;
        sleep(CURSOR_SAVE_TIMEOUT + Duration::from_millis(300)).await;
        assert_bounded_shutdown(&coalition, &sink).await;
        drop(guard);

        let logs = logged(&sink);
        let lines = stop_save_lines(&logs);
        assert_eq!(lines.len(), 1, "expected one stop save; got {lines:?}");
        assert!(
            lines[0].contains("ERROR") && lines[0].contains("8#0"),
            "the line must be an error with the position; got {}",
            lines[0]
        );
        let order = |needle: &str| logs.find(needle).expect("logged line");
        assert!(
            order(STOP_SAVED_LOG) < order("KILL of live query"),
            "the stop save must come before the KILL; got {logs:?}"
        );
    }

    /// [`save_cursor`] on the schema of `cursor` in `sdb.rs`, with the row
    /// absent, at a lower position, at an equal position and at a higher one,
    /// through versionstamp and through `change_index`, and with a lower stamp
    /// and a higher index: each save returns `Ok`, and the row moves only
    /// forward.
    #[tokio::test(flavor = "current_thread")]
    async fn save_cursor_moves_the_row_only_forward() {
        let db = mem_db().await;
        db.query(format!(
            "DEFINE TABLE {CURSOR_TABLE} SCHEMAFULL; \
             DEFINE FIELD versionstamp ON {CURSOR_TABLE} TYPE int; \
             DEFINE FIELD change_index ON {CURSOR_TABLE} TYPE int DEFAULT 0;"
        ))
        .await
        .and_then(|r| r.check())
        .expect("define the cursor table");
        let at = |versionstamp, change_index| Cursor {
            versionstamp,
            change_index,
        };

        for (save, want) in [
            (at(5, 0), at(5, 0)),
            (at(8, 0), at(8, 0)),
            (at(6, 0), at(8, 0)),
            (at(8, 0), at(8, 0)),
            (at(8, 2), at(8, 2)),
            (at(8, 1), at(8, 2)),
            (at(9, 0), at(9, 0)),
            (at(8, 3), at(9, 0)),
        ] {
            let saved = save_cursor(&db, "frank", save).await;
            assert!(saved.is_ok(), "save of {save} returned {saved:?}");
            let row = load_cursor(&db, "frank").await.expect("read cursor");
            assert_eq!(row, Some(want), "the row after a save of {save}");
        }
    }

    /// The lines of `logs` for a retry of a save that did not finish.
    fn retry_lines(logs: &str) -> Vec<&str> {
        logs.lines()
            .filter(|l| l.contains("ERROR") && l.contains("retry of cursor"))
            .collect()
    }

    /// A timed-out save, then an empty page with a healthy database, in one
    /// listen loop: the row moves to the cursor with no retry line, and the
    /// loop's stop finds nothing to save.
    #[tokio::test(flavor = "current_thread")]
    async fn empty_page_retries_a_timed_out_save() {
        let db: &'static Surreal<any::Any> = Box::leak(Box::new(mem_db().await));
        define_agent_tables(db).await;
        let (seen_tx, mut seen) = tokio::sync::mpsc::unbounded_channel();

        let (subscriber, sink) = capturing_subscriber();
        let guard = tracing::subscriber::set_default(subscriber);
        let pages = in_order(vec![vec![changeset(7)]]);
        let reads = reads_with_a_first_stall(db, 3, pages, seen_tx);
        let coalition = scripted_coalition(db, Cursor::at(5), 8, reads).await;
        sleep(Duration::from_secs(3)).await;
        let before = load_cursor(db, "frank").await.expect("read cursor");
        wake_frank(db).await;
        let reads_seen = (next_read(&mut seen).await, next_read(&mut seen).await);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let mut row = load_cursor(db, "frank").await.expect("read cursor");
        while row.is_none() && tokio::time::Instant::now() < deadline {
            sleep(Duration::from_millis(10)).await;
            row = load_cursor(db, "frank").await.expect("read cursor");
        }
        coalition.shutdown().await;
        drop(guard);

        assert_eq!(reads_seen, (Cursor::at(5), Cursor::at(8)), "reads");
        assert_eq!(before, None, "precondition: the first save did not land");
        assert_eq!(row, Some(Cursor::at(8)), "the row after the empty page");
        let logs = logged(&sink);
        assert_eq!(retry_lines(&logs), Vec::<&str>::new(), "retry failures");
        assert_eq!(stop_save_lines(&logs), Vec::<&str>::new(), "stop saves");
    }

    /// A timed-out save on a full page, then an empty page with every call
    /// still stalled, in one listen loop: one retry line holds the position,
    /// the pass does not fail, and there is no further read.
    #[tokio::test(flavor = "current_thread")]
    async fn empty_page_retry_timeout_logs_and_the_pass_continues() {
        let db: &'static Surreal<any::Any> = Box::leak(Box::new(mem_db().await));
        let batch = i64::try_from(CATCHUP_BATCH).expect("batch fits i64");
        let full: Vec<Value> = (6..6 + batch).map(changeset).collect();
        let (seen_tx, mut seen) = tokio::sync::mpsc::unbounded_channel();

        let (subscriber, sink) = capturing_subscriber();
        let guard = tracing::subscriber::set_default(subscriber);
        let reads = reads_with_a_first_stall(db, 5, in_order(vec![full]), seen_tx);
        let coalition = scripted_coalition(db, Cursor::at(5), 8, reads).await;
        let reads_seen = (next_read(&mut seen).await, next_read(&mut seen).await);
        sleep(CURSOR_SAVE_TIMEOUT + Duration::from_millis(300)).await;
        let later_read = seen.try_recv();
        coalition.shutdown().await;
        drop(guard);

        let position = Cursor::at(6 + batch);
        assert_eq!(reads_seen, (Cursor::at(5), position), "reads");
        assert!(later_read.is_err(), "a further read: {later_read:?}");
        let logs = logged(&sink);
        let retries = retry_lines(&logs);
        assert_eq!(retries.len(), 1, "expected one retry line; got {retries:?}");
        assert!(
            retries[0].contains(&position.to_string()),
            "the line must hold the position {position}; got {}",
            retries[0]
        );
        assert!(
            !logs.contains("catch_up failed"),
            "the retry must not fail the pass; got {logs:?}"
        );
    }

    /// A retry whose save call fails: one error line with the position, and
    /// the known-saved position does not move. The `cursor` table's
    /// `versionstamp` type refuses the write.
    #[tokio::test(flavor = "current_thread")]
    async fn failed_retry_is_logged_with_the_position() {
        let db: &'static Surreal<any::Any> = Box::leak(Box::new(mem_db().await));
        db.query(format!(
            "DEFINE TABLE {CURSOR_TABLE} SCHEMAFULL; \
             DEFINE FIELD versionstamp ON {CURSOR_TABLE} TYPE string;"
        ))
        .await
        .and_then(|r| r.check())
        .expect("define a cursor table that refuses a save");
        let (ctx, _inbox, _wakes) = page_fixture(db, CancellationToken::new(), 8).await;
        let mut progress = LoopProgress::at(Cursor::at(5));
        progress.cursor = Cursor::at(8);

        let (subscriber, sink) = capturing_subscriber();
        let guard = tracing::subscriber::set_default(subscriber);
        retry_save(&ctx, &mut progress).await;
        drop(guard);

        assert_eq!(progress.saved, Cursor::at(5), "known-saved position");
        let logs = logged(&sink);
        let retries = retry_lines(&logs);
        assert_eq!(retries.len(), 1, "expected one retry line; got {retries:?}");
        assert!(
            retries[0].contains("8#0"),
            "the line must hold the position; got {}",
            retries[0]
        );
    }

    /// With every call stalled, a cancel during a retry on an empty page: the
    /// retry counts as the one save after the cancel, so the loop makes no
    /// save at its stop and [`Coalition::shutdown`] stays in its bound.
    #[tokio::test(flavor = "current_thread")]
    async fn retry_in_flight_at_a_cancel_is_the_one_save() {
        let db: &'static Surreal<any::Any> = Box::leak(Box::new(mem_db().await));
        let batch = i64::try_from(CATCHUP_BATCH).expect("batch fits i64");
        let full: Vec<Value> = (6..6 + batch).map(changeset).collect();
        let (seen_tx, mut seen) = tokio::sync::mpsc::unbounded_channel();

        let (subscriber, sink) = capturing_subscriber();
        let guard = tracing::subscriber::set_default(subscriber);
        let pages = in_order(vec![full]);
        let reads = reads_with_a_first_stall(db, ALL_CALLS_STALL_SECS, pages, seen_tx);
        let coalition = scripted_coalition(db, Cursor::at(5), 8, reads).await;
        next_read(&mut seen).await;
        next_read(&mut seen).await;
        sleep(Duration::from_millis(200)).await;
        assert_bounded_shutdown(&coalition, &sink).await;
        drop(guard);

        let logs = logged(&sink);
        let retries = retry_lines(&logs);
        assert_eq!(retries.len(), 1, "expected one retry line; got {retries:?}");
        assert_eq!(stop_save_lines(&logs), Vec::<&str>::new(), "stop saves");
    }

    /// Margin over [`CURSOR_SAVE_TIMEOUT`] plus [`LIVE_KILL_TIMEOUT`] in the
    /// shutdown tests below.
    const SHUTDOWN_MARGIN: Duration = Duration::from_secs(1);

    /// Seconds the shutdown tests below stall every call; above their bound.
    const ALL_CALLS_STALL_SECS: u64 = 8;

    /// `coalition.shutdown()` with every call stalled, with its time, and the
    /// assertion that it ends within the bound and that the `KILL` stalled.
    async fn assert_bounded_shutdown(coalition: &Coalition<String>, sink: &LogSink) {
        let limit = CURSOR_SAVE_TIMEOUT + LIVE_KILL_TIMEOUT + SHUTDOWN_MARGIN;
        let began = tokio::time::Instant::now();
        coalition.shutdown().await;
        let elapsed = began.elapsed();
        assert!(
            elapsed < limit,
            "shutdown took {elapsed:?}, above its bound {limit:?}"
        );
        let logs = logged(sink);
        assert!(
            logs.lines()
                .any(|l| l.contains("WARN") && l.contains("KILL of live query")),
            "precondition: the KILL must stall too; got {logs:?}"
        );
    }

    /// With every call stalled and a save in flight at the cancel on a
    /// complete page, [`Coalition::shutdown`] returns within
    /// [`CURSOR_SAVE_TIMEOUT`] plus [`LIVE_KILL_TIMEOUT`] plus
    /// [`SHUTDOWN_MARGIN`], with no further save at the stop.
    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_is_bounded_with_a_save_in_flight() {
        let db: &'static Surreal<any::Any> = Box::leak(Box::new(mem_db().await));
        let page = vec![stamped(
            7i64.into_value(),
            vec![message_change(&frank(), "delivered")],
        )];

        let (seen_tx, _seen) = tokio::sync::mpsc::unbounded_channel();

        let (subscriber, sink) = capturing_subscriber();
        let guard = tracing::subscriber::set_default(subscriber);
        let pages = in_order(vec![page]);
        let reads = reads_with_a_first_stall(db, ALL_CALLS_STALL_SECS, pages, seen_tx);
        let coalition = scripted_coalition(db, Cursor::at(5), 8, reads).await;
        let delivered = timeout(Duration::from_secs(5), coalition.inbox().recv()).await;
        sleep(Duration::from_millis(100)).await;
        assert_bounded_shutdown(&coalition, &sink).await;
        drop(guard);

        let delivered = delivered
            .expect("precondition: the delivery came before the stalled save")
            .expect("the bus is open");
        assert_eq!(delivered.message.payload, "delivered");
        let logs = logged(&sink);
        let timed_out = format!("saving cursor 8#0 {}", SaveFailure::TimedOut);
        assert!(
            logs.lines()
                .any(|l| l.contains("ERROR") && l.contains(&timed_out)),
            "precondition: the save must stall; got {logs:?}"
        );
        let lines = stop_save_lines(&logs);
        assert_eq!(lines, Vec::<&str>::new(), "saves at the stop");
    }

    /// With every call stalled and a cancel during a send on a full bus, the
    /// save of the stop leaves [`Coalition::shutdown`] within
    /// [`CURSOR_SAVE_TIMEOUT`] plus [`LIVE_KILL_TIMEOUT`] plus
    /// [`SHUTDOWN_MARGIN`].
    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_is_bounded_with_a_save_on_a_stop() {
        let db: &'static Surreal<any::Any> = Box::leak(Box::new(mem_db().await));
        let changes = vec![
            message_change(&frank(), "on the bus"),
            message_change(&frank(), "not on the bus"),
        ];
        let page = vec![stamped(7i64.into_value(), changes)];
        let (seen_tx, _seen) = tokio::sync::mpsc::unbounded_channel();

        let (subscriber, sink) = capturing_subscriber();
        let guard = tracing::subscriber::set_default(subscriber);
        let pages = in_order(vec![page]);
        let reads = reads_with_a_first_stall(db, ALL_CALLS_STALL_SECS, pages, seen_tx);
        let coalition = scripted_coalition(db, Cursor::at(5), 1, reads).await;
        let inbox = coalition.inbox();
        timeout(Duration::from_secs(5), async {
            while !inbox.is_full() {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("precondition: the bus must fill");
        sleep(Duration::from_millis(100)).await;
        assert_bounded_shutdown(&coalition, &sink).await;
        drop(guard);

        let logs = logged(&sink);
        assert!(
            logs.lines().any(|l| l.contains("ERROR")
                && l.contains("could not save cursor 7#1")
                && l.contains(&SaveFailure::TimedOut.to_string())),
            "precondition: the save of the stop must stall; got {logs:?}"
        );
    }

    /// With every call stalled, a full page whose save timed out, and then a
    /// cancel during a send on a full bus: the page's save on the stop times
    /// out, the cursor is still past the known-saved position, and the loop
    /// makes no second save, so [`Coalition::shutdown`] stays in its bound.
    #[tokio::test(flavor = "current_thread")]
    async fn no_second_save_after_a_stop_save_timed_out() {
        let db: &'static Surreal<any::Any> = Box::leak(Box::new(mem_db().await));
        let batch = i64::try_from(CATCHUP_BATCH).expect("batch fits i64");
        let full: Vec<Value> = (6..6 + batch).map(changeset).collect();
        let changes = vec![
            message_change(&frank(), "on the bus"),
            message_change(&frank(), "not on the bus"),
        ];
        let pages = in_order(vec![full, vec![stamped(2000i64.into_value(), changes)]]);
        let (seen_tx, mut seen) = tokio::sync::mpsc::unbounded_channel();

        let (subscriber, sink) = capturing_subscriber();
        let guard = tracing::subscriber::set_default(subscriber);
        let reads = reads_with_a_first_stall(db, ALL_CALLS_STALL_SECS, pages, seen_tx);
        let coalition = scripted_coalition(db, Cursor::at(5), 1, reads).await;
        let first_read = next_read(&mut seen).await;
        let second_read = next_read(&mut seen).await;
        let inbox = coalition.inbox();
        timeout(Duration::from_secs(5), async {
            while !inbox.is_full() {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("precondition: the bus must fill");
        sleep(Duration::from_millis(100)).await;
        assert_bounded_shutdown(&coalition, &sink).await;
        drop(guard);

        assert_eq!(
            (first_read, second_read),
            (Cursor::at(5), Cursor::at(6 + batch)),
            "the cursor of each read"
        );
        let logs = logged(&sink);
        assert!(
            logs.lines().any(|l| l.contains("ERROR")
                && l.contains("could not save cursor 2000#1")
                && l.contains(&SaveFailure::TimedOut.to_string())),
            "precondition: the save of the stop must stall; got {logs:?}"
        );
        let lines = stop_save_lines(&logs);
        assert_eq!(lines, Vec::<&str>::new(), "second saves at the stop");
    }

    /// Time a cancelled sweep may take to end in the tests below; below the
    /// time left in [`hold_session`]'s stall.
    const SWEEP_END_LIMIT: Duration = Duration::from_secs(1);

    /// After a cancel, [`sweep_on`] ends within [`SWEEP_END_LIMIT`] while its
    /// connection never arrives.
    #[tokio::test(flavor = "current_thread")]
    async fn sweep_ends_at_a_cancel_during_a_stalled_connection() {
        let token = CancellationToken::new();
        let sweep = tokio::spawn(sweep_on(
            token.clone(),
            Duration::from_secs(1),
            Duration::from_millis(10),
            std::future::pending(),
        ));
        sleep(Duration::from_millis(50)).await;
        assert!(!sweep.is_finished(), "precondition: the connection stalls");

        token.cancel();
        let ended = timeout(SWEEP_END_LIMIT, sweep).await;
        assert!(
            ended.is_ok(),
            "the sweep waited for its connection after a cancel"
        );
    }

    /// After a cancel, [`sweep_on`] ends within [`SWEEP_END_LIMIT`] with its
    /// `DELETE` stalled (see [`hold_session`]).
    #[tokio::test(flavor = "current_thread")]
    async fn sweep_ends_at_a_cancel_during_a_stalled_delete() {
        let db: &'static Surreal<any::Any> = Box::leak(Box::new(mem_db().await));
        let held = hold_session(db).await;
        let token = CancellationToken::new();
        let sweep = tokio::spawn(sweep_on(
            token.clone(),
            Duration::from_secs(1),
            Duration::from_millis(10),
            std::future::ready(db),
        ));
        sleep(Duration::from_millis(200)).await;
        assert!(!sweep.is_finished(), "precondition: the sweep is running");

        let began = tokio::time::Instant::now();
        token.cancel();
        let ended = timeout(SWEEP_END_LIMIT, sweep).await;
        assert!(
            ended.is_ok(),
            "the sweep waited for its DELETE for over {:?}",
            began.elapsed()
        );
        held.await.expect("join");
    }

    /// [`sweep_on`] deletes a message older than the retention and keeps a
    /// newer one.
    #[tokio::test]
    async fn sweep_deletes_only_aged_messages() {
        let db: &'static Surreal<any::Any> = Box::leak(Box::new(mem_db().await));
        db.query(
            "CREATE message:aged CONTENT { created: time::now() - 1h }; \
             CREATE message:new CONTENT { created: time::now() };",
        )
        .await
        .and_then(|r| r.check())
        .expect("create messages");
        let token = CancellationToken::new();
        let sweep = tokio::spawn(sweep_on(
            token.clone(),
            Duration::from_secs(60),
            Duration::from_millis(10),
            std::future::ready(db),
        ));

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while row_exists(db, MESSAGE_TABLE, "aged").await && tokio::time::Instant::now() < deadline
        {
            sleep(Duration::from_millis(10)).await;
        }
        token.cancel();
        sweep.await.expect("join");

        assert!(
            !row_exists(db, MESSAGE_TABLE, "aged").await,
            "the sweep must delete the aged message within 5 s"
        );
        assert!(
            row_exists(db, MESSAGE_TABLE, "new").await,
            "the sweep must keep the new message"
        );
    }
}
