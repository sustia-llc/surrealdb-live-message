# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Two-tier durable message bus** — delivery is now at-least-once + idempotent
  across disconnects *and* restarts, replacing the previous at-most-once LIVE
  behaviour (a message published while an agent was disconnected was silently
  lost; see the SurrealDB `live-queries` skill, "Delivery guarantees").
  - The `message` edge table is the durable log, backed by a `CHANGEFEED`
    (window = `sdb.message_retention_secs`, default 24h).
  - `LIVE SELECT` is now a pure low-latency **wake-up**; the sole delivery path
    is a `SHOW CHANGES … SINCE <cursor>` catch-up that advances a **persisted
    per-agent versionstamp cursor** (`cursor:<agent>`), so messages are never
    re-read (no dedup set) and resume exactly across restarts.
  - The listen loop now **reconnects** with capped exponential backoff and
    re-runs catch-up on every (re)connect, instead of exiting on stream end.
  - Messages are **no longer deleted on shutdown**; the log is aged out by a
    per-coalition retention sweep (`DELETE message WHERE created < now-retention`).
  - `Agent::new` is now restart-idempotent (reuses an existing agent record) so a
    restarted coalition resumes its durable cursors.
  - `Message<T>` gains an `id: Option<RecordId>` field, populated on delivery, so
    consumers can identify/deduplicate under the at-least-once guarantee.
  - New setting `sdb.message_retention_secs` (default `86400`).

### Changed

