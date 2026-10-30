# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.6.0] - 2026-10-30

### Changed

- Breaking: `Error::LiveQueryIdMissing` gains a `source`: the failed read, or
  a message that names the value that is not a uuid.
- `Coalition::new` bounds each database call of `Agent::new`, each changefeed
  page among them, by the ready timeout. The whole `Agent::new` has no fixed
  bound. The `Error::AgentCreateTimeout` message now says "a database call".
- A subscribe in flight at a cancel moves to a spawned task. The loop and
  `Coalition::shutdown` do not wait for it. The task keeps waiting after its
  warn for a reply still missing at `LIVE_KILL_TIMEOUT`, and that warn now says
  "not yet ended".

### Fixed

- If the `RELATE` statement fails, `Agent::send` returns `Error::Send`. It
  previously returned `Ok(())` and no edge existed.
- For a new agent, the cursor starts after the highest versionstamp in the
  whole `message` changefeed. The read ends on a page with fewer than 1000
  changesets. This needs a database where `message` is the one table with a
  changefeed. It previously used the first page, at most 1000 changesets, so a
  longer feed replayed its remainder. A `Coalition::new` reads the feed at most
  once. An agent that the first lookup finds, with no cursor, reads no feed.
- `Agent::new` returns `Error::AgentCreate` for a changefeed page that is not
  an array, a full page with no numeric versionstamp, and a page whose highest
  versionstamp is below its `SINCE` or is `i64::MAX`. It writes neither record.
  A changeset with no numeric versionstamp is logged at error level.
- When the reply arrives, a subscribe that a cancel interrupted sends a `KILL`
  of its live query. A failed subscribe and a failed or timed-out `KILL` call
  are logged at warn. A reply still missing after `LIVE_KILL_TIMEOUT` is logged
  at warn, and so is a drop of that task before it finishes.
- When the wake stream cannot be opened, `subscribe_wake` sends a `KILL` of the
  live query before it returns `Error::Stream`.

## [0.5.0] - 2026-10-29

### Added

- Breaking: `Error::Cancelled { agent: Option<String> }`. If the coalition's
  token is cancelled before every listen loop is ready, `Coalition::new`
  returns it. `agent` is `None` for a `Coalition::new_with_parent` parent that
  is already cancelled, found before any database call. `agent` is also `None`
  for a cancel found after every readiness wait ended in `Ok` or
  `Error::ReadyTimeout`. Otherwise it names the agent whose `Agent::new` call
  or loop startup the cancel interrupted. A loop sends `Error::Cancelled`
  before any cleanup await. `Error::ListenLoopDropped` now means only that a
  loop ended without a ready signal and without a cancel.
- Breaking: `Error::AgentCreateTimeout { agent, timeout }`. If an `Agent::new`
  call does not finish within the ready timeout, `Coalition::new` returns it.

### Changed

- `Agent::new` writes the `agent` record and an absent `cursor` row in one
  transaction. An existing cursor is kept. For a new agent, the cursor starts
  after the highest versionstamp in the first `SHOW CHANGES` page of the
  changefeed (at most 1000 changesets). For an existing agent without a
  cursor, it starts at versionstamp 0, so the agent's retained backlog is
  delivered, and a warn is logged. The listen loop only loads the cursor. A
  missing row fails startup with `Error::CursorLoad`. A failed changefeed read
  in `Agent::new` returns `Error::AgentCreate`.
- `Coalition::new` returns the first error a listen loop sends (including
  `Error::Cancelled`) as soon as it arrives, in preference to
  `Error::ReadyTimeout` and `Error::ListenLoopDropped`. Among those two, the
  one for the earliest name is returned.
- A subscribe still in flight `LIVE_KILL_TIMEOUT` after a cancel, and a
  subscribe that fails after a cancel, are logged at warn with the agent.

### Fixed

- A failed `Coalition::new` keeps backlog deliveries. Each listen loop holds
  at its ready point. `Coalition::new` sends a start signal once it has every
  ready signal. So no delivery reaches the bus before `Coalition::new` returns
  `Ok`. A failed `Coalition::new` moves no cursor past a message that was not
  on the bus. `Agent::new` does not write an agent record without its
  cursor. A backlog larger than the bus does not block `Coalition::new`.
- Every listen-loop await before the ready signal (connection, cursor load,
  LIVE subscribe, first page query) is raced against the loop's token. The
  ready timeout bounds each `Agent::new` call in `Coalition::new`
  (`Error::AgentCreateTimeout` for that agent), and each call is raced against
  the token. A startup database call that never completes therefore does not
  stop `Coalition::new` from returning an error.
