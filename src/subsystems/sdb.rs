use crate::error::{Error, Result};
use crate::sdb_server::SurrealDBContainer;
use crate::settings::SETTINGS;
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
            .map_err(|_| Error::ReadyChannelClosed)?;
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
            .map_err(Error::Connect)
            .expect("Failed to connect to SurrealDB via WebSocket");

        db.use_ns(&SETTINGS.sdb.namespace)
            .use_db(&SETTINGS.sdb.database)
            .await
            .map_err(Error::UseNsDb)
            .expect("Failed to select namespace and database");

        let _ = db
            .signin(Root {
                username: SETTINGS.sdb.username.clone(),
                password: SETTINGS.sdb.password.clone(),
            })
            .await
            .map_err(Error::Auth)
            .expect("Failed to authenticate with SurrealDB");

        // Define schema exactly once, after signin and before any agent/message
        // ops. Every caller reaches the database through `connection()`, which
        // runs `client()` exactly once via its `OnceCell`, so this also runs
        // exactly once. `IF NOT EXISTS` on every statement keeps repeated
        // startups (e.g. reconnects against a persistent store) idempotent.
        Self::define_schema(&db)
            .await
            .expect("Failed to define SurrealDB schema");

        db
    }

    /// Define the `agent` and `message` schema idempotently.
    ///
    /// `agent` is SCHEMAFULL: it only ever holds `name`/`created`, so locking
    /// the shape catches typos and stray fields.
    ///
    /// `message` is a graph edge (`TYPE RELATION IN agent OUT agent`) and is
    /// deliberately **SCHEMALESS**. The endpoint constraint is enforced by the
    /// table type regardless of schema mode, and we type `created`, but the
    /// `payload` field is generic over the caller's `T` (each `Coalition<T>`
    /// uses a different payload). SCHEMALESS lets any `payload` shape round-trip
    /// untouched. SCHEMAFULL was rejected here: a generic payload would require
    /// `FLEXIBLE`, which as of 3.1.4 is restricted to types containing `object`
    /// (`FLEXIBLE TYPE any` errors), and an un-flexible `TYPE any` adds no value
    /// while risking dropped fields. SCHEMALESS enforces the endpoints + typed
    /// `created` without ever constraining `payload`.
    ///
    /// `message` carries a **`CHANGEFEED`** so the two-tier durable bus can
    /// replay missed messages on (re)connect (`SHOW CHANGES FOR TABLE message
    /// SINCE <versionstamp>`); the window equals `message_retention_secs`. The
    /// feed is set via `DEFINE TABLE IF NOT EXISTS` so it is established once at
    /// table creation and **preserved across restarts** — re-defining it every
    /// startup would reset the change log and defeat cross-restart catch-up.
    /// NOTE: a `message` table created by a pre-changefeed schema version will
    /// not gain the feed retroactively (this lib is pre-1.0; no migration ships).
    ///
    /// `cursor` persists each agent's high-water mark (the last versionstamp it
    /// drained) so catch-up is bounded and exactly resumable across restarts.
    async fn define_schema(db: &Surreal<any::Any>) -> Result<()> {
        let retention = SETTINGS.sdb.message_retention_secs;
        let schema = format!(
            "
            DEFINE TABLE IF NOT EXISTS agent SCHEMAFULL;
            DEFINE FIELD IF NOT EXISTS name ON agent TYPE string;
            DEFINE FIELD IF NOT EXISTS created ON agent TYPE datetime;

            DEFINE TABLE IF NOT EXISTS message TYPE RELATION IN agent OUT agent SCHEMALESS CHANGEFEED {retention}s;
            DEFINE FIELD IF NOT EXISTS created ON message TYPE datetime;

            DEFINE TABLE IF NOT EXISTS cursor SCHEMAFULL;
            DEFINE FIELD IF NOT EXISTS versionstamp ON cursor TYPE int;
        "
        );

        db.query(&schema)
            .await
            .map_err(Error::Schema)?
            .check()
            .map_err(Error::Schema)?;

        Ok(())
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
