use std::env;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use surrealdb::engine::remote::ws::{Client, Ws};
use surrealdb::opt::auth::Root;
use surrealdb::opt::Resource;
use surrealdb::Surreal;
use surrealdb_live_message::agent::get_registry;
use surrealdb_live_message::logger;
use surrealdb_live_message::message::{
    MessageHistory, Payload, TextPayload, MESSAGE_HISTORY_TABLE, MESSAGE_TABLE,
};
use surrealdb_live_message::top;
use tokio::time::{sleep, Duration};
use tokio_graceful_shutdown::{SubsystemBuilder, Toplevel};

lazy_static::lazy_static! {
    static ref READY_FLAG: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
}

fn is_ready() -> bool {
    READY_FLAG.load(Ordering::SeqCst)
}

fn set_ready() {
    READY_FLAG.store(true, Ordering::SeqCst);
}

const AGENT_BOB: &str = "bob";
const AGENT_ALICE: &str = "alice";

async fn setup() -> Surreal<Client> {
    let db = Surreal::new::<Ws>("localhost:8000").await.unwrap();
    db.signin(Root {
        username: "root",
        password: "root",
    })
    .await
    .unwrap();
    db.use_ns("test").use_db("test").await.unwrap();
    clear_tables(&db).await;
    db
}

async fn clear_tables(db: &Surreal<Client>) {
    db.delete(Resource::from(MESSAGE_HISTORY_TABLE))
        .await
        .unwrap();
    db.delete(Resource::from(MESSAGE_TABLE)).await.unwrap();
}

async fn teardown(db: &Surreal<Client>) {
    clear_tables(db).await;
}

#[tokio::test]
async fn test_agent_messaging() {
    use nix::sys::signal::{self, Signal};
    use nix::unistd::Pid;

    env::set_var("RUN_MODE", "test");
    logger::setup();

    let db = setup().await;

    tokio::join!(
        async {
            while !is_ready() {
                sleep(Duration::from_millis(100)).await;
            }
            let registry = get_registry();
            let agents = registry.lock().unwrap();
            assert_eq!(agents.len(), 2);

            let alice = agents
                .iter()
                .find(|&a| a.id.id.to_string() == AGENT_ALICE)
                .expect("Failed to find alice");
            let bob = agents
                .iter()
                .find(|&a| a.id.id.to_string() == AGENT_BOB)
                .expect("Failed to find bob");

            // Send messages
            let text_payload = TextPayload {
                content: "Hello from Alice!".to_string(),
            };
            let payload = Payload::Text(text_payload);
            alice
                .send(AGENT_BOB, payload)
                .await
                .expect("Failed to send message");

            let text_payload = TextPayload {
                content: "Hello from Bob!".to_string(),
            };
            let payload = Payload::Text(text_payload);
            bob.send(AGENT_ALICE, payload)
                .await
                .expect("Failed to send message");

            sleep(Duration::from_secs(1)).await; // Wait for messages to be processed

            // Verify messages are saved in the message history
            let mut response = db
                .query("SELECT * FROM message_history WHERE message.from = agent:alice")
                .await
                .unwrap();
            let alice_messages: Vec<MessageHistory> = response.take(0).unwrap();
            assert_eq!(alice_messages.len(), 1);

            let mut response = db
                .query("SELECT * FROM message_history WHERE message.from = agent:bob")
                .await
                .unwrap();
            let bob_messages: Vec<MessageHistory> = response.take(0).unwrap();
            assert_eq!(bob_messages.len(), 1);

            tracing::debug!("sending SIGINT to itself.");
            signal::kill(Pid::this(), Signal::SIGINT).unwrap();
        },
        async {
            let names = vec![AGENT_ALICE.to_string(), AGENT_BOB.to_string()];
            let result = Toplevel::new(move |s| async move {
                s.start(SubsystemBuilder::new("top_level", move |s| {
                    top::top_level(s, names)
                }));
                set_ready();
            })
            .catch_signals()
            .handle_shutdown_requests(Duration::from_millis(3000))
            .await;
            assert!(result.is_ok());

            teardown(&db).await;
        },
    );
}
