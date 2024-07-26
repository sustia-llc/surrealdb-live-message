use crate::sdb_server;
use crate::settings::SETTINGS;
use miette::Result;
use std::{panic, str};
use surrealdb::engine::remote::ws::{Client, Ws};
use surrealdb::opt::auth::Root;
use surrealdb::Surreal;
use tokio::sync::OnceCell;
use tokio::time::{sleep, Duration};
use tokio_graceful_shutdown::SubsystemHandle;

pub const SUBSYS_NAME: &str = "sdb";

pub async fn connection() -> &'static Surreal<Client> {
    static CONNECTION: OnceCell<Surreal<Client>> = OnceCell::const_new();
    
    CONNECTION.get_or_init(|| async {
        tracing::debug!("Initializing SurrealDB connection");
        client().await
    }).await
}

async fn client() -> Surreal<Client> {
    let db = Surreal::new::<Ws>(format!("{}:{}", SETTINGS.sdb.host, SETTINGS.sdb.port))
        .await
        .unwrap();

    db.use_ns(&SETTINGS.sdb.namespace)
        .use_db(&SETTINGS.sdb.database)
        .await
        .unwrap();
    let _ = db
        .signin(Root {
            username: &SETTINGS.sdb.username,
            password: &SETTINGS.sdb.password,
        })
        .await;
    db
}

pub async fn sdb_subsystem(subsys: SubsystemHandle) -> Result<()> {
    if SETTINGS.environment == "production" {
        panic!("Production environment not implemented.");
    }
    tracing::debug!("{} subsystem starting.", SUBSYS_NAME);
    sdb_server::start_surrealdb_container().await.unwrap();
    tracing::debug!("{} started.", SUBSYS_NAME);

    subsys.on_shutdown_requested().await;
    tracing::debug!("Shutting down {} subsystem ...", SUBSYS_NAME);
    sleep(Duration::from_secs(2)).await;
    sdb_server::stop_surrealdb_container().await.unwrap();
    tracing::debug!("{} stopped.", SUBSYS_NAME);
    Ok(())
}
