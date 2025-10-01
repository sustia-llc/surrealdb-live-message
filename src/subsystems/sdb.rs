use crate::sdb_server::SurrealDBContainer;
use crate::settings::SETTINGS;
use miette::Result;
use std::sync::Mutex;
use std::sync::OnceLock;
use surrealdb::Surreal;
use surrealdb::engine::any;
use surrealdb::opt::auth::Root;
use tokio::sync::{OnceCell, oneshot};
use tokio::time::{Duration, sleep};
use tokio_graceful_shutdown::SubsystemHandle;

struct ReadyChannel {
    tx: Option<oneshot::Sender<()>>,
    rx: Option<oneshot::Receiver<()>>,
}

static DB_READY: OnceLock<Mutex<ReadyChannel>> = OnceLock::new();

pub struct SurrealDBWrapper;

impl SurrealDBWrapper {
    fn init_ready_channel() -> &'static Mutex<ReadyChannel> {
        DB_READY.get_or_init(|| {
            let (tx, rx) = oneshot::channel();
            Mutex::new(ReadyChannel {
                tx: Some(tx),
                rx: Some(rx),
            })
        })
    }

    pub fn get_ready_receiver() -> oneshot::Receiver<()> {
        let channel = Self::init_ready_channel();
        channel
            .lock()
            .unwrap()
            .rx
            .take()
            .expect("Ready receiver already taken")
    }

    pub fn set_ready() {
        let channel = Self::init_ready_channel();
        if let Some(tx) = channel.lock().unwrap().tx.take() {
            let _ = tx.send(());
        }
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
                username: &SETTINGS.sdb.username,
                password: &SETTINGS.sdb.password,
            })
            .await
            .expect("Failed to authenticate with SurrealDB");
        db
    }
}

pub async fn sdb_subsystem(subsys: &mut SubsystemHandle<miette::Report>) -> Result<()> {
    tracing::info!("{} subsystem starting.", subsys.name());
    let container = if SETTINGS.environment == "production" {
        tracing::info!("{} using cloud connection.", subsys.name());
        let _db = SurrealDBWrapper::connection().await.to_owned();
        None
    } else {
        tracing::info!("{} using local container.", subsys.name());
        let container = SurrealDBContainer::new()
            .await
            .map_err(|e| miette::miette!(e.to_string()))?;
        container
            .start_and_wait()
            .await
            .map_err(|e| miette::miette!(e.to_string()))?;
        Some(container)
    };

    // Signal database is ready
    SurrealDBWrapper::set_ready();

    tracing::info!("{} ready and accepting connections.", subsys.name());

    subsys.on_shutdown_requested().await;
    tracing::info!("Shutting down {} subsystem ...", subsys.name());
    sleep(Duration::from_secs(2)).await;
    if let Some(container) = container {
        container
            .stop()
            .await
            .map_err(|e| miette::miette!(e.to_string()))?;
    }
    tracing::info!("{} stopped.", subsys.name());
    Ok(())
}
