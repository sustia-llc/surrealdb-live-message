# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) for code in this repository.

`surrealdb_live_message` is a **library-first** crate (`src/lib.rs`) that demonstrates agent-to-agent
messaging over SurrealDB graph edges + LIVE queries. `src/main.rs` is a binary example.

## Build & Test

- Integration tests require **Docker**. `sdb_server.rs` spins up a SurrealDB container via `bollard`.
- Run: `cargo test --test integration_test` (the Docker-backed integration test).
- Deterministic unit tests (no Docker) live in `src/subsystems/agents.rs` (the
  `await_ready` handshake-error seam): `cargo test --lib`.
- To use the production cloud endpoint in place of a local container: `RUN_MODE=production cargo test --test integration_test`.
- Config layers (via the `config` crate): `config/default.toml` → `config/{RUN_MODE}.toml` → env vars (`__` separator). `RUN_MODE` defaults to `development`.

## SurrealDB SDK gotchas (do not regress)

These are diagnosed fixes, and tests lock them in. See the docstrings in `src/message.rs` and `src/subsystems/agents.rs`.

1. **Edge fields need `#[surreal(rename = "in")]`.** `SurrealValue` derive uses raw Rust identifiers as wire keys, so `r#in` serializes as `"r#in"` without the attribute. SurrealDB emits `"in"` for edge source. **serde `rename` is ignored**, so you must use the `#[surreal(...)]` attribute. (`src/message.rs:34`)
2. **A bare `SELECT *` / `LIVE SELECT *` on edge records omits `in`/`out`.** Read edge pointers with an explicit projection: `SELECT *, in, out FROM message WHERE ...` (`tests/integration_test.rs:153`, `:177`, `:299`). The pointer assertions at `:167` to `:174` lock it in. The **durable bus sidesteps this**: `SHOW CHANGES` changeset records carry `id`/`in`/`out` natively. So the wake-up subscription is only `LIVE SELECT id FROM message WHERE out = $owner` (`src/subsystems/agents.rs:1451`). The gotcha still applies to any plain `SELECT` you add.
3. Payload types must derive `SurrealValue`. The bound is `Message<T: SurrealValue>`, not only `Serialize`.

## Lifecycle / shutdown contract

### Coalition ownership

- `Coalition` owns a root `CancellationToken`, which is a child of the caller's token with `Coalition::new_with_parent`.
- It spawns listen-loops on `child_token()` and exposes `cancellation_token()` + `shutdown().await`.
- Dropping a `Coalition` cancels its root token without waiting.
- `shutdown().await` cancels the root token and waits for the listen loops and the retention sweep to end.
- The library installs no signal handling. Callers wire top-level shutdown (for example bare `tokio::signal::ctrl_c`) outside the library. Do not add signal handling or `tokio-graceful-shutdown` inside the lib.

### Delivery bus

- Agent `listen_loop`s send each message that their catch-up reads onto a shared **async-channel MPMC** channel as `Delivery<T> { recipient, message }`.
- Consume via `coalition.inbox()`, an `async_channel::Receiver`. Clone it for multiple workers. Sync handlers call `recv_blocking()` on a blocking thread.
- The bus is `async_channel::bounded(INBOX_CAPACITY=256)`. For backpressure, `send` waits while the bus is full.
- The bus closes once all agents shut down. `recv()` then returns the queued deliveries, then `Err(RecvError)`.

### Readiness handshake

`Coalition::new` awaits every listen-loop's ready signal before it returns, so the first `Agent::send` is guaranteed observed. Each wait is bounded by `READY_TIMEOUT`, and a failure cancels the root token. Preserve the ready-signal and start-signal paths.

- Each listen-loop signals after its first LIVE subscribe and its first catch-up page query succeed.
- An error before then is logged with its source chain and sent on the ready channel (`oneshot::Sender<Result<()>>`), so `Coalition::new` returns that error.
- Each loop then holds at its ready point. `Coalition::new` sends the start signal once it has every ready signal.
- So no delivery reaches the bus before `Coalition::new` returns `Ok`.
- A failed `Coalition::new` moves no cursor past a message that was not on the bus (`scenario_failed_new_keeps_backlog`).
- A backlog larger than the bus does not block `Coalition::new` (`scenario_startup_backlog`).

### Agent creation

- `Agent::new` writes `agent:<name>` and an absent `cursor:<name>` in one transaction. So `Agent::new` does not write an agent without its cursor. An existing cursor is kept (`scenario_failed_new_keeps_fresh_cursor`).
- A new agent starts after the highest versionstamp in the whole `message` changefeed. The read goes in pages until a page holds fewer than 1000 changesets. This needs a database where `message` is the one table with a changefeed (`scenario_new_agent_skips_a_long_feed`).
- A `Coalition::new` reads the feed at most once, for its first new agent (`scenario_new_agents_share_one_walk`).
- An existing agent without a cursor starts at versionstamp 0 with a warn log, so its retained backlog is delivered (`scenario_lost_cursor_replays_backlog`). If the first lookup finds the agent, there is no feed read.
- A failed changefeed read in `Agent::new` returns `AgentCreate`. So does a page that is not an array, or a full page with no numeric versionstamp. A page whose highest versionstamp is below its `SINCE` or is `i64::MAX` does too.
- The loop's cursor load is a pure load. A missing row is `CursorLoad`.
- The ready timeout bounds each database call of `Agent::new` in `build`, each changefeed page among them. A stall returns `AgentCreateTimeout` for that agent (`scenario_stalled_agent_record_returns_create_timeout`).
- Each of those calls is raced against the token. The whole `Agent::new` has no fixed bound.
- A repeated name fails with `DuplicateAgentName` before any database call or spawn.

