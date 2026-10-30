# surrealdb-live-message

A light message layer for multi-agent interaction, built on SurrealDB. Messages are first-class graph edges: agents send to one another by establishing a `RELATE` relation.

Delivery is a **two-tier durable bus** with three parts:

- The `message` table is a `CHANGEFEED`-backed durable log.
- A `LIVE SELECT` is a low-latency *wake-up*.
- The sole delivery path is a `SHOW CHANGES … SINCE <cursor>` catch-up. It advances a **persisted per-agent cursor** (versionstamp, change index).

The cursor passes a message addressed to the agent only after its delivery is on the in-process bus. So a message published while an agent is offline is put on the bus on reconnect. Plain `LIVE` alone is at-most-once with no replay.

Malformed data follows one rule: log at error level and skip. The rule covers these cases:

- A changeset that is not an object or has no numeric versionstamp. Such a changeset has no position, so the cursor passes it only by moving to a later changeset.
- A missing or non-array `changes`.
- A change that is not an object, or whose op key is not `update`, `delete` or `define_table`.
- A record that is not an object.
- A record whose `out` is missing or not a record id.
- A message whose payload does not decode as `T`.

`delete` and `define_table` ops and messages for other agents are skipped without a log. Each `update` op of a message record for the agent is one delivery, so a message record updated after creation is delivered again.

A stop by cancellation or by a closed bus persists the cursor at the first change not on the bus. For message records that are only created, the next run then neither skips nor repeats a message. If that save fails, the persisted cursor stays at its previous saved position, and the messages sent since then are put on the bus again.

The cursor is persisted once per catch-up page. If the process exits mid-page, that page's already-sent messages are put on the bus again after restart.

Deliveries already on the bus are **not** replayed. If the process exits, or every receiver is dropped, before a consumer reads them, they are lost. `close()` on any receiver stops further sends, and the queued deliveries stay readable. (See the SurrealDB `live-queries` skill, "Delivery guarantees".)

Since the 2026-04 library-first refactor, the library exposes a `Coalition<T: SurrealValue>` surface in place of a `tokio-graceful-shutdown` subsystem. Callers wire their own top-level shutdown: bare `tokio::signal::ctrl_c`, a parent token, or their preferred framework. The library hands back a `CancellationToken` and a three-step `shutdown().await`.

## Architecture

**`Message<T: SurrealValue>`** is a payload-generic edge record. `T` is the caller's typed payload. `id` is populated on delivery from the changefeed record, so consumers can identify and deduplicate a delivery put on the bus more than once.

**`Agent::new(name)`** works in these steps:

- It validates `name`, which must be non-empty ASCII alphanumeric or `_`. A rejected name returns `Error::InvalidAgentName`.
- It **reuses an existing `agent` record or creates one**. It is restart-idempotent, so a restarted coalition resumes its durable cursors.
- If `cursor:<name>` is absent, it creates that cursor. An existing cursor is kept.
- A new agent starts after the highest versionstamp in the whole `message` changefeed. The read goes in pages until a page holds fewer than 1000 changesets. This needs a database where `message` is the one table with a changefeed.
- An existing agent starts at versionstamp 0, so its retained backlog is delivered, with a warn log. If the first lookup finds the agent, there is no changefeed read.
- The agent record and the cursor are written in one transaction, so `Agent::new` does not write an `agent` record without its cursor.

**`Agent::send<T>(to, payload)`** issues a typed `RELATE $from -> message -> $to CONTENT { ... }`. It rejects unknown recipients with `Error::UnknownRecipient` in place of a dangling `out` edge. If the `RELATE` fails, as a call or as a statement, it returns `Error::Send`.

**`Agent::listen_loop<T>(token, ready_tx, start, inbox_tx)`** is private, and `Coalition::new` spawns it. It is the two-tier durable bus:

