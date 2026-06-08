# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

`surrealdb_live_message` is a **library-first** crate (`src/lib.rs`) demonstrating agent-to-agent
messaging over SurrealDB graph edges + LIVE queries, with `src/main.rs` as a binary example.

## Build & Test

- Integration tests require **Docker** — `sdb_server.rs` spins up a SurrealDB container via `bollard`.
- Run: `cargo test --test integration_test` (the only test; no unit tests).
- Against the production cloud endpoint instead of a local container: `RUN_MODE=production cargo test --test integration_test`.
- Config layers (via the `config` crate): `config/default.toml` → `config/{RUN_MODE}.toml` → env vars (`__` separator). `RUN_MODE` defaults to `development`.

## SurrealDB SDK gotchas (do not regress)

These were diagnosed fixes; tests lock them in. See docstrings in `src/message.rs` and `src/subsystems/agents.rs`.

1. **Edge fields need `#[surreal(rename = "in")]`.** `SurrealValue` derive uses raw Rust identifiers as wire keys, so `r#in` would serialize as `"r#in"`. SurrealDB emits `"in"` for edge source. **serde `rename` is ignored** — must use the `#[surreal(...)]` attribute. (`src/message.rs:25`)
2. **`LIVE SELECT *` on edge records omits `in`/`out`.** The subscription must be `LIVE SELECT *, in, out FROM message WHERE ...` (`src/subsystems/agents.rs:95`). `tests/integration_test.rs:127` asserts both pointers deserialize.
3. Payload types must derive `SurrealValue` — `Message<T: SurrealValue>`, not just `Serialize`.

## Lifecycle / shutdown contract

- The library **never self-cancels**. `Coalition` owns a root `CancellationToken`, spawns listen-loops on `child_token()`, and exposes `cancellation_token()` + `shutdown().await`. Callers wire top-level shutdown (e.g. bare `tokio::signal::ctrl_c`) outside the library. Don't add signal handling or `tokio-graceful-shutdown` inside the lib.
- `Coalition::new` performs a **readiness handshake**: it awaits every listen-loop's LIVE-query registration before returning, so the first `Agent::send` is guaranteed observed. A listen-loop panicking before it signals ready would hang `new` — preserve the ready-signal path.

## Conventions

- Error handling uses **`anyhow`** (`anyhow::Result`, `anyhow::anyhow!`, `?`) — not `thiserror`. No `unwrap()` in library code; `.expect()` only at fatal startup points.
- Workflow: commit **direct to `main`** (no PR gate). Track TODOs in `TODO.md`.
