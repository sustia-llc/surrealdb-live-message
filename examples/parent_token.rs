//! Driving `coalition.shutdown()` from a parent `CancellationToken`.
//!
//! `Coalition<T>` owns an *independent* internal token — cancelling the
//! daemon's root token does NOT stop its listen-loops. The two shutdown
//! examples react to an OS signal directly. This one shows Pattern 2 from the
//! `rust-practical:async-lifecycle` skill: the application owns a single
//! `app_token` representing "shut everything down", and a bridge ties the
//! coalition's lifecycle to it via `tokio::select!`.
//!
//! - An OS signal (SIGINT/SIGTERM) cancels `app_token`.
//! - The supervisor selects on `app_token.cancelled()`, then runs the standard
//!   coalition-first drain order.
//!
//! This is the composition pattern to reach for when the coalition is one of
//! several subsystems all hanging off one parent token, without pulling in a
//! supervision framework (contrast `examples/graceful_shutdown.rs`).
//!
//! # How to run
//! ```text
//! cargo run --example parent_token
//! ```
//!
//! # How to stop
//! Press Ctrl-C or send `kill -TERM <pid>`.
//!
//! # Prerequisites
//! Docker must be running.

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use surrealdb_types::SurrealValue;
use tokio::time::{Duration, sleep};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use surrealdb_live_message::logger;
use surrealdb_live_message::subsystems::agents::Coalition;
use surrealdb_live_message::subsystems::sdb::{self, SurrealDBWrapper};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
pub struct DaemonPayload {
    pub kind: String,
    pub body: String,
}

/// Wait for SIGINT or SIGTERM on Unix; fall back to ctrl_c elsewhere.
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
        tokio::signal::ctrl_c().await.expect("install ctrl_c handler");
        tracing::info!("ctrl-c received");
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    logger::setup();

    // The single parent token. Every subsystem's shutdown is wired to this.
    let app_token = CancellationToken::new();
    let tracker = TaskTracker::new();

    // Bridge the OS signal onto the parent token.
    {
        let app_token = app_token.clone();
        tokio::spawn(async move {
            shutdown_signal().await;
            app_token.cancel();
        });
    }

    // The DB token is INDEPENDENT of the parent — not a child. The shutdown
    // order requires the DB to stay up until the coalition has drained, so it
    // must be cancelled explicitly *after* `coalition.shutdown()`, never
    // simultaneously with the parent.
    let sdb_token = CancellationToken::new();
    let sdb_child = sdb_token.child_token();
    let mut sdb_handle = tracker.spawn(async move { sdb::sdb_task(sdb_child).await });
    tracker.close();

    tokio::select! {
        result = SurrealDBWrapper::wait_until_ready() => {
            result.context("SurrealDB ready signal failed")?;
        }
        result = &mut sdb_handle => {
            sdb_token.cancel();
            anyhow::bail!("sdb_task exited during startup: {result:?}");
        }
        _ = sleep(STARTUP_TIMEOUT) => {
            sdb_token.cancel();
            tracker.wait().await;
            anyhow::bail!("SurrealDB not ready within {STARTUP_TIMEOUT:?}");
        }
    }
    tracing::info!("SurrealDB ready.");

    let coalition = Coalition::<DaemonPayload>::new(vec!["alice".to_string(), "bob".to_string()])
        .await
        .context("failed to build coalition")?;

    let inbox = coalition.inbox();
    tokio::spawn(async move {
        while let Ok(d) = inbox.recv().await {
            tracing::info!(recipient = %d.recipient, payload = ?d.message.payload, "delivered");
        }
    });

    if let Some(alice) = coalition.agent("alice").await {
        alice
            .send(
                "bob",
                DaemonPayload {
                    kind: "greeting".to_string(),
                    body: "hello from alice (parent_token example)".to_string(),
                },
            )
            .await
            .context("alice → bob send")?;
        tracing::info!("alice sent greeting to bob");
    }

    tracing::info!("daemon running — send SIGINT or SIGTERM to stop");

    // Pattern 2: the coalition's lifecycle is bound to the parent token. When
    // anything cancels `app_token` (signal here, but it could be any subsystem
    // failure), we run the coalition-first drain order. Note we cannot derive
    // the coalition's token FROM the parent — it owns its own — so we bridge by
    // awaiting cancellation and calling shutdown() explicitly.
    app_token.cancelled().await;
    tracing::info!("parent token cancelled, draining");

    // ORDER MATTERS: coalition first (agent listen-loops delete their message
    // edges while the DB is still up), THEN cancel the independent sdb token
    // (triggers the container drain + stop), then wait for the sdb task.
    coalition.shutdown().await;
    sdb_token.cancel();
    tracker.wait().await;
    tracing::info!("daemon stopped cleanly");
    Ok(())
}