- A `LIVE SELECT … WHERE out = $owner` acts as a wake-up. The owner is bound as a parameter and is never string-interpolated.
- Each wake, and each **reconnect** with capped exponential backoff, runs a `SHOW CHANGES FOR TABLE message SINCE <versionstamp>` catch-up. The catch-up sends every message addressed to the agent onto `inbox_tx`. It logs malformed data at error level and skips it, as described above.
- The cursor (`cursor:<agent>`: `versionstamp`, `change_index`) is persisted after each page at the first change not yet on the bus.
- A missing `cursor:<agent>` row fails with `Error::CursorLoad`.
- `ready_tx` receives `Ok(())` after the first catch-up page query succeeds. If a startup error occurs first, it receives that error. If the token is cancelled first, it receives `Error::Cancelled` naming the agent.
- Every await before then is raced against the token, and a cancel is sent before any cleanup await.
- After `Ok(())`, the loop puts nothing on the bus until `Coalition::new` opens the `start` signal. If its ready send fails, the loop ends.
- It runs until `token.cancelled()`. Messages are **never deleted here**.

**`Coalition<T>`** holds a registry, a `TaskTracker`, a root `CancellationToken` and a per-spawn `child_token()`. Its startup and shutdown follow these rules:

- `new()` performs a oneshot readiness handshake with every listen loop before it returns, so the first `Agent::send` after `Coalition::new()` is guaranteed to be observed.
- No delivery reaches the bus before `new()` returns `Ok`. Each loop waits at its ready point for a start signal, which `new()` sends once every loop is ready.
- A failed `new()` moves no cursor past a message that was not on the bus.
- Each database call of `Agent::new`, each changefeed page among them, is bounded by the ready timeout (`Error::AgentCreateTimeout`).
- Each listen-loop startup await is raced against the token and covered by the readiness wait (`Error::ReadyTimeout`). So a startup database call that never completes does not stop `new()` from returning an error.
- The readiness waits start after the last `Agent::new` call. The first error a loop sent that those waits see (including `Error::Cancelled`) ends them. That error is returned after the teardown, in preference to `Error::ReadyTimeout` and `Error::ListenLoopDropped`.
- `Error::ListenLoopDropped` means a loop ended without a ready signal and without a cancel.
- Dropping the `new()` future cancels every spawned loop and the retention sweep without waiting for them.
- `new()` also spawns one **retention sweep** task that ages out the durable log (`DELETE message WHERE created < now - sdb.message_retention_secs`, default 24h).
- `shutdown().await` cancels the root token and waits for these tasks. Dropping a `Coalition` cancels the root token without waiting.
- `new_with_parent(names, &parent)` makes the root token a child of `parent`.

For names that pass validation, a cancel before every loop is ready returns `Error::Cancelled`, unless the waits see another sent error or `Error::ListenLoopDropped`. The `agent` field is set as follows:

- `agent: None` for a `new_with_parent` parent that is already cancelled, found before any database call.
- `agent: None` for a cancel found after every readiness wait ended in `Ok` or `Error::ReadyTimeout`.
- `agent: Some(name)` for a cancel that interrupts the `Agent::new` of that agent or the startup of its loop.

**`sdb_task(token)`** runs the SurrealDB container and connection lifecycle as a plain async task. It defines the schema, including the `message` `CHANGEFEED` window and the `cursor` table.

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

`cargo run` (above) is the minimal demo, with bare `ctrl_c`. Seven runnable
examples live in `examples/`. Each needs Docker, and each spins up and tears
down its own SurrealDB container.

### Durability

This is the headline of the two-tier durable bus. A message sent to an agent
while it is **fully shut down** is replayed on restart. Plain at-most-once LIVE
loses that message.

```sh
# Round 1: bring alice + bob up, then shut the coalition down (bob offline).
# While offline, alice sends bob a message. Round 2: restart — bob resumes from
# his persisted versionstamp cursor and the CHANGEFEED catch-up replays it.
cargo run --example durable_restart
```

### Messaging

These exercise the `Coalition<T>` / `inbox()` surface and self-terminate. Run
each to completion.

