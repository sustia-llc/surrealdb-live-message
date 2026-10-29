# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Breaking: `Error::Cancelled { agent: Option<String> }`. `Coalition::new`
  returns it when the coalition's token is cancelled before every listen
  loop is ready. `agent` is `None` for a `Coalition::new_with_parent`
  parent that is already cancelled (checked before any database call) and
  for a cancel found after every readiness wait ended in `Ok`,
  `Error::ReadyTimeout` or `Error::ListenLoopDropped`; it names the agent
  whose `Agent::new` call or loop startup the cancel interrupted otherwise.
  A loop sends `Error::Cancelled` before any cleanup await.
  `Error::ListenLoopDropped` now means only that a loop ended without a
  ready signal and without a cancel.

### Changed

- `Agent::new` writes the `agent` record and, when absent, its `cursor` row
  (at the changeset after the latest one retained in the changefeed) in one
  transaction; an existing cursor is kept. The listen loop only loads the
  cursor; a missing row fails startup with `Error::CursorLoad`.
- `Coalition::new` returns the first error a listen loop sends (including
  `Error::Cancelled`) as soon as it arrives, in preference to
  `Error::ReadyTimeout` and `Error::ListenLoopDropped`; among those two, the
  one for the earliest name is returned.
- A subscribe still in flight `LIVE_KILL_TIMEOUT` after a cancel, and a
  subscribe that fails after a cancel, are logged at warn with the agent.

### Fixed

- A failed `Coalition::new` keeps backlog deliveries: each listen
  loop holds at its ready point until `Coalition::new` has every ready
  signal and sends a start signal, so no delivery reaches the bus before
  `Coalition::new` returns `Ok`, and a failed `Coalition::new` moves no
  cursor past a message that was not on the bus. A failed or cancelled
  `Coalition::new` never leaves an agent record without its cursor. A
  backlog larger than the bus does not block `Coalition::new`.
- Every listen-loop await before the ready signal (cursor load, LIVE
  subscribe, first page query) is raced against the loop's token, and each
  `Agent::new` call in `Coalition::new` is bounded by the ready timeout
  (`Error::ReadyTimeout` for that agent) and raced against the token, so a
  startup database call that never completes does not stop
  `Coalition::new` returning `Error::ReadyTimeout` or a startup error.
- Dropping the `Coalition::new` future cancels every spawned listen loop and
  the retention sweep without waiting for them; a loop whose ready send
  fails ends.

## [0.4.1] - 2026-10-29

### Changed

- Bump `surrealdb`/`surrealdb-types` to 3.3.5 and the integration-test Docker
  image tag (`sdb.tag` in `config/default.toml`) to `v3.3.5`.

## [0.4.0] - 2026-10-29

### Changed

- Breaking: the delivery bus is `async_channel::bounded(256)`.
  `Coalition::inbox` returns `async_channel::Receiver<Delivery<T>>` in place
  of `kanal::AsyncReceiver<Delivery<T>>`; `Receiver::recv_blocking` replaces
  `to_sync()`. After the last sender drops, `recv` returns the queued
  deliveries and then `Err(RecvError)`.
- Remove the `kanal` git dependency; add `async-channel` 2.5.
- Dropping a `Coalition` cancels its root token without waiting;
  `shutdown().await` cancels it and waits for the listen loops and the
  retention sweep to end.

### Added

- `Coalition::new_with_parent`, whose root token is a child of a caller's
  token.
- `Error::DuplicateAgentName`.

### Fixed

- `Coalition::new` returns when an agent's durable backlog exceeds the bus
  capacity: each listen loop signals readiness after its first LIVE
  subscribe and its first catch-up page query succeed, before any bus send.
  An error before then is logged with its source chain and returned by
  `Coalition::new` as that error (for example `Error::CatchUp`,
  `Error::CursorLoad`, `Error::LiveQuery`); `Error::ListenLoopDropped` is
  returned only for a listen loop that ends without signalling. When several
  listen loops fail, `Coalition::new` returns an error that a loop sent in
  preference to `Error::ReadyTimeout` or `Error::ListenLoopDropped`. If an agent
  name fails after earlier agents' listen loops were spawned,
  `Coalition::new` cancels those loops and waits for them before returning
  the error.
- Wakes on the LIVE stream are read while a bus send waits on a full bus,
  and every wake read before or during a catch-up pass is served by one
  further pass.
- `Coalition::shutdown` returns when the bus is full and nothing consumes it:
  each bus send and each catch-up page query is raced against the
  cancellation token.
