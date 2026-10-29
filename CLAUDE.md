# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

`surrealdb_live_message` is a **library-first** crate (`src/lib.rs`) demonstrating agent-to-agent
messaging over SurrealDB graph edges + LIVE queries, with `src/main.rs` as a binary example.

## Build & Test

- Integration tests require **Docker** — `sdb_server.rs` spins up a SurrealDB container via `bollard`.
- Run: `cargo test --test integration_test` (the Docker-backed integration test).
- Deterministic unit tests (no Docker) live in `src/subsystems/agents.rs` (the
  `await_ready` handshake-error seam): `cargo test --lib`.
- Against the production cloud endpoint instead of a local container: `RUN_MODE=production cargo test --test integration_test`.
- Config layers (via the `config` crate): `config/default.toml` → `config/{RUN_MODE}.toml` → env vars (`__` separator). `RUN_MODE` defaults to `development`.

## SurrealDB SDK gotchas (do not regress)

These were diagnosed fixes; tests lock them in. See docstrings in `src/message.rs` and `src/subsystems/agents.rs`.

1. **Edge fields need `#[surreal(rename = "in")]`.** `SurrealValue` derive uses raw Rust identifiers as wire keys, so `r#in` would serialize as `"r#in"`. SurrealDB emits `"in"` for edge source. **serde `rename` is ignored** — must use the `#[surreal(...)]` attribute. (`src/message.rs:34`)
2. **A bare `SELECT *` / `LIVE SELECT *` on edge records omits `in`/`out`.** Read edge pointers with an explicit projection — `SELECT *, in, out FROM message WHERE ...` (`tests/integration_test.rs:153`, `:177`, `:288`; the pointer assertions at `:167`–`:174` lock it in). The **durable bus sidesteps this**: `SHOW CHANGES` changeset records carry `id`/`in`/`out` natively, so the wake-up subscription is only `LIVE SELECT id FROM message WHERE out = $owner` (`src/subsystems/agents.rs:1111`). The gotcha still applies to any plain `SELECT` you add.
3. Payload types must derive `SurrealValue` — `Message<T: SurrealValue>`, not just `Serialize`.

## Lifecycle / shutdown contract

- `Coalition` owns a root `CancellationToken` (a child of the caller's token with `Coalition::new_with_parent`), spawns listen-loops on `child_token()`, and exposes `cancellation_token()` + `shutdown().await`. Dropping a `Coalition` cancels its root token without waiting; `shutdown().await` cancels it and waits for the listen loops and the retention sweep to end. The library installs no signal handling: callers wire top-level shutdown (e.g. bare `tokio::signal::ctrl_c`) outside the library. Don't add signal handling or `tokio-graceful-shutdown` inside the lib.
- **Delivery bus:** agent `listen_loop`s send each message their catch-up reads onto a shared **async-channel MPMC** channel as `Delivery<T> { recipient, message }`. Consume via `coalition.inbox()` (an `async_channel::Receiver`) — clone for multiple workers; sync handlers call `recv_blocking()` on a blocking thread. Bus is `async_channel::bounded(INBOX_CAPACITY=256)` (backpressure: `send` waits while full) and closes once all agents shut down: `recv()` returns the queued deliveries, then `Err(RecvError)`.
- `Coalition::new` performs a **readiness handshake**: it awaits every listen-loop's ready signal (bounded by `READY_TIMEOUT`, cancels the root token on failure) before returning, so the first `Agent::send` is guaranteed observed. Each listen-loop signals after its first LIVE subscribe and its first catch-up page query succeed, before any bus send; an error before then is logged with its source chain and sent on the ready channel (`oneshot::Sender<Result<()>>`), so `Coalition::new` returns that error; `ListenLoopDropped` means the loop ended without signalling. A repeated name fails with `DuplicateAgentName` before any database call or spawn. If `Coalition::new` fails after spawning, it cancels and waits for every spawned loop before it returns (`scenario_build_failure_teardown`). Preserve the ready-signal path.
- **Catch-up cursor:** `cursor:<agent>` holds `versionstamp` and `change_index` (default 0); a row that is not an object with non-negative integers fails with `CursorLoad`. Each bus send and each page query is raced against the agent's cancel token, and the LIVE wake stream is still read while a send waits. The cursor advances past a message addressed to the agent only once its delivery is on the bus; it is persisted after each page and on a stop, and the in-memory cursor moves only after the save succeeds. Malformed data is one rule: a changeset that is not an object or has no integer versionstamp (it has no position; the cursor passes it only by moving to a later changeset), a missing or non-array `changes`, a change that is not an object or whose op key is not `update`, `delete` or `define_table`, a record that is not an object, a record whose `out` is missing or not a record id, and a message whose payload does not decode as `T` are logged at error level and skipped (`delete` and `define_table` ops, and messages for other agents, are skipped without a log). A `SHOW CHANGES` reply that is not an array fails the catch-up with `CatchUp`. After a stop whose save succeeds, no message is put on the bus twice; after a failed save or a process exit mid-page, the messages sent since the last saved position are put on the bus again. Deliveries already on the bus are not replayed: they are lost if the process exits, or every receiver is dropped, before a consumer reads them. Integration scenarios `scenario_startup_backlog`, `scenario_full_bus_shutdown_and_replay`, `scenario_stop_mid_changeset`, `scenario_cancel_while_paging`, `scenario_drop_cancels`, `scenario_malformed_cursor`, and `scenario_wake_during_pass` lock this in.
- **Production top-level shutdown** lives in `examples/` (not `main.rs`, which is the minimal `ctrl_c` demo). Three requirements bare `ctrl_c` misses: handle **SIGTERM** (not just SIGINT), race startup readiness against sdb-task failure + a timeout (else `wait_until_ready` hangs), and bound the drain — **order: `coalition.shutdown()` first, then cancel the sdb token** (agents persist cursors and KILL live queries while the DB is still up). `examples/production_shutdown.rs` = hand-rolled (zero-dep, single subsystem); `examples/graceful_shutdown.rs` = `tokio-graceful-shutdown` (multi-subsystem supervision + failure propagation). A coalition built with `Coalition::new` has a token independent of the daemon root; call `coalition.shutdown()` to wait for its drain (dropping it only cancels).

## Conventions

- Library code returns a typed **`thiserror`** enum — `crate::error::Error` / `crate::error::Result`. `anyhow` is used **only** in `src/main.rs` and `tests/` (its `?` absorbs `Error` via the blanket `From`). No `unwrap()` in library code; `.expect()` only at fatal startup points (`settings::SETTINGS`, `sdb::client`, `sdb_task`) and as `.expect("invariant: …")` where a precondition makes the other branch unreachable and the message states it.
- Workflow: commit **direct to `main`** (no PR gate). Track TODOs in `TODO.md`.
