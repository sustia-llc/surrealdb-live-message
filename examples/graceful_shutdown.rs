//! Production-grade top-level lifecycle using `tokio-graceful-shutdown` 0.19.
//!
//! Demonstrates how to wrap the library's `CancellationToken`-based API inside
//! the `tokio-graceful-shutdown` framework, contrasting point-by-point with
//! `examples/production_shutdown.rs` (the hand-rolled version):
//!
//! | Concern                    | production_shutdown.rs         | this file                            |
//! |----------------------------|-------------------------------|--------------------------------------|
//! | Signal handling            | hand-rolled `shutdown_signal()`| `.catch_signals()` on `Toplevel`     |
//! | Shutdown timeout           | `tokio::time::timeout(...)`    | `handle_shutdown_requests(timeout)`  |
//! | Subsystem nesting          | explicit `token`/`tracker`     | `SubsystemBuilder` + `start()`       |
//! | `anyhow::Result` in `main` | direct                         | `.map_err(Into::into)`               |
//!
//! ## Design note — single orchestrating subsystem
//!
//! The coalition can only be built *after* the DB is ready. Cross-subsystem
//! ordering inside the framework is awkward (subsystems run concurrently), so
//! both the sdb task and the coalition live inside one `daemon` subsystem.
//! A local `CancellationToken` + `TaskTracker` bridge the library's token API
//! to the framework's `SubsystemHandle`.
//!
//! ## Shutdown order (same as production_shutdown.rs, framework-driven)
//!
//! When `Toplevel` catches a signal (or `subsys.on_shutdown_requested()` is
//! awaited in the subsystem), it calls the user code after the future returns.
//! Inside the subsystem we preserve the required order:
//!   1. `coalition.shutdown().await` — agent listen-loops drain while DB is up.
//!   2. `sdb_token.cancel()`         — triggers 2 s drain + container stop.
//!   3. `tracker.wait().await`       — waits for the sdb_task JoinHandle.
//!
//! `handle_shutdown_requests(SHUTDOWN_TIMEOUT)` enforces the outer bound so a
//! stuck coalition or container stop can't block the process indefinitely.
//!
//! # How to run
//! ```text
//! cargo run --example graceful_shutdown
//! ```
//!
//! # How to stop
//! Press Ctrl-C or send `kill -TERM <pid>`.
//!
//! # Prerequisites
//! Docker must be running. The library spins up a SurrealDB container on start
//! and tears it down on shutdown.

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use surrealdb_types::SurrealValue;
use tokio::time::{Duration, sleep};
use tokio_graceful_shutdown::{SubsystemBuilder, SubsystemHandle, Toplevel};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use surrealdb_live_message::logger;
use surrealdb_live_message::subsystems::agents::Coalition;
use surrealdb_live_message::subsystems::sdb::{self, SurrealDBWrapper};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// Typed payload exchanged between coalition agents.
#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
pub struct DaemonPayload {
    pub kind: String,
    pub body: String,
}

// ---------------------------------------------------------------------------
// Orchestrating subsystem
// ---------------------------------------------------------------------------

