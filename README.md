# surrealdb-live-message

A light message layer for multi-agent interaction, built on SurrealDB live queries. Messages are first-class graph edges: agents send to one another by establishing a `RELATE` relation, and each agent subscribes via `LIVE SELECT` to the edges targeting it.

As of the 2026-04 library-first refactor, the library exposes a `Coalition<T: SurrealValue>` surface rather than a `tokio-graceful-shutdown` subsystem. Callers wire their own top-level shutdown (bare `tokio::signal::ctrl_c`, parent token, or wrap in their preferred framework); the library hands back a `CancellationToken` and a three-step `shutdown().await`.

## Architecture

- **`Message<T: SurrealValue>`** — payload-generic edge record. `T` is the caller's typed payload.
- **`Agent::new(name)`** — validates `name` (non-empty, ASCII alphanumeric or `_`; rejects with `Error::InvalidAgentName`) before creating the `agent` record.
- **`Agent::send<T>(to, payload)`** — issues a typed `RELATE $from -> message -> $to CONTENT { ... }`. Rejects unknown recipients (`Error::UnknownRecipient`) instead of creating a dangling `out` edge.
- **`Agent::listen_loop<T>(token, ready_tx)`** — `LIVE SELECT *, in, out FROM message WHERE out = $owner` (owner bound as a parameter, never string-interpolated), drives `Notification<Message<T>>` until `token.cancelled()`.
- **`Coalition<T>`** — registry + `TaskTracker` + root `CancellationToken` + per-spawn `child_token()`. `new()` performs a oneshot readiness handshake with every listen loop before returning, so the first `Agent::send` after `Coalition::new()` is guaranteed to be observed.
- **`sdb_task(token)`** — SurrealDB container/connection lifecycle as a plain async task.

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

- **Library-first async lifecycle** (`rust-v2:async-lifecycle`) — expose `CancellationToken` + `TaskTracker`, not `SubsystemHandle`; let callers wire their top-level shutdown. Readiness handshake in `Coalition::new` for subscription-registering spawns. `DropGuard` in the integration test for panic-safe container teardown.
- **`#[surreal(rename)]` for raw-identifier fields** (`surrealdb:repository-patterns`) — the `SurrealValue` derive ignores `#[serde(rename)]`. Fields like `r#in` must carry `#[surreal(rename = "in")]` or round-trip as `None`.
- **Explicit edge-pointer projection in LIVE SELECT** (`surrealdb:live-queries`) — `LIVE SELECT *` on `RELATE`-created edges omits `in`/`out` from notifications. Must be `LIVE SELECT *, in, out FROM message WHERE ...`.
- **Sync/async delivery bus** — each agent forwards received live-query messages onto a shared [kanal](https://github.com/fereidani/kanal) MPMC channel as `Delivery<T> { recipient, message }`. Consume with `coalition.inbox()`; clone for multiple workers, or bridge to a synchronous agent handler via kanal's `to_sync()` / `as_sync()` (see `examples/worker_pool.rs`). This is the seam a framework hangs agent logic off of.
- **Validate at the boundary, parameterize at the query** (`surrealdb:graph-operations`) — agent names are validated in `Agent::new` (`InvalidAgentName`) and bound into the LIVE query as `$owner` rather than interpolated, so a name can never alter the SQL. `Agent::send` checks the recipient exists (`UnknownRecipient`) instead of writing a dangling edge.

## Documentation

For detailed documentation on SurrealDB, visit [SurrealDB's Documentation](https://surrealdb.com/docs).

## Contributors

- [@tsondru](https://github.com/tsondru) — original design, daemon-shape POC, SurrealDB integration.
- Claude Opus 4.7 (1M context, via [Claude Code](https://claude.com/claude-code)) — library-first refactor (2026-04): `Coalition<T>`, generic `Message<T>`, readiness handshake, panic-safe test via `DropGuard`, and the three SDK-gotcha diagnoses (subscription race, `#[surreal(rename)]`, edge-pointer projection).

## Acknowledgments

- [SurrealDB](https://github.com/surrealdb/surrealdb) — the graph + live-query database underneath.
- [tokio-util](https://docs.rs/tokio-util) — `CancellationToken`, `TaskTracker`, `DropGuard`.
- [tokio-graceful-shutdown](https://github.com/Finomnis/tokio-graceful-shutdown) — the prior subsystem framework this repo used before the 2026-04 library-first refactor; excellent for binary-level shutdown, intentionally dropped at the library level.
