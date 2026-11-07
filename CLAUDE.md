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

1. A field named `r#in` decodes from the wire key `in` with no attribute, because the `SurrealValue` derive strips `r#` (`surrealdb-types-derive-4.0.0/src/lib.rs:27-29`). `Message` has no `#[surreal(rename)]` on `r#in` (`src/message.rs:22`). `scenario_bare_select_edge_pointers` decodes it over WS.
2. A bare `SELECT *` on a `message` edge returns `in` and `out` over WS (`scenario_bare_select_edge_pointers`, `tests/integration_test.rs:284`). The wake-up subscription reads only `id`: `LIVE SELECT id FROM message WHERE out = $owner` (`src/subsystems/agents.rs:1692`). The durable bus reads `in` and `out` from `SHOW CHANGES` changeset records.
3. Payload types must derive `SurrealValue`. The bound is `Message<T: SurrealValue>`, not only `Serialize`.

## Lifecycle / shutdown contract

### Coalition ownership

- `Coalition` owns a root `CancellationToken`, which is a child of the caller's token with `Coalition::new_with_parent`.
- It spawns listen-loops on `child_token()` and exposes `cancellation_token()` + `shutdown().await`.
- Dropping a `Coalition` cancels its root token without waiting.
- `shutdown().await` cancels the root token and waits for the listen loops and the retention sweep to end. After the cancel, a loop makes at most one cursor save. That save waits up to `CURSOR_SAVE_TIMEOUT`. The end of its live query waits up to `LIVE_KILL_TIMEOUT`. If that end is not confirmed, one fallback `KILL` and one more `INFO` read share up to `LIVE_KILL_TIMEOUT` more. So a loop stops within `CURSOR_SAVE_TIMEOUT + 2 × LIVE_KILL_TIMEOUT` after the cancel. Tests: `shutdown_is_bounded_with_a_save_in_flight`, `shutdown_is_bounded_with_a_save_on_a_stop`. The sweep's connection and `DELETE` are raced against the token.
- Only a stop runs that full end. On a reconnect (`Exit::CatchUp` or `Exit::Wake`), the end wait stops at the first failed read and sends no fallback `KILL` (`exit_end_scope_is_full_only_at_a_stop`, `short_end_live_sends_no_fallback_kill`).
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
- A new agent starts after the highest versionstamp in the whole `message` changefeed. The read goes in pages until a page holds fewer than 1000 changesets (`scenario_new_agent_skips_a_long_feed`). Another table with a changefeed in the same database does not end the read early (`scenario_other_changefeed_table`).
- A `Coalition::new` reads the feed at most once, for its first new agent (`scenario_new_agents_share_one_walk`).
- An existing agent without a cursor starts at versionstamp 0 with a warn log, so its retained backlog is delivered (`scenario_lost_cursor_replays_backlog`). If the first lookup finds the agent, there is no feed read.
- A failed changefeed read in `Agent::new` returns `AgentCreate`. So does a page that is not an array, or a full page with no integer versionstamp. A page whose highest versionstamp is below its `SINCE` or is `i64::MAX` does too.
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
- When the reply arrives, that task ends the live query as a loop does at its stop. It drops the stream, and the SDK's `Drop` sends a `KILL` on the stream's session (`surrealdb-4.0.0/src/method/live.rs:369`). It then reads `INFO FOR TABLE message` until the live query is not listed, up to `LIVE_KILL_TIMEOUT`. If the end is not confirmed by then, it sends one `KILL` of its own.
- It logs these cases at warn: a failed subscribe, a reply still missing after `LIVE_KILL_TIMEOUT`, an end not confirmed by the deadline (`LiveEndUnconfirmed`, whose `listed` field says whether a read still listed the live query), a failed or timed-out fallback `KILL` (`LiveKill`, `LiveKillTimeout`), and its own drop before it finishes. After the missing-reply warn, it keeps waiting.
- The SDK keeps the result of a `KILL` statement (`surrealdb-4.0.0/src/method/query.rs:445`). After a failed or refused fallback `KILL`, the crate reads `INFO FOR TABLE message` once more. If that read returns and does not list the live query, nothing is logged. Otherwise the crate logs `LiveKill` with the server's error.

