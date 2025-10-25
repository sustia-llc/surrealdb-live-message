use surrealdb::Surreal;
use surrealdb::engine::any;
use surrealdb::opt::Resource;
use surrealdb_live_message::logger;
use surrealdb_live_message::message::{MESSAGE_TABLE, Message, Payload, TextPayload};
use surrealdb_live_message::subsystems::agents::{AGENT_TABLE, Agent};
use surrealdb_live_message::subsystems::{agents, sdb};
use tokio::time::{Duration, sleep};
use tokio_graceful_shutdown::{SubsystemBuilder, SubsystemHandle, Toplevel};

const AGENT_BOB: &str = "bob";
const AGENT_ALICE: &str = "alice";

async fn clear_db(db: &Surreal<any::Any>) {
    db.delete(Resource::from(MESSAGE_TABLE)).await.unwrap();
    db.delete(Resource::from(AGENT_TABLE)).await.unwrap();
}

async fn test_main_subsystem(s: &mut SubsystemHandle) {
    // Start database subsystem
    s.start(SubsystemBuilder::new("sdb", sdb::sdb_subsystem));

    // Wait for database to be ready
    let db_ready_rx = sdb::SurrealDBWrapper::get_ready_receiver();
    match tokio::time::timeout(Duration::from_secs(30), db_ready_rx).await {
        Ok(Ok(())) => tracing::info!("Database is ready, starting agents..."),
        Ok(Err(_)) => panic!("Database ready signal channel closed"),
        Err(_) => panic!("Timeout waiting for database to be ready"),
    }

    // Clear database and start agents subsystem
    let db = sdb::SurrealDBWrapper::connection().await.to_owned();
    clear_db(&db).await;

    let names = vec![AGENT_ALICE.to_string(), AGENT_BOB.to_string()];
    s.start(SubsystemBuilder::new(
        "agents",
        agents::agents_subsystem_with_names(names),
    ));
}

/// Helper function to wait for an agent to be created in the database
async fn wait_for_agent(db: &Surreal<any::Any>, name: &str) -> Agent {
    let timeout = tokio::time::sleep(Duration::from_secs(10));
    tokio::select! {
        _ = timeout => panic!("Timeout waiting for agent {}", name),
        agent = async {
            let mut interval = tokio::time::interval(Duration::from_millis(100));
            loop {
                interval.tick().await;
                if let Ok(Some(agent)) = db.select((AGENT_TABLE, name)).await {
                    return agent;
                }
            }
        } => agent
    }
}

#[tokio::test]
async fn test_agent_messaging() {
    use nix::sys::signal::{self, Signal};
    use nix::unistd::Pid;

    logger::setup();

    tokio::join!(
        async {
            // Wait for the database container to start and be ready
            // This gives enough time for Docker container to pull image, start, and initialize
            sleep(Duration::from_secs(10)).await;

            // Get the database connection
            let db = sdb::SurrealDBWrapper::connection().await.to_owned();

            // Wait for agents to be created in the database
            let alice = wait_for_agent(&db, AGENT_ALICE).await;
            let bob = wait_for_agent(&db, AGENT_BOB).await;

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
            let result = Toplevel::new(test_main_subsystem)
                .catch_signals()
                .handle_shutdown_requests(Duration::from_secs(4))
                .await;
            assert!(result.is_ok());
        },
    );
}