```sh
# Request/response round-trip: alice → bob → alice, both legs observed off
# inbox(). The sender name rides in the payload so the recipient can reply
# without parsing the edge's `in` RecordId.
cargo run --example messaging

# N-agent fan-out: hub broadcasts to four workers; the consumer asserts each
# recipient received exactly once (async-channel MPMC under N producers).
cargo run --example fanout

# MPMC worker pool: M async workers clone inbox() and compete for a burst,
# plus a blocking handler calling recv_blocking() on spawn_blocking.
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

- **Two-tier durable message bus** (`surrealdb:live-queries`, "Delivery guarantees"). `LIVE` is at-most-once with no replay, so it is only a low-latency wake-up over a durable, `CHANGEFEED`-backed log. The sole delivery path is `SHOW CHANGES … SINCE <cursor>` catch-up. It advances a persisted per-agent (versionstamp, change index) cursor past a change only once its delivery is on the bus. The listen loop reconnects with backoff and re-drains on every (re)connect. A per-coalition retention sweep ages out the log. As a result, a message is put on the bus after disconnects and restarts. A stop whose cursor save succeeds repeats no message. A process exit mid-page repeats the sent messages of that page. Deliveries already on the bus are not replayed.
- **Library-first async lifecycle** (`rust-v2:async-lifecycle`). Expose `CancellationToken` + `TaskTracker`, not `SubsystemHandle`, and let callers wire their top-level shutdown. `Coalition::new` has a readiness handshake for subscription-registering spawns. The integration test uses `DropGuard` for panic-safe container teardown.
- **`#[surreal(rename)]` for raw-identifier fields** (`surrealdb:repository-patterns`). The `SurrealValue` derive ignores `#[serde(rename)]`. Fields like `r#in` must carry `#[surreal(rename = "in")]` or round-trip as `None`.
- **Explicit edge-pointer projection on edge records** (`surrealdb:live-queries`). A bare `SELECT *` / `LIVE SELECT *` on `RELATE`-created edges omits `in`/`out`. Read them with `SELECT *, in, out FROM message ...`, as the integration test asserts. The durable-bus delivery path sidesteps this because `SHOW CHANGES` changeset records carry `id`/`in`/`out` natively. So the wake-up subscription only needs `LIVE SELECT id`.
- **Sync/async delivery bus**. Each agent forwards durable-log messages, delivered via catch-up, onto a shared bounded [async-channel](https://crates.io/crates/async-channel) MPMC channel as `Delivery<T> { recipient, message }`. Consume with `coalition.inbox()`. Clone it for multiple workers, or call `recv_blocking()` from a synchronous agent handler on a blocking thread (see `examples/worker_pool.rs`). This is the seam that a framework hangs agent logic off of.
- **Validate at the boundary, parameterize at the query** (`surrealdb:graph-operations`). `Agent::new` validates agent names (`InvalidAgentName`). The LIVE query binds them as `$owner` in place of interpolation, so a name can never alter the SQL. `Agent::send` makes sure that the recipient exists (`UnknownRecipient`) in place of writing a dangling edge.

## Documentation

For detailed documentation on SurrealDB, visit [SurrealDB's Documentation](https://surrealdb.com/docs).

## Contributors

- [@tsondru](https://github.com/tsondru): original design, daemon-shape POC, SurrealDB integration.
- Claude Opus 4.7 (1M context, via [Claude Code](https://claude.com/claude-code)): library-first refactor (2026-04). This work added `Coalition<T>`, generic `Message<T>`, the readiness handshake and a panic-safe test via `DropGuard`. It also made the three SDK-gotcha diagnoses: subscription race, `#[surreal(rename)]` and edge-pointer projection.
- Claude Opus 4.8 (1M context, via [Claude Code](https://claude.com/claude-code)): two-tier durable bus (2026-06). This work added `CHANGEFEED` + `SHOW CHANGES` catch-up over a persisted per-agent versionstamp cursor, and LIVE as a wake-up. It also added reconnect with backoff, the retention sweep, a restart-idempotent `Agent::new` and a restart-replay integration test, and moved surrealdb 3.1.4 → 3.1.5.

## Acknowledgments

- [SurrealDB](https://github.com/surrealdb/surrealdb): the graph + live-query database underneath.
- [tokio-util](https://docs.rs/tokio-util): `CancellationToken`, `TaskTracker`, `DropGuard`.
- [tokio-graceful-shutdown](https://github.com/Finomnis/tokio-graceful-shutdown): the subsystem framework that this repo used before the 2026-04 library-first refactor. It suits binary-level shutdown, and the library level dropped it on purpose.
