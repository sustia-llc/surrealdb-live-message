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

1. **Edge fields need `#[surreal(rename = "in")]`.** `SurrealValue` derive uses raw Rust identifiers as wire keys, so `r#in` would serialize as `"r#in"`. SurrealDB emits `"in"` for edge source. **serde `rename` is ignored** — must use the `#[surreal(...)]` attribute. (`src/message.rs:25`)
2. **`LIVE SELECT *` on edge records omits `in`/`out`.** The subscription must be `LIVE SELECT *, in, out FROM message WHERE ...` (`src/subsystems/agents.rs:95`). `tests/integration_test.rs:127` asserts both pointers deserialize.
3. Payload types must derive `SurrealValue` — `Message<T: SurrealValue>`, not just `Serialize`.

## Lifecycle / shutdown contract

- The library **never self-cancels**. `Coalition` owns a root `CancellationToken`, spawns listen-loops on `child_token()`, and exposes `cancellation_token()` + `shutdown().await`. Callers wire top-level shutdown (e.g. bare `tokio::signal::ctrl_c`) outside the library. Don't add signal handling or `tokio-graceful-shutdown` inside the lib.
- **Delivery bus:** agent `listen_loop`s forward each received live-query message onto a shared **kanal MPMC** channel as `Delivery<T> { recipient, message }`. Consume via `coalition.inbox()` — clone for multiple workers; bridge to sync handlers with kanal's `as_sync()`. Bus is `bounded(INBOX_CAPACITY=256)` (backpressure) and closes (`recv()` → `Err`) once all agents shut down. kanal is a pinned git dep (`rev`), not a crates.io release.
- `Coalition::new` performs a **readiness handshake**: it awaits every listen-loop's LIVE-query registration (bounded by `READY_TIMEOUT`, cancels the root token on failure) before returning, so the first `Agent::send` is guaranteed observed. Preserve the ready-signal path.
- **Production top-level shutdown** lives in `examples/` (not `main.rs`, which is the minimal `ctrl_c` demo). Three requirements bare `ctrl_c` misses: handle **SIGTERM** (not just SIGINT), race startup readiness against sdb-task failure + a timeout (else `wait_until_ready` hangs), and bound the drain — **order: `coalition.shutdown()` first, then cancel the sdb token** (agents delete edges while the DB is still up). `examples/production_shutdown.rs` = hand-rolled (zero-dep, single subsystem); `examples/graceful_shutdown.rs` = `tokio-graceful-shutdown` (multi-subsystem supervision + failure propagation). Coalition's token is independent of the daemon root, so `coalition.shutdown()` must be called explicitly.

## Conventions

- Library code returns a typed **`thiserror`** enum — `crate::error::Error` / `crate::error::Result`. `anyhow` is used **only** in `src/main.rs` and `tests/` (its `?` absorbs `Error` via the blanket `From`). No `unwrap()` in library code; `.expect()` only at fatal startup points (`sdb::client`, `sdb_task`).
- Workflow: commit **direct to `main`** (no PR gate). Track TODOs in `TODO.md`.
