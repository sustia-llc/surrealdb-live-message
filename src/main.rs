use miette::Result;
use surrealdb::opt::auth::Root;
use tokio::time::Duration;
use tokio_graceful_shutdown::{SubsystemBuilder, Toplevel};

use surrealdb_live_message::connection;
use surrealdb_live_message::top::top_level;
use surrealdb_live_message::logger;

const AGENT_BOB: &str = "bob";
const AGENT_ALICE: &str = "alice";

#[tokio::main]
async fn main() -> Result<()> {
    logger::setup();

    let db = connection().await.to_owned();
    db.signin(Root {
        username: "root",
        password: "root",
    })
    .await
    .unwrap();
    db.use_ns("test").use_db("test").await.unwrap();
    // Setup and execute subsystem tree
    let names = vec![AGENT_ALICE.to_string(), AGENT_BOB.to_string()];
    Toplevel::new(move |s| async move {
        s.start(SubsystemBuilder::new("top_level", move |s| top_level(s, names)));
    })
    .catch_signals()
    .handle_shutdown_requests(Duration::from_millis(1000))
    .await
    .map_err(Into::into)
}
