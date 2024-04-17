pub mod agent;
pub mod message;
pub mod top;

use surrealdb::engine::remote::ws::{Client, Ws};
use surrealdb::Surreal;
use tokio::sync::OnceCell;

static CONNECTION: OnceCell<Surreal<Client>> = OnceCell::const_new();

pub async fn connection() -> &'static Surreal<Client> {
    CONNECTION
        .get_or_init(|| async {
            tracing::debug!("Connecting to surrealdb");
            client().await
        })
        .await
}

async fn client() -> Surreal<Client> {
    let db = Surreal::new::<Ws>(format!("{}:{}", "localhost", 8000))
        .await
        .unwrap();

    db.use_ns("test").use_db("test").await.unwrap();
    db
}