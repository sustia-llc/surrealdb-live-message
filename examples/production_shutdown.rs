//! Production-grade top-level lifecycle with a hand-rolled tokio driver.
//!
//! Demonstrates:
//! - Root `CancellationToken` + `TaskTracker` owned by `main`.
//! - `sdb_task` spawned with a `child_token()` so the database lifecycle is
//!   independently cancellable.
//! - A three-arm startup `select!` that races DB readiness, sdb task failure,
//!   and a hard startup timeout — no path can hang forever.
//! - Correct shutdown ORDER: `coalition.shutdown()` first (agent listen-loops
//!   delete their message edges while the DB is still up), then cancel the sdb
//!   token (triggers the 2 s drain + container stop).
//! - A bounded graceful drain via `tokio::time::timeout` so the process never
//!   blocks past `SHUTDOWN_TIMEOUT`.
//! - Portable `shutdown_signal()` that handles SIGINT + SIGTERM on Unix and
//!   falls back to `ctrl_c` on other platforms.
//!
//! # How to run
//! ```text
//! cargo run --example production_shutdown
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
// Signal helper
// ---------------------------------------------------------------------------

/// Wait for SIGINT or SIGTERM on Unix; fall back to ctrl_c on other platforms.
///
/// `.expect("install ... handler")` is acceptable in an example binary: signal
/// registration only fails if the process has already installed the maximum
/// number of handlers, which is not possible this early in startup.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut sigint = signal(SignalKind::interrupt()).expect("install SIGINT handler");
        let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM handler");

        tokio::select! {
            _ = sigint.recv()  => tracing::info!("SIGINT received"),
            _ = sigterm.recv() => tracing::info!("SIGTERM received"),
        }
    }

    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c()
            .await
            .expect("install ctrl_c handler");
        tracing::info!("ctrl-c received");
    }
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    logger::setup();

    // Root token + tracker. Every spawned task gets a child token derived from
    // this root so a single `token.cancel()` propagates everywhere.
    let token = CancellationToken::new();
    let tracker = TaskTracker::new();

    // Spawn the SurrealDB lifecycle under a child token so it can be cancelled
    // independently from the root (the shutdown order requires cancelling the
    // sdb token *after* the coalition has drained).
    let sdb_token = token.child_token();
    let mut sdb_handle = tracker.spawn(async move { sdb::sdb_task(sdb_token).await });

    // Close the tracker now — no more tasks will be spawned on it after this
    // point. `tracker.wait()` can therefore return once the sdb task finishes.
    tracker.close();

    // ------------------------------------------------------------------
    // Startup supervision
    //
    // Three arms race:
    //   A) DB signals ready           → proceed to build the coalition
    //   B) sdb_task exits early       → bail, it logged its own error
    //   C) Hard startup timeout fires → bail, cancel everything
    // ------------------------------------------------------------------
    tracing::info!("waiting for SurrealDB to become ready …");

    tokio::select! {
        result = SurrealDBWrapper::wait_until_ready() => {
            result.context("SurrealDB ready signal failed")?;
        }
        result = &mut sdb_handle => {
            // sdb_task returned before signalling ready — startup failed.
            token.cancel();
            match result {
                Ok(Ok(())) => anyhow::bail!("sdb_task exited before signalling ready"),
                Ok(Err(e)) => anyhow::bail!("sdb_task failed during startup: {e}"),
                Err(e)     => anyhow::bail!("sdb_task panicked during startup: {e}"),
            }
        }
        _ = sleep(STARTUP_TIMEOUT) => {
            token.cancel();
            tracker.wait().await;
            anyhow::bail!("SurrealDB not ready within {STARTUP_TIMEOUT:?}");
        }
    }

    tracing::info!("SurrealDB ready.");

    // ------------------------------------------------------------------
    // Build the coalition.
    //
    // Coalition<T> owns its OWN internal CancellationToken + TaskTracker —
    // they are NOT children of the daemon's root token. The daemon must call
    // coalition.shutdown().await explicitly; cancelling `token` does NOT stop
    // the coalition's listen-loops.
    // ------------------------------------------------------------------
    let coalition = Coalition::<DaemonPayload>::new(vec!["alice".to_string(), "bob".to_string()])
        .await
        .context("failed to build coalition")?;

    // Untracked best-effort inbox drainer — consumes the delivery bus and logs
    // each delivery. Stops on its own when the bus closes (all agents shut down
    // → recv returns Err).
    let inbox = coalition.inbox();
    tokio::spawn(async move {
        // Ends when the bus closes (all agents shut down → recv returns Err).
        while let Ok(d) = inbox.recv().await {
            tracing::info!(recipient = %d.recipient, payload = ?d.message.payload, "delivered");
        }
    });

    // Optional: have alice send one message to bob so the run does something
    // visible in the logs even on an immediate Ctrl-C.
    if let Some(alice) = coalition.agent("alice").await {
        alice
            .send(
                "bob",
                DaemonPayload {
                    kind: "greeting".to_string(),
                    body: "hello from alice".to_string(),
                },
            )
            .await
            .context("alice → bob send")?;
        tracing::info!("alice sent greeting to bob");
    }

    tracing::info!("daemon running — send SIGINT or SIGTERM to stop");

    // ------------------------------------------------------------------
    // Supervision loop
    //
    // Wait for either an OS signal or sdb_task exiting unexpectedly.
    // ------------------------------------------------------------------
    tokio::select! {
        _ = shutdown_signal() => {
            tracing::info!("signal received, beginning graceful shutdown");
        }
        result = &mut sdb_handle => {
            tracing::error!(
                "sdb_task exited unexpectedly: {:?} — initiating emergency shutdown",
                result
            );
        }
    }

    // ------------------------------------------------------------------
    // Bounded graceful drain
    //
    // ORDER MATTERS:
    //   1. coalition.shutdown() — cancels agent listen-loops, which delete
    //      their message edges.  The DB must still be up for this to succeed.
    //   2. token.cancel()       — propagates to the sdb child token, triggering
    //      its 2 s drain and container stop.
    //   3. tracker.wait()       — waits for the sdb_task JoinHandle to resolve.
    //
    // The whole sequence is wrapped in a timeout so a stuck coalition or
    // container stop cannot block the process indefinitely.
    // ------------------------------------------------------------------
    match tokio::time::timeout(SHUTDOWN_TIMEOUT, async {
        coalition.shutdown().await;
        token.cancel();
        tracker.wait().await;
    })
    .await
    {
        Ok(()) => tracing::info!("daemon stopped cleanly"),
        Err(_) => {
            tracing::error!("graceful shutdown exceeded {SHUTDOWN_TIMEOUT:?}, forcing exit");
            // Force-cancel anything still running so the process can exit.
            token.cancel();
        }
    }

    Ok(())
}
