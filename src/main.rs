use miette::Result;
use surrealdb::opt::auth::Root;
use tokio::time::Duration;
use tokio_graceful_shutdown::{SubsystemBuilder, Toplevel};

use surrealdb_live_message::connection;
use surrealdb_live_message::top::top_level;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .init();
    let db = connection().await.to_owned();
    db.signin(Root {
        username: "root",
        password: "root",
    })
    .await
    .unwrap();
    db.use_ns("test").use_db("test").await.unwrap();
    // Setup and execute subsystem tree
    Toplevel::new(move |s| async move {
        s.start(SubsystemBuilder::new("top_level", move |s| top_level(s)));
    })
    .catch_signals()
    .handle_shutdown_requests(Duration::from_millis(1000))
    .await
    .map_err(Into::into)
}