- The persisted cursor passes a message addressed to the agent only once it
  is on the bus. Malformed data follows one rule, log at error level and
  skip: a changeset that is not an object or has no integer versionstamp
  (it has no position; the cursor passes it only by moving to a later
  changeset), a missing or non-array `changes`, a change that is not an
  object or whose op key is not `update`, `delete` or `define_table`, a
  record that is not an object, a record whose `out` is missing or not a
  record id, and a message whose payload does not decode as `T`. `delete`
  and `define_table` ops and messages for other agents are skipped without a
  log. A `SHOW CHANGES` reply that is not an array fails the catch-up with
  `Error::CatchUp`. The `cursor` table gains `change_index`
  (default 0): a catch-up stopped by cancellation, or by a closed bus, saves
  the cursor at the first change not on the bus, and the next catch-up
  resumes there. Each `update` op of a message record for the agent is one
  delivery, so a message record updated after creation is delivered again. If that save fails, the error is logged, the
  loop still stops, and the persisted cursor stays at its previous saved
  position. The cursor is persisted once per catch-up page, so a process exit
  mid-page puts that page's already-sent messages on the bus again.
  Deliveries already on the bus are not replayed and are lost if the process
  exits, or every receiver is dropped, before a consumer reads them.
  `close()` on any receiver stops further sends, and the queued deliveries
  stay readable.
- A `cursor` row whose `versionstamp` or `change_index` is not a
  non-negative integer, or whose `change_index` does not fit `usize`, fails
  with `Error::CursorLoad` instead of being treated as absent.
- `Coalition::new` rejects a name that appears more than once with
  `Error::DuplicateAgentName`, before any agent is created or listen loop
  spawned.

## [0.3.0] - 2026-10-28

### Changed

- Bump `surrealdb`/`surrealdb-types` to 3.3.4 and the integration-test Docker
  image tag (`sdb.tag` in `config/default.toml`) to `v3.3.4`. Pin the other
  dependencies to their current patch versions. Bump the toolchain to 1.99.1.
- Breaking: every `Error` variant that holds a `surrealdb::Error` now holds a
  `Box<surrealdb::Error>`. `Connect`, `UseNsDb`, `Auth` and `Schema` box the
  tuple field. `AgentCreate`, `Send`, `LiveQuery`, `Stream`, `CursorLoad`,
  `CursorSave` and `CatchUp` box their `source` field.

### Added

- `Error::LiveQueryIdMissing`, `Error::LiveKill` and `Error::LiveKillTimeout`.
- `error::chain`, which joins an error and its `source()` chain with `": "`.
  The warn and error logs in `listen_loop`, the retention sweep, `main.rs`
  and `examples/messaging.rs` use it, so they show the inner SurrealDB error.
- All seven examples return the startup failure of `sdb_task` as the source
  of the `anyhow` error, so `{:#}` shows the inner error.

### Fixed

- `listen_loop` sends `KILL` for its wake-up live query through the signed-in
  connection before it drops the stream, with a 2-second limit
  (`LIVE_KILL_TIMEOUT`). The SDK's own drop sends `KILL` on a session that
  never signed in, so the server refused it and the live query stayed
  registered (surrealdb/surrealdb#7625). The SDK still logs
  `Failed to kill live query` once per stream.

## [0.2.2] - 2026-08-14

### Changed

- Bump `surrealdb`/`surrealdb-types` to 3.2.4 and the test-harness Docker image
  tag (`sdb.tag` in `config/default.toml`) to `v3.2.4`. No source changes were
  needed — `cargo clippy --all-targets -- -W clippy::pedantic` is clean of new
  diagnostics and both `--lib` and the Docker-backed `integration_test` pass
  against the v3.2.4 container.

### Fixed

- Docs: the "edge records omit `in`/`out`" gotcha in `CLAUDE.md` and
  `src/message.rs` still described the pre-two-tier subscription shape
  (`LIVE SELECT *, in, out`). The wake-up subscription has been
  `LIVE SELECT id FROM message WHERE out = $owner` since 0.2.0 —
  `SHOW CHANGES` changesets carry `id`/`in`/`out` natively. The gotcha is
  re-pointed at the plain `SELECT` sites in `tests/integration_test.rs`,
  where it is still load-bearing. (`README.md` was already correct.)

## [0.2.1] - 2026-07-15

### Changed

- Bump `surrealdb`/`surrealdb-types` to 3.2.1 and `kanal` to rev `89a1a3c`
  (commit `4052401`, 2026-07-11 — tagged here so downstream git-tag consumers
  such as koalisi can pin the 3.2.1-aligned stack).
- `Cargo.toml` `version` field now tracks releases (it had stayed at `0.1.0`
  through the `v0.2.0` tag; the git tag is the authoritative pin either way).

## [0.2.0] - 2026-06-24

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