- Dropping the `Coalition::new` future cancels every spawned listen loop and
  the retention sweep without waiting for them. A loop whose ready send fails
  ends.

## [0.4.1] - 2026-10-29

### Changed

- Bump `surrealdb`/`surrealdb-types` to 3.3.5 and the integration-test Docker
  image tag (`sdb.tag` in `config/default.toml`) to `v3.3.5`.

## [0.4.0] - 2026-10-29

### Changed

- Breaking: the delivery bus is `async_channel::bounded(256)`.
  `Coalition::inbox` returns `async_channel::Receiver<Delivery<T>>` in place
  of `kanal::AsyncReceiver<Delivery<T>>`. `Receiver::recv_blocking` replaces
  `to_sync()`. After the last sender drops, `recv` returns the queued
  deliveries and then `Err(RecvError)`.
- Remove the `kanal` git dependency, and add `async-channel` 2.5.
- Dropping a `Coalition` cancels its root token without waiting.
  `shutdown().await` cancels it and waits for the listen loops and the
  retention sweep to end.

### Added

- `Coalition::new_with_parent`, whose root token is a child of a caller's
  token.
- `Error::DuplicateAgentName`.

### Fixed

- If an agent's durable backlog exceeds the bus capacity, `Coalition::new`
  still returns. Each listen loop signals readiness after its first LIVE subscribe
  and its first catch-up page query succeed, before any bus send.
  An error before then is logged with its source chain, and `Coalition::new`
  returns it as that error (for example `Error::CatchUp`, `Error::CursorLoad`,
  `Error::LiveQuery`). `Error::ListenLoopDropped` is returned only for a
  listen loop that ends without signalling. If several listen loops fail,
  `Coalition::new` returns an error that a loop sent in preference to
  `Error::ReadyTimeout` or `Error::ListenLoopDropped`. If an agent name fails
  after earlier agents' listen loops were spawned, `Coalition::new` cancels
  those loops and waits for them before it returns the error.
- Wakes on the LIVE stream are read while a bus send waits on a full bus.
  One further pass serves every wake read before or during a catch-up pass.
- If the bus is full and nothing consumes it, `Coalition::shutdown` still
  returns. Each bus send and each catch-up page query is raced against the
  cancellation token.
- The persisted cursor passes a message addressed to the agent only once it
  is on the bus.
- Malformed data follows one rule: log at error level and skip. The rule
  covers a changeset that is not an object or has no integer versionstamp.
  Such a changeset has no position, so the cursor passes it only by moving to
  a later changeset. The rule also covers a missing or non-array `changes`.
  It covers a change that is not an object or whose op key is not `update`,
  `delete` or `define_table`. It covers a record that is not an object, or
  whose `out` is missing or not a record id. It covers a message whose payload
  does not decode as `T`. `delete` and `define_table` ops and messages for other
  agents are skipped without a log.
- A `SHOW CHANGES` reply that is not an array fails the catch-up with
  `Error::CatchUp`.
- The `cursor` table gains `change_index` (default 0). A catch-up stopped by
  cancellation or by a closed bus saves the cursor at the first change not on
  the bus, and the next catch-up resumes there. Each `update` op of a message
  record for the agent is one delivery, so a message record updated after
  creation is delivered again. If that save fails, the error is logged, the
  loop still stops, and the persisted cursor stays at its previous saved
  position. The cursor is persisted once per catch-up page, so a process exit
  mid-page puts that page's already-sent messages on the bus again.
- Deliveries already on the bus are not replayed. If the process exits, or
  every receiver is dropped, before a consumer reads them, they are lost.
  `close()` on any receiver stops further sends, and the queued deliveries
  stay readable.
- A `cursor` row whose `versionstamp` or `change_index` is not a non-negative
  integer, or whose `change_index` does not fit `usize`, fails with
  `Error::CursorLoad`. It is no longer treated as absent.
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

- Bump `surrealdb`/`surrealdb-types` to 3.2.4 and the test Docker image
  tag (`sdb.tag` in `config/default.toml`) to `v3.2.4`. No source changes were
  needed. `cargo clippy --all-targets -- -W clippy::pedantic` is clean of new
  diagnostics, and both `--lib` and the Docker-backed `integration_test` pass
  against the v3.2.4 container.

### Fixed

