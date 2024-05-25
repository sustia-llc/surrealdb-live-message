use once_cell::sync::Lazy;
use std::env;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use surrealdb::engine::remote::ws::Client;
use surrealdb::opt::Resource;
use surrealdb::Surreal;
use surrealdb_live_message::logger;
use surrealdb_live_message::message::{Message, Payload, TextPayload, MESSAGE_TABLE};
use surrealdb_live_message::sdb_server;
use surrealdb_live_message::subsystems::agent::get_registry;
use surrealdb_live_message::subsystems::agents;
use surrealdb_live_message::subsystems::sdb;
use tokio::time::{sleep, Duration};
use tokio_graceful_shutdown::{SubsystemBuilder, Toplevel};

pub static READY_FLAG: Lazy<Arc<AtomicBool>> = Lazy::new(|| Arc::new(AtomicBool::new(false)));

fn is_ready() -> bool {
    READY_FLAG.load(Ordering::SeqCst)
}

fn set_ready() {
    READY_FLAG.store(true, Ordering::SeqCst);
}

const AGENT_BOB: &str = "bob";
const AGENT_ALICE: &str = "alice";

async fn clear_db(db: &Surreal<Client>) {
    db.delete(Resource::from(MESSAGE_TABLE)).await.unwrap();
}

#[tokio::test]
async fn test_agent_messaging() {
    use nix::sys::signal::{self, Signal};
    use nix::unistd::Pid;

    env::set_var("RUN_MODE", "test");
    logger::setup();

    tokio::join!(
        async {
            while !is_ready() {
                sleep(Duration::from_millis(100)).await;
            }

            let db = sdb::connection().await.to_owned();
            clear_db(&db).await;

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

            // Verify messages
            let mut response = db
                .query("SELECT * FROM message WHERE in = agent:alice")
                .await
                .unwrap();
            let alice_messages: Vec<Message> = response.take(0).unwrap();
            assert_eq!(alice_messages.len(), 1);

            let mut response = db
                .query("SELECT * FROM message WHERE in = agent:bob")
                .await
                .unwrap();
            let bob_messages: Vec<Message> = response.take(0).unwrap();
            assert_eq!(bob_messages.len(), 1);

            clear_db(&db).await;

            tracing::debug!("sending SIGINT to itself.");
            signal::kill(Pid::this(), Signal::SIGINT).unwrap();
        },
        async {
            let names = vec![AGENT_ALICE.to_string(), AGENT_BOB.to_string()];
            let result = Toplevel::new(move |s| async move {
                s.start(SubsystemBuilder::new(sdb::SUBSYS_NAME, sdb::sdb_subsystem));
                sdb_server::surrealdb_ready().await.unwrap();
                s.start(SubsystemBuilder::new(agents::SUBSYS_NAME, move |s| {
                    agents::agents_subsystem(s, names)
                }));
                set_ready();
            })
            .catch_signals()
            .handle_shutdown_requests(Duration::from_secs(4))
            .await;
            assert!(result.is_ok());
        },
    );
}
