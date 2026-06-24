# surrealdb-live-message

A light message layer for multi-agent interaction, built on SurrealDB. Messages are first-class graph edges: agents send to one another by establishing a `RELATE` relation.

Delivery is a **two-tier durable bus**: the `message` table is a `CHANGEFEED`-backed durable log, a `LIVE SELECT` is a low-latency *wake-up*, and the sole delivery path is a `SHOW CHANGES … SINCE <cursor>` catch-up that advances a **persisted per-agent versionstamp cursor**. Delivery is therefore **at-least-once and survives disconnects and restarts** — a message published while an agent is offline is replayed on reconnect, where plain `LIVE` alone is at-most-once with no replay. (See the SurrealDB `live-queries` skill, "Delivery guarantees".) Consumers should treat handling as idempotent.

As of the 2026-04 library-first refactor, the library exposes a `Coalition<T: SurrealValue>` surface rather than a `tokio-graceful-shutdown` subsystem. Callers wire their own top-level shutdown (bare `tokio::signal::ctrl_c`, parent token, or wrap in their preferred framework); the library hands back a `CancellationToken` and a three-step `shutdown().await`.

## Architecture

- **`Message<T: SurrealValue>`** — payload-generic edge record. `T` is the caller's typed payload; `id` is populated on delivery (from the changefeed record) so consumers can identify/deduplicate under the at-least-once guarantee.
- **`Agent::new(name)`** — validates `name` (non-empty, ASCII alphanumeric or `_`; rejects with `Error::InvalidAgentName`), then **reuses an existing `agent` record or creates one** (restart-idempotent, so a restarted coalition resumes its durable cursors).
- **`Agent::send<T>(to, payload)`** — issues a typed `RELATE $from -> message -> $to CONTENT { ... }`. Rejects unknown recipients (`Error::UnknownRecipient`) instead of creating a dangling `out` edge.
- **`Agent::listen_loop<T>(token, ready_tx)`** — the two-tier durable bus. A `LIVE SELECT … WHERE out = $owner` (owner bound as a parameter, never string-interpolated) acts as a wake-up; each wake (and each **reconnect**, with capped exponential backoff) runs a `SHOW CHANGES FOR TABLE message SINCE <cursor>` catch-up that delivers every message addressed to the agent and advances + persists its versionstamp cursor (`cursor:<agent>`). Runs until `token.cancelled()`; messages are **never deleted here**.
- **`Coalition<T>`** — registry + `TaskTracker` + root `CancellationToken` + per-spawn `child_token()`. `new()` performs a oneshot readiness handshake with every listen loop before returning, so the first `Agent::send` after `Coalition::new()` is guaranteed to be observed. It also spawns one **retention sweep** task that ages out the durable log (`DELETE message WHERE created < now - sdb.message_retention_secs`, default 24h).
- **`sdb_task(token)`** — SurrealDB container/connection lifecycle as a plain async task. Defines the schema, including the `message` `CHANGEFEED` window and the `cursor` table.

See the integration test for an end-to-end library-first usage.

## Requirements