### Catch-up cursor

- `cursor:<agent>` holds `versionstamp` and `change_index` (absent or `NONE` reads as 0). A row that is not an object with non-negative integers fails with `CursorLoad`. So does `change_index: NULL`. `integral` reads a `Number` of any variant as an integer when its value is a whole number in `i64` range. A float must also be below 2^53 (`Float`), 2^24 (`F32`) or 2^11 (`F16`) in size.
- Each bus send and each page query is raced against the agent's cancel token. The LIVE wake stream is still read while a send waits.
- The cursor advances past a message addressed to the agent only once its delivery is on the bus.
- The cursor is persisted after each page and on a stop. After a failed save call on a complete page, the in-memory cursor stays and the pass fails.
- A save that does not finish within `CURSOR_SAVE_TIMEOUT` stops waiting and is logged at error level with the position. On a complete page, the in-memory cursor still moves and the pass continues, so the page is not read again. Three saves can then write the position: the next page's save, a retry on an empty page, and one save at the loop's stop.
- A cursor save creates an absent row and writes a row at a lower position. It leaves a row at an equal or higher position (`scenario_cursor_save_moves_only_forward`).
- A stalled database can leave more than one page unsaved, so a crash can replay more than one page.
- A `SHOW CHANGES` reply that is not an array fails the catch-up with `CatchUp`.
- A full page that leaves the cursor in place fails the pass with `CatchUp`. The loop reconnects with backoff and reads the page again.
- Each `update` op of a message record for the agent is one delivery, so a message record updated after creation is delivered again.
- For message records that are only created, a stop whose save succeeds, with no earlier save failed or timed out, puts no message on the bus twice.
- Three events can repeat messages at the next start: a failed save, a timed-out save, and a process exit mid-page. The messages sent since the position in the `cursor` row go on the bus again.
- Deliveries already on the bus are not replayed. If the process exits, or every receiver is dropped, before a consumer reads them, they are lost.
- `close()` on any receiver stops further sends, and the queued deliveries stay readable.

Malformed data is one rule: log at error level and skip. The rule covers these cases:

- A changeset that is not an object, has no integer versionstamp, or has versionstamp `i64::MAX`. It has no position, so the cursor passes it only by moving to a later changeset. These changesets give one error line per page, with their count and the first of them. A page that repeats the position and count of the last logged page does not log them.
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
- Bound the drain. The **order is `coalition.shutdown()` first, then cancel the sdb token**. Agents then persist cursors and end the live queries of their subscribed streams while the DB is still up. This order does not cover the end of a subscribe in flight at the cancel.

`examples/production_shutdown.rs` is hand-rolled (zero-dep, single subsystem). `examples/graceful_shutdown.rs` uses `tokio-graceful-shutdown` (multi-subsystem supervision + failure propagation). A coalition built with `Coalition::new` has a token independent of the daemon root. Call `coalition.shutdown()` to wait for its drain, because dropping it only cancels.

## Conventions

- Library code returns a typed **`thiserror`** enum: `crate::error::Error` / `crate::error::Result`.
- `anyhow` is used **only** in `src/main.rs` and `tests/`. Its `?` absorbs `Error` via the blanket `From`.
- No `unwrap()` in library code.
- `.expect()` appears at fatal startup points: `settings::SETTINGS`, `sdb::client`, `sdb_task`.
- `.expect("invariant: …")` is the other allowed form. Use it where a precondition makes the other branch unreachable. State that precondition in the message.
- Workflow: commit **direct to `main`** (no PR gate). Track TODOs in `TODO.md`.
