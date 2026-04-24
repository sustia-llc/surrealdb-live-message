use serde::{Deserialize, Serialize};
use surrealdb::Surreal;
use surrealdb::engine::any;
use surrealdb::opt::Resource;
use surrealdb_live_message::logger;
use surrealdb_live_message::message::{MESSAGE_TABLE, Message};
use surrealdb_live_message::subsystems::agents::{AGENT_TABLE, Coalition};
use surrealdb_live_message::subsystems::sdb::{self, SurrealDBWrapper};
use surrealdb_types::{RecordId, SurrealValue};
use tokio::time::{Duration, sleep, timeout};
use tokio_util::{sync::CancellationToken, sync::DropGuard, task::TaskTracker};

const AGENT_BOB: &str = "bob";
const AGENT_ALICE: &str = "alice";

/// Test payload — user-defined type, not a library concern.
#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue, PartialEq)]
struct ChatMessage {
    content: String,
}

async fn init_db(db: &Surreal<any::Any>) {
    let _ = db.delete(Resource::from(MESSAGE_TABLE)).await;
    let _ = db.delete(Resource::from(AGENT_TABLE)).await;
    let _ = db.create(Resource::from(MESSAGE_TABLE)).await;
    let _ = db.create(Resource::from(AGENT_TABLE)).await;
}

/// Library-first integration test.
///
/// Exercises the exact shape from `rust-practical:async-lifecycle`:
/// - root `CancellationToken` owned by the test harness
/// - `sdb_task` spawned with a `child_token()`
/// - `Coalition<T>` manages its own internal TaskTracker; the test calls
///   `coalition.shutdown().await` when assertions are done
/// - no `tokio-graceful-shutdown`, no `Toplevel`, no `SIGINT` self-kill,
///   no `tokio::join!` over parallel futures
#[tokio::test]
async fn test_agent_messaging() {
    logger::setup();

    // Harness-level cancellation — one root token drives everything.
    let harness_token = CancellationToken::new();
    let harness_tracker = TaskTracker::new();

    // Panic-safe cleanup: if any assertion below panics, Drop runs on
    // _drop_guard which cancels the harness token, which cascades to the
    // sdb_task via child_token() → container shutdown → docker auto_remove.
    // Without this guard, a panic skips the explicit shutdown at the end of
    // the test and leaves the container running for the next invocation.
    let _drop_guard: DropGuard = harness_token.clone().drop_guard();

    // 1) Spawn sdb lifecycle.
    let sdb_token = harness_token.child_token();
    harness_tracker.spawn(async move {
        if let Err(e) = sdb::sdb_task(sdb_token).await {
            tracing::error!("sdb_task failed in test: {}", e);
        }
    });

    // 2) Wait for the DB to come up (bounded — fail fast rather than hang).
    timeout(Duration::from_secs(30), SurrealDBWrapper::wait_until_ready())
        .await
        .expect("timeout waiting for sdb")
        .expect("sdb ready signal failed");

    let db = sdb::SurrealDBWrapper::connection().await;
    init_db(db).await;

    // 3) Build the coalition. It owns its own lifecycle internally.
    let coalition = Coalition::<ChatMessage>::new(vec![
        AGENT_ALICE.to_string(),
        AGENT_BOB.to_string(),
    ])
    .await
    .expect("coalition creation");

    // 4) Resolve handles and exchange typed messages.
    let alice = coalition
        .agent(AGENT_ALICE)
        .await
        .expect("alice in coalition");
    let bob = coalition
        .agent(AGENT_BOB)
        .await
        .expect("bob in coalition");

    alice
        .send(
            AGENT_BOB,
            ChatMessage {
                content: "Hello from Alice!".to_string(),
            },
        )
        .await
        .expect("alice → bob send");

    bob.send(
        AGENT_ALICE,
        ChatMessage {
            content: "Hello from Bob!".to_string(),
        },
    )
    .await
    .expect("bob → alice send");

    // Live-query delivery latency — deliberately bounded.
    sleep(Duration::from_secs(1)).await;

    // 5) Assert on the graph edges.
    // RELATE direction: `alice -> message -> bob` creates an edge where
    // `in = alice` and `out = bob`. So `in = agent:alice` filters for
    // *outgoing* messages from alice (i.e., alice's sends, containing her
    // payload). Likewise for bob.
    let mut response = db
        .query("SELECT *, in, out FROM message WHERE in = agent:alice")
        .await
        .unwrap();
    let alice_outgoing: Vec<Message<ChatMessage>> = response.take(0).unwrap();
    assert_eq!(alice_outgoing.len(), 1);
    assert_eq!(
        alice_outgoing[0].payload,
        ChatMessage {
            content: "Hello from Alice!".to_string(),
        }
    );
    // Full edge-pointer assertion — locks in the `#[surreal(rename = "in")]`
    // + explicit `SELECT *, in, out` projection combo that makes `r#in`
    // deserialize correctly.
    assert_eq!(
        alice_outgoing[0].r#in,
        Some(RecordId::new(AGENT_TABLE, AGENT_ALICE))
    );
    assert_eq!(
        alice_outgoing[0].out,
        Some(RecordId::new(AGENT_TABLE, AGENT_BOB))
    );

    let mut response = db
        .query("SELECT *, in, out FROM message WHERE in = agent:bob")
        .await
        .unwrap();
    let bob_outgoing: Vec<Message<ChatMessage>> = response.take(0).unwrap();
    assert_eq!(bob_outgoing.len(), 1);
    assert_eq!(
        bob_outgoing[0].payload,
        ChatMessage {
            content: "Hello from Bob!".to_string(),
        }
    );
    assert_eq!(
        bob_outgoing[0].r#in,
        Some(RecordId::new(AGENT_TABLE, AGENT_BOB))
    );
    assert_eq!(
        bob_outgoing[0].out,
        Some(RecordId::new(AGENT_TABLE, AGENT_ALICE))
    );

    // 6) Shutdown — coalition first (agent drain), then the sdb task.
    coalition.shutdown().await;
    harness_token.cancel();
    harness_tracker.close();
    harness_tracker.wait().await;
}