- Docs: the "edge records omit `in`/`out`" gotcha in `CLAUDE.md` and
  `src/message.rs` still described the pre-two-tier subscription shape
  (`LIVE SELECT *, in, out`). The wake-up subscription is
  `LIVE SELECT id FROM message WHERE out = $owner` since 0.2.0, because
  `SHOW CHANGES` changesets carry `id`/`in`/`out` natively. The gotcha now
  points at the plain `SELECT` sites in `tests/integration_test.rs`, where it
  still matters. (`README.md` was already correct.)

## [0.2.1] - 2026-07-15

### Changed

- Bump `surrealdb`/`surrealdb-types` to 3.2.1 and `kanal` to rev `89a1a3c`
  (commit `4052401`, 2026-07-11). This is tagged here so downstream git-tag
  consumers such as koalisi can pin the 3.2.1-aligned stack.
- `Cargo.toml` `version` field now tracks releases. It stayed at `0.1.0`
  through the `v0.2.0` tag, and the git tag is the authoritative pin either way.

## [0.2.0] - 2026-06-24

### Added

- **Two-tier durable message bus**. Delivery is now at-least-once + idempotent
  across disconnects *and* restarts. This replaces the previous at-most-once
  LIVE behaviour. A message published while an agent was disconnected was then
  lost with no signal (see the SurrealDB `live-queries` skill, "Delivery
  guarantees").
  - The `message` edge table is the durable log, backed by a `CHANGEFEED`
    (window = `sdb.message_retention_secs`, default 24h).
  - `LIVE SELECT` is now a pure low-latency **wake-up**. The sole delivery path
    is a `SHOW CHANGES … SINCE <cursor>` catch-up that advances a **persisted
    per-agent versionstamp cursor** (`cursor:<agent>`). So messages are never
    re-read (no dedup set) and resume exactly across restarts.
  - The listen loop now **reconnects** with capped exponential backoff and
    re-runs catch-up on every (re)connect. It no longer exits on stream end.
  - Messages are **no longer deleted on shutdown**. A per-coalition retention
    sweep ages out the log (`DELETE message WHERE created < now-retention`).
  - `Agent::new` is now restart-idempotent (reuses an existing agent record), so
    a restarted coalition resumes its durable cursors.
  - `Message<T>` gains an `id: Option<RecordId>` field, populated on delivery, so
    consumers can identify/deduplicate under the at-least-once guarantee.
  - New setting `sdb.message_retention_secs` (default `86400`).

### Changed

- Bumped `surrealdb` / `surrealdb-types` 3.1.4 → 3.1.5. This inherits a WS
  client fix.
  See [surrealdb#7037](https://github.com/surrealdb/surrealdb/issues/7037). Some
  incoming WS frames cannot be decoded far enough to recover a request id. Such
  a frame now **fails all pending requests with the deserialization error**. It
  was previously dropped with no signal, which left every in-flight `await` (query,
  LIVE registration, RELATE) hanging with no limit. This matters here because
  the dev/prod endpoints are `ws://` / `wss://`, and the listen loop relies on
  long-lived LIVE streams. A malformed or incompatible server frame can no
  longer wedge an agent forever.

## [0.1.0] - 2026-06-17

### Added

- Four runnable messaging examples cover the `Coalition<T>` / `inbox()`
  surface:
  - `examples/messaging.rs`: request/response round-trip.
  - `examples/fanout.rs`: N-agent broadcast, each delivery once.
  - `examples/worker_pool.rs`: M async `inbox()` clones competing, plus a sync
    handler bridged via `inbox().to_sync()`.
  - `examples/parent_token.rs`: drive `coalition.shutdown()` from a parent
    `CancellationToken`.
- `Coalition::new_with_ready_timeout(names, Duration)`, a caller-chosen
  readiness-handshake timeout. `new` delegates with the default `READY_TIMEOUT`.
- `await_ready` helper that extracts the per-agent handshake await, with
  deterministic DB-free unit tests for the `ReadyTimeout` and `ListenLoopDropped`
  paths (`cargo test --lib`).
- `Agent::new` validates agent names (non-empty, ASCII alphanumeric or
  `_`) → `Error::InvalidAgentName`, and rejects malformed names before
  any DB work.
- `Error::UnknownRecipient` and `Error::InvalidAgentName` variants.
- Expanded integration coverage, consolidated into the single-container test:
  unknown-recipient rejection, schema enforcement (non-`agent` endpoint /
  non-datetime `created`), handshake-race delivery, N-agent fan-out, bus
  backpressure + close semantics, and `InvalidAgentName`.
- Message-delivery bus: agent `listen_loop`s forward each received message onto
  a shared kanal MPMC channel as `Delivery<T> { recipient, message }`. Consume
  via `coalition.inbox()`. Clone it for multiple workers, and bridge sync/async
  with kanal's `as_sync`/`as_async`. The bus closes after all agents shut down.
  The integration test now asserts the **delivery path** (notification reaching
  the agent), not only the written graph edges.
- Two production top-level-shutdown examples: `examples/production_shutdown.rs`
  (hand-rolled `tokio::signal::unix` SIGINT+SIGTERM, startup-race supervision,
  timeout-bounded drain) and `examples/graceful_shutdown.rs`
  (`tokio-graceful-shutdown` framework wrap, dev-dependency only).
- Typed public error enum `crate::error::Error` (`thiserror`) replaces `anyhow`
  in library code. Callers can match on `ReadyTimeout`, `ListenLoopDropped`,
  `AgentCreate`, `Send`, `LiveQuery`, `Schema`, and others. `anyhow` stays in
  the binary example and tests.
- Explicit schema defined once at connect: `agent` SCHEMAFULL, and `message`
  `TYPE RELATION IN agent OUT agent` SCHEMALESS with a typed `created` and a
  flexible generic `payload`. It is idempotent via `IF NOT EXISTS`.
- `READY_TIMEOUT` (10s) bound on the `Coalition::new` readiness handshake. On
  failure the root token is cancelled, so spawned listen-loops do not orphan.
- Library-first lifecycle: `Coalition<T>` exposes `cancellation_token()` and
  `shutdown().await`, so downstream binaries wire their own top-level shutdown.
- Readiness handshake in `Coalition::new`. It awaits every agent's LIVE-query
  registration before it returns, which guarantees that the first `Agent::send`
  is observed.
- Generic `Message<T: SurrealValue>` edge record over user-typed payloads.
- Panic-safe integration-test teardown via `tokio-util` `DropGuard`.
- GitHub Actions CI: `fmt --check`, `clippy -D warnings`, Docker integration test.
- Pinned toolchain (`rust-toolchain.toml`, 1.96.0 + rustfmt/clippy).
- `CLAUDE.md` documenting test workflow, SDK gotchas, and the shutdown contract.

### Changed

- `Agent::send` now rejects a recipient with no `agent` record
  (`Error::UnknownRecipient`) in place of a `RELATE` edge with a dangling `out`
  pointer. This adds one recipient-lookup round-trip per send.
- `docker.platform` is now `Option<String>`. Unset gives the Docker host-native
  platform, which fixes ARM hosts. Pin it via `DOCKER__PLATFORM` or config.
  `config/default.toml` no longer hardcodes `linux/x86_64`.
- Bumped `surrealdb`/`surrealdb-types` to 3.1.4 (server image `tag` in
  `config/default.toml` → `v3.1.4`) and `bollard` to 0.21. The 3.1.4 release is
  server-only, with no client or wire changes that affect this crate.
- `Agent::listen_loop`'s `select!` handles stream-end explicitly. `biased;` gives
  shutdown priority. A live-stream that ends on its own now breaks the loop
  cleanly. The agent then drops its bus sender and does not go silent until
  cancel.
- Shutdown edge-cleanup deletes by endpoint
  (`DELETE message WHERE in = $owner OR out = $owner`). The previous name-keyed
  id never matched RELATE's random edge ids. A 2s timeout bounds it, so a wedged
  DB cannot hang `shutdown()`.
- The handshake-failure path now drains spawned listen-loops (`cancel → close →
  wait`) before it returns the error, which leaves no detached tasks.

### Security

- `Agent::listen_loop` binds the agent name as a `$owner` query parameter in
  place of string-interpolating `agent:{name}` into the LIVE SELECT. This closes
  a query-injection seam. With `Agent::new` name validation, malformed or
  hostile names can no longer reach the query string.

### Fixed

- Edge fields now use `#[surreal(rename = "in")]`. The `SurrealValue` derive
  ignores serde `rename`, so `r#in` otherwise serializes as `"r#in"`.
- LIVE subscription projects edge pointers explicitly
  (`LIVE SELECT *, in, out FROM message ...`). Plain `LIVE SELECT *` omits
  `in`/`out`.
- The readiness handshake resolves the subscription race (see Added).
- Deterministic unit tests against `await_ready` replace the flaky
  `ReadyTimeout` integration scenario. `tokio::time::timeout` polls the inner
  future before the timer. So in some runs a real LIVE-query registration
  resolved the receiver and returned `Ok`, however small the window was. That
  scenario failed in about 1 of 3 runs.
