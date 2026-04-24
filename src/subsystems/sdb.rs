use crate::sdb_server::SurrealDBContainer;
use crate::settings::SETTINGS;
use anyhow::Result;
use std::sync::OnceLock;
use surrealdb::Surreal;
use surrealdb::engine::any;
use surrealdb::opt::auth::Root;
use tokio::sync::{OnceCell, watch};
use tokio::time::{Duration, sleep};
use tokio_util::sync::CancellationToken;

static DB_READY: OnceLock<watch::Sender<bool>> = OnceLock::new();

pub struct SurrealDBWrapper;

impl SurrealDBWrapper {
    fn init_ready_channel() -> &'static watch::Sender<bool> {
        DB_READY.get_or_init(|| {
            let (tx, _rx) = watch::channel(false);
            tx
        })
    }

    fn get_ready_receiver() -> watch::Receiver<bool> {
        Self::init_ready_channel().subscribe()
    }

    /// Wait for the database to be ready (blocking async function).
    /// This can be called from multiple places concurrently.
    pub async fn wait_until_ready() -> Result<()> {
        let mut rx = Self::get_ready_receiver();
        rx.wait_for(|&ready| ready)
            .await
            .map_err(|_| anyhow::anyhow!("Database ready channel closed"))?;
        Ok(())
    }

    fn set_ready() {
        let tx = Self::init_ready_channel();
        let _ = tx.send(true);
    }

    pub async fn connection() -> &'static Surreal<any::Any> {
        static CONNECTION: OnceCell<Surreal<any::Any>> = OnceCell::const_new();

        CONNECTION
            .get_or_init(|| async {
                tracing::debug!("Initializing SurrealDB connection");
                SurrealDBWrapper::client().await
            })
            .await
    }

    async fn client() -> Surreal<any::Any> {
        let db = any::connect(&SETTINGS.sdb.endpoint)
            .await
            .expect("Failed to connect to SurrealDB via WebSocket");

        db.use_ns(&SETTINGS.sdb.namespace)
            .use_db(&SETTINGS.sdb.database)
            .await
            .expect("Failed to select namespace and database");

        let _ = db
            .signin(Root {
                username: SETTINGS.sdb.username.clone(),
                password: SETTINGS.sdb.password.clone(),
            })
            .await
            .expect("Failed to authenticate with SurrealDB");
        db
    }
}

/// Spawn the SurrealDB lifecycle as a library-first async task.
///
/// Takes a `CancellationToken` rather than a `SubsystemHandle`. Callers wire
/// their own top-level shutdown (`tokio::signal::ctrl_c`, parent
/// CancellationToken, test harness, etc.) and cancel the token when ready.
///
/// On cancel: drains for 2s to let outstanding queries complete, then stops
/// the local container (if present).
pub async fn sdb_task(token: CancellationToken) -> Result<()> {
    tracing::info!("sdb task starting.");
    let container = if SETTINGS.environment == "production" {
        tracing::info!("sdb using cloud connection.");
        None
    } else {
        tracing::info!("sdb using local container.");
        let container = SurrealDBContainer::new()
            .await
            .map_err(anyhow::Error::from)
            .expect("Failed to create SurrealDB container");
        container
            .start_and_wait()
            .await
            .expect("Failed to start and wait for container");
        Some(container)
    };

    // Establish the initial connection (works for both production and local)
    let _db = SurrealDBWrapper::connection().await.to_owned();

    // Signal database is ready
    SurrealDBWrapper::set_ready();

    tracing::info!("sdb ready and accepting connections.");

    token.cancelled().await;
    tracing::info!("sdb shutting down ...");
    sleep(Duration::from_secs(2)).await;
    if let Some(container) = container {
        container
            .stop()
            .await
            .expect("Failed to stop SurrealDB container");
    }
    tracing::info!("sdb stopped.");
    Ok(())
}
