use surrealdb::Surreal;
use surrealdb::engine::any;
use surrealdb::opt::Resource;
use surrealdb_live_message::logger;
use surrealdb_live_message::message::{MESSAGE_TABLE, Message, Payload, TextPayload};
use surrealdb_live_message::subsystems::agent::{AGENT_TABLE, get_registry};
use surrealdb_live_message::subsystems::agents::AgentsSubsystem;
use surrealdb_live_message::subsystems::sdb::{self, SdbSubsystem};
use tokio::time::{Duration, sleep};
use tokio_graceful_shutdown::{IntoSubsystem, SubsystemBuilder, SubsystemHandle, Toplevel};

const AGENT_BOB: &str = "bob";
const AGENT_ALICE: &str = "alice";

async fn clear_db(db: &Surreal<any::Any>) {
    db.delete(Resource::from(MESSAGE_TABLE)).await.unwrap();
    db.delete(Resource::from(AGENT_TABLE)).await.unwrap();
}

async fn test_main_subsystem(s: &mut SubsystemHandle<miette::Report>) {
    s.start(SubsystemBuilder::new("sdb", SdbSubsystem.into_subsystem()));

    // Wait for database to be ready
    let mut rx = sdb::SurrealDBWrapper::get_ready_receiver();

    tokio::select! {
        _ = tokio::time::sleep(Duration::from_secs(30)) => panic!("Timeout waiting for database to be ready"),
        _ = async {
            while !*rx.borrow_and_update() {
                let _ = rx.changed().await;
            }
        } => {},
    }

    let names = vec![AGENT_ALICE.to_string(), AGENT_BOB.to_string()];
    let db = sdb::SurrealDBWrapper::connection().await.to_owned();
    clear_db(&db).await;
    s.start(SubsystemBuilder::new(
        "agents",
        AgentsSubsystem { names }.into_subsystem(),
    ));
}

#[tokio::test]
async fn test_agent_messaging() {
    use nix::sys::signal::{self, Signal};
    use nix::unistd::Pid;

    logger::setup();

    tokio::join!(
        async {
            // Wait for agents to be initialized by polling the registry
            let registry = get_registry();
            let timeout = tokio::time::sleep(Duration::from_secs(10));
            tokio::select! {
                _ = timeout => panic!("Timeout waiting for agents to initialize"),
                _ = async {
                    let mut interval = tokio::time::interval(Duration::from_millis(100));
                    loop {
                        interval.tick().await;
                        let count = {
                            let agents = registry.lock().unwrap();
                            agents.len()
                        };
                        if count == 2 {
                            break;
                        }
                    }
                } => {}
            }

            // Now proceed with the test
            let db = sdb::SurrealDBWrapper::connection().await.to_owned();

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

            tracing::debug!("sending SIGINT to itself.");
            signal::kill(Pid::this(), Signal::SIGINT).unwrap();
        },
        async {
            let result = Toplevel::<miette::Report>::new(test_main_subsystem)
                .catch_signals()
                .handle_shutdown_requests(Duration::from_secs(4))
                .await;
            assert!(result.is_ok());
        },
    );
}