- [Rust](https://www.rust-lang.org/tools/install)
- [Docker](https://docs.docker.com/get-docker/)
- [SurrealDB](https://surrealdb.com/docs/surrealdb/installation/) (for client SQL queries, see Run below)

## Test

```sh
# run the integration test (spins up a local SurrealDB container,
# exercises Coalition<ChatMessage> with 2 agents and asserts typed
# Message<ChatMessage> edges end-to-end, then drains cleanly)
cargo test --test integration_test

# or run in production mode with your surrealdb cloud endpoint:
# config/production.toml:
cat << EOF > config/production.toml
environment = "production"

[logger]
level = "info"

[sdb]
username = "test"
password = "test"
namespace = "test"
database = "test"
endpoint = "wss://<your surrealdb cloud endpoint>"
EOF

RUN_MODE=production cargo test --test integration_test
```

## Run

### Terminal 1

```sh
# start the daemon — spawns sdb_task + Coalition<DaemonPayload> with
# alice + bob, waits for ctrl-c, then drains cleanly
cargo run
```

### Terminal 2

```sh
# start surrealdb client
surreal sql --user root --pass root --namespace test --database test

# send a typed message from bob to alice
RELATE agent:bob->message->agent:alice
    CONTENT {
        created: time::now(),
        payload: { kind: "Text", body: "Hello, Alice!" },
    };

# and back the other way
RELATE agent:alice->message->agent:bob
    CONTENT {
        created: time::now(),
        payload: { kind: "Text", body: "Hello, Bob!" },
    };

# inspect — note the *, in, out projection is required for edge records
SELECT *, in, out FROM message;
```

## Examples

`cargo run` (above) is the minimal demo — bare `ctrl_c`. Six runnable examples
live in `examples/` (each needs Docker; each spins up and tears down its own
SurrealDB container).

### Messaging

These exercise the `Coalition<T>` / `inbox()` surface and self-terminate — run
each to completion:

```sh
# Request/response round-trip: alice → bob → alice, both legs observed off
# inbox(). The sender name rides in the payload so the recipient can reply
# without parsing the edge's `in` RecordId.
cargo run --example messaging

# N-agent fan-out: hub broadcasts to four workers; the consumer asserts each
# recipient received exactly once (kanal MPMC under N producers).
cargo run --example fanout

# MPMC worker pool: M async workers clone inbox() and compete for a burst,
# plus a blocking handler bridged via inbox().to_sync() on spawn_blocking.
cargo run --example worker_pool
```

### Lifecycle / shutdown

Three examples drive the library's `CancellationToken` + `TaskTracker` surface
for production-grade top-level cancellation. All handle **SIGINT *and*
SIGTERM** (the signal `docker stop`/Kubernetes/systemd send), and drain the
coalition before stopping the database container. Stop each with `Ctrl-C` or
`kill -TERM <pid>`.

```sh
# Hand-rolled tokio driver — zero extra dependencies. Races DB readiness
# against startup failure/timeout and bounds the drain so it can't hang.
cargo run --example production_shutdown

# Same guarantees via the tokio-graceful-shutdown framework (dev-dependency,
# binary-only — never pulled into the library).
cargo run --example graceful_shutdown

# Pattern 2: drive coalition.shutdown() from a parent CancellationToken; the
# sdb token is kept independent so the DB outlives the coalition drain.
cargo run --example parent_token
```

## Patterns Demonstrated

This repo is the reference implementation for several transferable patterns, each documented in a corresponding cc-polymath skill:

- **Two-tier durable message bus** (`surrealdb:live-queries`, "Delivery guarantees") — `LIVE` is at-most-once with no replay, so it is used only as a low-latency wake-up over a durable, `CHANGEFEED`-backed log. The sole delivery path is `SHOW CHANGES … SINCE <cursor>` catch-up, advancing a persisted per-agent versionstamp cursor (`cursor = max_seen + 1`, so messages are never re-read — no dedup set). The listen loop reconnects with backoff and re-drains on every (re)connect; a per-coalition retention sweep ages out the log. Result: at-least-once delivery that survives disconnects and restarts. (`sequences` skill covers the cursor-as-high-water-mark variant.)
- **Library-first async lifecycle** (`rust-v2:async-lifecycle`) — expose `CancellationToken` + `TaskTracker`, not `SubsystemHandle`; let callers wire their top-level shutdown. Readiness handshake in `Coalition::new` for subscription-registering spawns. `DropGuard` in the integration test for panic-safe container teardown.
- **`#[surreal(rename)]` for raw-identifier fields** (`surrealdb:repository-patterns`) — the `SurrealValue` derive ignores `#[serde(rename)]`. Fields like `r#in` must carry `#[surreal(rename = "in")]` or round-trip as `None`.
- **Explicit edge-pointer projection on edge records** (`surrealdb:live-queries`) — a bare `SELECT *` / `LIVE SELECT *` on `RELATE`-created edges omits `in`/`out`; read them with `SELECT *, in, out FROM message ...` (as the integration test asserts). The durable-bus delivery path sidesteps this — `SHOW CHANGES` changeset records carry `id`/`in`/`out` natively — so the wake-up subscription only needs `LIVE SELECT id`.
- **Sync/async delivery bus** — each agent forwards durable-log messages (delivered via catch-up) onto a shared [kanal](https://github.com/fereidani/kanal) MPMC channel as `Delivery<T> { recipient, message }`. Consume with `coalition.inbox()`; clone for multiple workers, or bridge to a synchronous agent handler via kanal's `to_sync()` / `as_sync()` (see `examples/worker_pool.rs`). This is the seam a framework hangs agent logic off of.
- **Validate at the boundary, parameterize at the query** (`surrealdb:graph-operations`) — agent names are validated in `Agent::new` (`InvalidAgentName`) and bound into the LIVE query as `$owner` rather than interpolated, so a name can never alter the SQL. `Agent::send` checks the recipient exists (`UnknownRecipient`) instead of writing a dangling edge.

## Documentation

For detailed documentation on SurrealDB, visit [SurrealDB's Documentation](https://surrealdb.com/docs).

## Contributors

- [@tsondru](https://github.com/tsondru) — original design, daemon-shape POC, SurrealDB integration.
- Claude Opus 4.7 (1M context, via [Claude Code](https://claude.com/claude-code)) — library-first refactor (2026-04): `Coalition<T>`, generic `Message<T>`, readiness handshake, panic-safe test via `DropGuard`, and the three SDK-gotcha diagnoses (subscription race, `#[surreal(rename)]`, edge-pointer projection).
- Claude Opus 4.8 (1M context, via [Claude Code](https://claude.com/claude-code)) — two-tier durable bus (2026-06): `CHANGEFEED` + `SHOW CHANGES` catch-up over a persisted per-agent versionstamp cursor, LIVE-as-wake-up, reconnect/backoff, retention sweep, restart-idempotent `Agent::new`, restart-replay integration test; surrealdb 3.1.4 → 3.1.5.

## Acknowledgments

- [SurrealDB](https://github.com/surrealdb/surrealdb) — the graph + live-query database underneath.
- [tokio-util](https://docs.rs/tokio-util) — `CancellationToken`, `TaskTracker`, `DropGuard`.
- [tokio-graceful-shutdown](https://github.com/Finomnis/tokio-graceful-shutdown) — the prior subsystem framework this repo used before the 2026-04 library-first refactor; excellent for binary-level shutdown, intentionally dropped at the library level.