/// Single subsystem that owns both the DB lifecycle and the coalition.
///
/// Using one subsystem (rather than two sibling subsystems) guarantees that
/// the coalition is built only after `wait_until_ready()` resolves — something
/// the framework cannot express as a cross-subsystem ordering constraint.
async fn daemon(subsys: &mut SubsystemHandle) -> anyhow::Result<()> {
    // Local token + tracker bridge between the library's CancellationToken API
    // and the framework's SubsystemHandle. This token is NOT derived from the
    // framework's shutdown channel; it is cancelled explicitly by this function
    // after the coalition drains.
    let sdb_token = CancellationToken::new();
    let tracker = TaskTracker::new();

    // Spawn the SurrealDB lifecycle under a child of the local sdb_token.
    let child_token = sdb_token.child_token();
    let mut sdb_handle = tracker.spawn(async move { sdb::sdb_task(child_token).await });
    tracker.close();

    // ------------------------------------------------------------------
    // Startup supervision (three-arm select — no path hangs forever)
    //   A) DB signals ready           → proceed
    //   B) sdb_task exits early       → abort startup
    //   C) Hard startup timeout fires → abort startup
    // ------------------------------------------------------------------
    tracing::info!("waiting for SurrealDB to become ready …");

    tokio::select! {
        result = SurrealDBWrapper::wait_until_ready() => {
            result.context("SurrealDB ready signal failed")?;
        }
        result = &mut sdb_handle => {
            // sdb_task returned before signalling ready — startup failed.
            sdb_token.cancel();
            match result {
                Ok(Ok(())) => anyhow::bail!("sdb_task exited before signalling ready"),
                Ok(Err(e)) => anyhow::bail!("sdb_task failed during startup: {e}"),
                Err(e)     => anyhow::bail!("sdb_task panicked during startup: {e}"),
            }
        }
        _ = sleep(STARTUP_TIMEOUT) => {
            sdb_token.cancel();
            tracker.wait().await;
            anyhow::bail!("SurrealDB not ready within {STARTUP_TIMEOUT:?}");
        }
        // Also respect a shutdown request from the framework arriving during
        // startup (e.g. a signal fired before the DB was ready).
        _ = subsys.on_shutdown_requested() => {
            tracing::info!("shutdown requested during startup");
            sdb_token.cancel();
            tracker.wait().await;
            return Ok(());
        }
    }

    tracing::info!("SurrealDB ready.");

    // ------------------------------------------------------------------
    // Build coalition.
    //
    // Coalition<T> owns its OWN internal CancellationToken + TaskTracker.
    // They are independent of both the framework's shutdown channel and the
    // local sdb_token. The framework does NOT stop the coalition — we must
    // call coalition.shutdown().await ourselves before cancelling sdb_token.
    // ------------------------------------------------------------------
    let coalition = Coalition::<DaemonPayload>::new(vec!["alice".to_string(), "bob".to_string()])
        .await
        .context("failed to build coalition")?;

    // Optional: alice sends one message to bob so the run is observable.
    if let Some(alice) = coalition.agent("alice").await {
        alice
            .send(
                "bob",
                DaemonPayload {
                    kind: "greeting".to_string(),
                    body: "hello from alice (graceful_shutdown example)".to_string(),
                },
            )
            .await
            .context("alice → bob send")?;
        tracing::info!("alice sent greeting to bob");
    }

    tracing::info!("daemon running — send SIGINT or SIGTERM to stop");

    // ------------------------------------------------------------------
    // Wait for the framework to request shutdown (driven by .catch_signals()
    // on Toplevel — this replaces the hand-rolled shutdown_signal() in
    // production_shutdown.rs).
    // ------------------------------------------------------------------
    subsys.on_shutdown_requested().await;
    tracing::info!("shutdown requested by framework, beginning graceful drain");

    // ------------------------------------------------------------------
    // Graceful shutdown — ORDER MATTERS (same as production_shutdown.rs):
    //   1. coalition.shutdown() — agent listen-loops delete message edges;
    //      DB must still be up.
    //   2. sdb_token.cancel()  — triggers 2 s drain + container stop.
    //   3. tracker.wait()      — waits for the sdb_task JoinHandle.
    //
    // The outer SHUTDOWN_TIMEOUT is enforced by handle_shutdown_requests()
    // on Toplevel (replaces the hand-rolled tokio::time::timeout in
    // production_shutdown.rs).
    // ------------------------------------------------------------------
    coalition.shutdown().await;
    sdb_token.cancel();
    tracker.wait().await;

    tracing::info!("daemon stopped cleanly");
    Ok(())
}

// ---------------------------------------------------------------------------
// Root subsystem (Toplevel entry point)
// ---------------------------------------------------------------------------

/// Named async fn satisfies `for<'a> AsyncSubsysFn<&'a mut SubsystemHandle, ()>`.
///
/// `Toplevel::new` requires the root to return `()` (not `Result`) — errors are
/// reported by nested subsystems started via `SubsystemBuilder`, which *do*
/// return `Result`. An inline async closure hits a lifetime error because the
/// compiler cannot prove that the borrow of `s` outlives the async block; a
/// named fn is the idiomatic 0.19 pattern (matching the upstream examples).
async fn root_subsystem(s: &mut SubsystemHandle) {
    s.start(SubsystemBuilder::new("daemon", daemon));
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    logger::setup();

    // .catch_signals() replaces the hand-rolled shutdown_signal() from
    // production_shutdown.rs — the framework installs SIGINT + SIGTERM handlers
    // (Unix) or CTRL_C + CTRL_BREAK (Windows) automatically.
    //
    // handle_shutdown_requests(SHUTDOWN_TIMEOUT) replaces the hand-rolled
    // tokio::time::timeout wrapping the drain sequence in production_shutdown.rs.
    Toplevel::new(root_subsystem)
        .catch_signals()
        .handle_shutdown_requests(SHUTDOWN_TIMEOUT)
        .await
        .map_err(Into::into)
}