### Startup failures

- Every listen-loop await before the ready signal (connection, cursor load, LIVE subscribe, first page query) is raced against the loop's token. So a startup database call that never completes does not stop `Coalition::new` from returning `ReadyTimeout` (`scenario_stalled_startup_returns_ready_timeout`).
- For names that pass validation, a token cancelled before every loop is ready returns `Error::Cancelled { agent }`. The exception is that the waits see another sent error or `ListenLoopDropped`.
- `agent` is `None` for a cancel that `build` finds before any database call (`scenario_cancelled_parent_makes_no_db_call`).
- `agent` is `None` for a cancel found after the readiness waits end in `Ok` or `ReadyTimeout` (`scenario_cancel_after_ready_before_start`).
- `agent` is `Some(name)` for a cancel that interrupts that agent's `Agent::new` or loop startup (`scenario_cancel_during_first_page`).
- A loop sends `Cancelled` on its ready channel before any cleanup await.
- `ListenLoopDropped` means only that a loop ended without a ready signal and without a cancel.
- The readiness waits start after the last `Agent::new` call. The first error a loop sent that those waits see (including `Cancelled`) ends them. That error is returned after the teardown, in preference to `ReadyTimeout` and `ListenLoopDropped`. Among those two, the one for the earliest name wins.
- If `Coalition::new` fails after spawning, it cancels and waits for every spawned loop before it returns (`scenario_build_failure_teardown`).
- Dropping the `Coalition::new` future cancels every spawned loop and the retention sweep without waiting (`scenario_dropped_new_cancels_loops`). This uses a `DropGuard` on the root token, disarmed just before `Ok`. A loop whose ready send fails ends.

### Subscribe at a cancel

- A subscribe in flight at a cancel moves to a spawned task that no `TaskTracker` tracks. Neither the loop nor `shutdown()` waits for it, and the live count can settle after they return.
- When the reply arrives, that task sends a `KILL` of the live query.
- It logs these cases at warn: a failed subscribe, a failed or timed-out `KILL` call, a reply still missing after `LIVE_KILL_TIMEOUT`, and its own drop before it finishes. After the missing-reply warn, it keeps waiting.
- The SDK drops the result of a `KILL` statement (`surrealdb-3.3.5/src/method/query.rs:429`).

### Catch-up cursor

- `cursor:<agent>` holds `versionstamp` and `change_index` (default 0). A row that is not an object with numbers that are non-negative after `Number::to_int` fails with `CursorLoad`.
- Each bus send and each page query is raced against the agent's cancel token. The LIVE wake stream is still read while a send waits.
- The cursor advances past a message addressed to the agent only once its delivery is on the bus.
- The cursor is persisted after each page and on a stop. The in-memory cursor moves only after the save succeeds.
- A `SHOW CHANGES` reply that is not an array fails the catch-up with `CatchUp`.
- Each `update` op of a message record for the agent is one delivery, so a message record updated after creation is delivered again.
- For message records that are only created, a stop whose save succeeds puts no message on the bus twice.
- After a failed save or a process exit mid-page, the messages sent since the last saved position are put on the bus again.
- Deliveries already on the bus are not replayed. If the process exits, or every receiver is dropped, before a consumer reads them, they are lost.
- `close()` on any receiver stops further sends, and the queued deliveries stay readable.

Malformed data is one rule: log at error level and skip. The rule covers these cases:

- A changeset that is not an object or has no numeric versionstamp. It has no position, so the cursor passes it only by moving to a later changeset.
- A missing or non-array `changes`.
- A change that is not an object, or whose op key is not `update`, `delete` or `define_table`.
- A record that is not an object.
- A record whose `out` is missing or not a record id.
- A message whose payload does not decode as `T`.

`delete` and `define_table` ops, and messages for other agents, are skipped without a log.

Integration scenarios `scenario_startup_backlog`, `scenario_full_bus_shutdown_and_replay`, `scenario_stop_mid_changeset`, `scenario_cancel_during_first_page`, `scenario_drop_cancels`, `scenario_malformed_cursor`, and `scenario_wake_during_pass` lock this in.

### Production top-level shutdown

Production shutdown lives in `examples/`, not in `main.rs`, which is the minimal `ctrl_c` demo. Bare `ctrl_c` misses three requirements:

- Handle **SIGTERM**, not only SIGINT.
- Race startup readiness against sdb-task failure and a timeout. Otherwise `wait_until_ready` hangs.
- Bound the drain. The **order is `coalition.shutdown()` first, then cancel the sdb token**. Agents then persist cursors and KILL the live queries of their subscribed streams while the DB is still up. This order does not cover the `KILL` of a subscribe in flight at the cancel.

`examples/production_shutdown.rs` is hand-rolled (zero-dep, single subsystem). `examples/graceful_shutdown.rs` uses `tokio-graceful-shutdown` (multi-subsystem supervision + failure propagation). A coalition built with `Coalition::new` has a token independent of the daemon root. Call `coalition.shutdown()` to wait for its drain, because dropping it only cancels.

## Conventions

- Library code returns a typed **`thiserror`** enum: `crate::error::Error` / `crate::error::Result`.
- `anyhow` is used **only** in `src/main.rs` and `tests/`. Its `?` absorbs `Error` via the blanket `From`.
- No `unwrap()` in library code.
- `.expect()` appears at fatal startup points: `settings::SETTINGS`, `sdb::client`, `sdb_task`.
- `.expect("invariant: …")` is the other allowed form. Use it where a precondition makes the other branch unreachable. State that precondition in the message.
- Workflow: commit **direct to `main`** (no PR gate). Track TODOs in `TODO.md`.