- Bumped `surrealdb` / `surrealdb-types` 3.1.4 → 3.1.5. Notably inherits the WS
  client fix [surrealdb#7037](https://github.com/surrealdb/surrealdb/issues/7037):
  an incoming WS frame that cannot be decoded far enough to recover its request
  id now **fails all pending requests with the deserialization error** instead of
  being silently dropped (which previously left every in-flight `await` — query,
  LIVE registration, RELATE — hanging indefinitely). Relevant here because the
  dev/prod endpoints are `ws://` / `wss://` and the listen loop relies on
  long-lived LIVE streams; a malformed/incompatible server frame can no longer
  wedge an agent forever.

## [0.1.0] - 2026-06-17

### Added

- Four runnable messaging examples covering the `Coalition<T>` / `inbox()`
  surface: `examples/messaging.rs` (request/response round-trip),
  `examples/fanout.rs` (N-agent broadcast, each delivery once),
  `examples/worker_pool.rs` (M async `inbox()` clones competing + a sync handler
  bridged via `inbox().to_sync()`), and `examples/parent_token.rs` (drive
  `coalition.shutdown()` from a parent `CancellationToken`).
- `Coalition::new_with_ready_timeout(names, Duration)` — caller-chosen
  readiness-handshake timeout; `new` delegates with the default `READY_TIMEOUT`.
- `await_ready` helper extracting the per-agent handshake await, with
  deterministic DB-free unit tests for the `ReadyTimeout` and `ListenLoopDropped`
  paths (`cargo test --lib`).
- `Agent::new` validates agent names (non-empty, ASCII alphanumeric or
  underscore) → `Error::InvalidAgentName`, rejecting malformed names before any
  DB work.
- `Error::UnknownRecipient` and `Error::InvalidAgentName` variants.
- Expanded integration coverage (consolidated into the single-container test):
  unknown-recipient rejection, schema enforcement (non-`agent` endpoint /
  non-datetime `created`), handshake-race delivery, N-agent fan-out, bus
  backpressure + close semantics, and `InvalidAgentName`.
- Message-delivery bus: agent `listen_loop`s forward each received message onto
  a shared kanal MPMC channel as `Delivery<T> { recipient, message }`. Consume
  via `coalition.inbox()` (clone for multiple workers; bridge sync/async with
  kanal's `as_sync`/`as_async`). Bus closes when all agents shut down. The
  integration test now asserts the **delivery path** (notification reaching the
  agent), not just the written graph edges.
- Two production top-level-shutdown examples: `examples/production_shutdown.rs`
  (hand-rolled `tokio::signal::unix` SIGINT+SIGTERM, startup-race supervision,
  timeout-bounded drain) and `examples/graceful_shutdown.rs`
  (`tokio-graceful-shutdown` framework wrap, dev-dependency only).
- Typed public error enum `crate::error::Error` (`thiserror`) replacing `anyhow`
  in library code — callers can match on `ReadyTimeout`, `ListenLoopDropped`,
  `AgentCreate`, `Send`, `LiveQuery`, `Schema`, etc. `anyhow` stays in the binary
  example and tests.
- Explicit schema defined once at connect: `agent` SCHEMAFULL; `message`
  `TYPE RELATION IN agent OUT agent` SCHEMALESS with a typed `created` and a
  flexible generic `payload`. Idempotent via `IF NOT EXISTS`.
- `READY_TIMEOUT` (10s) bound on the `Coalition::new` readiness handshake; on
  failure the root token is cancelled so spawned listen-loops don't orphan.
- Library-first lifecycle: `Coalition<T>` exposes `cancellation_token()` and
  `shutdown().await` so downstream binaries wire their own top-level shutdown.
- Readiness handshake in `Coalition::new` — awaits every agent's LIVE-query
  registration before returning, guaranteeing the first `Agent::send` is observed.
- Generic `Message<T: SurrealValue>` edge record over user-typed payloads.
- Panic-safe integration-test teardown via `tokio-util` `DropGuard`.
- GitHub Actions CI: `fmt --check`, `clippy -D warnings`, Docker integration test.
- Pinned toolchain (`rust-toolchain.toml`, 1.96.0 + rustfmt/clippy).
- `CLAUDE.md` documenting test workflow, SDK gotchas, and the shutdown contract.

### Changed

- `Agent::send` now rejects a recipient with no `agent` record
  (`Error::UnknownRecipient`) instead of creating a `RELATE` edge with a dangling
  `out` pointer. Adds one existence-check round-trip per send.
- `docker.platform` is now `Option<String>`; unset → Docker host-native platform
  (fixes ARM hosts). Pin via `DOCKER__PLATFORM` or config. `config/default.toml`
  no longer hardcodes `linux/x86_64`.
- Bumped `surrealdb`/`surrealdb-types` to 3.1.4 (server image `tag` in
  `config/default.toml` → `v3.1.4`) and `bollard` to 0.21. The 3.1.4 release is
  server-only (no client/wire changes affecting this crate).
- `Agent::listen_loop`'s `select!` handles stream-end explicitly: `biased;` gives
  shutdown priority, and a live-stream that ends on its own now breaks the loop
  cleanly so the agent drops its bus sender instead of going silent until cancel.
- Shutdown edge-cleanup deletes by endpoint
  (`DELETE message WHERE in = $owner OR out = $owner`) — the previous name-keyed
  id never matched RELATE's random edge ids — bounded by a 2s timeout so a wedged
  DB can't hang `shutdown()`.
- The handshake-failure path now drains spawned listen-loops (`cancel → close →
  wait`) before returning the error, leaving no detached tasks.

### Security

- `Agent::listen_loop` binds the agent name as a `$owner` query parameter rather
  than string-interpolating `agent:{name}` into the LIVE SELECT, closing a
  query-injection seam. Combined with `Agent::new` name validation, malformed or
  hostile names can no longer reach the query string.

### Fixed

- Edge fields now use `#[surreal(rename = "in")]` — serde `rename` is ignored by
  the `SurrealValue` derive, so `r#in` would otherwise serialize as `"r#in"`.
- LIVE subscription projects edge pointers explicitly
  (`LIVE SELECT *, in, out FROM message ...`); plain `LIVE SELECT *` omits `in`/`out`.
- Subscription race resolved by the readiness handshake (see Added).
- Flaky `ReadyTimeout` integration scenario replaced with deterministic unit tests
  against `await_ready`: `tokio::time::timeout` polls the inner future before the
  timer, so racing a real LIVE-query registration could resolve the receiver and
  return `Ok` regardless of how small the window is (failed ~1/3 of runs).
