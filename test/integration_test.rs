use std::env;
use std::sync::OnceLock;
use surrealdb::engine::remote::ws::Client;
use surrealdb::opt::Resource;
use surrealdb::Surreal;
use surrealdb_live_message::logger;
use surrealdb_live_message::message::{Message, Payload, TextPayload, MESSAGE_TABLE};
use surrealdb_live_message::subsystems::agent::{get_registry, AGENT_TABLE};
use surrealdb_live_message::subsystems::agents;
use surrealdb_live_message::subsystems::sdb;
use tokio::time::{sleep, Duration};
use tokio_graceful_shutdown::{SubsystemBuilder, Toplevel};
use tokio::sync::watch;

static SETUP_READY: OnceLock<watch::Sender<bool>> = OnceLock::new();

fn get_ready_receiver() -> watch::Receiver<bool> {
    SETUP_READY.get_or_init(|| {
        let (tx, _) = watch::channel(false);
        tx
    }).subscribe()
}

const AGENT_BOB: &str = "bob";
const AGENT_ALICE: &str = "alice";

async fn clear_db(db: &Surreal<Client>) {
    db.delete(Resource::from(MESSAGE_TABLE)).await.unwrap();
    db.delete(Resource::from(AGENT_TABLE)).await.unwrap();
}

#[tokio::test]
async fn test_agent_messaging() {
    use nix::sys::signal::{self, Signal};
    use nix::unistd::Pid;

    env::set_var("RUN_MODE", "test");
    logger::setup();

    tokio::join!(
        async {
            let mut rx = get_ready_receiver();
            let timeout = tokio::time::sleep(Duration::from_secs(30));
            tokio::select! {
                _ = async {
                    while !*rx.borrow_and_update() {
                        let _ = rx.changed().await;
                    }
                } => {},
                _ = timeout => panic!("Timeout waiting for setup"),
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
                s.start(SubsystemBuilder::new("sdb", sdb::sdb_subsystem));
                
                // Wait for database to be ready
                let mut rx = sdb::get_ready_receiver();
                let timeout = tokio::time::sleep(Duration::from_secs(30));
                tokio::select! {
                    _ = async {
                        while !*rx.borrow_and_update() {
                            let _ = rx.changed().await;
                        }
                    } => {},
                    _ = timeout => panic!("Timeout waiting for database to be ready"),
                }

                s.start(SubsystemBuilder::new("agents", move |s| agents::agents_subsystem(s, names)));
                SETUP_READY.get_or_init(|| {
                    let (tx, _) = watch::channel(true);
                    tx
                }).send(true).unwrap();
            })
            .catch_signals()
            .handle_shutdown_requests(Duration::from_secs(4))
            .await;
            assert!(result.is_ok());
        },
    );
}