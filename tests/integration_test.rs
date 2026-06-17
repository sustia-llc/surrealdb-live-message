use serde::{Deserialize, Serialize};
use surrealdb::Surreal;
use surrealdb::engine::any;
use surrealdb::opt::Resource;
use surrealdb_live_message::error::Error;
use surrealdb_live_message::logger;
use surrealdb_live_message::message::{MESSAGE_TABLE, Message};
use surrealdb_live_message::subsystems::agents::{AGENT_TABLE, Agent, Coalition, Delivery};
use surrealdb_live_message::subsystems::sdb::{self, SurrealDBWrapper};
use surrealdb_types::{RecordId, SurrealValue};
use tokio::time::{Duration, timeout};
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
    timeout(
        Duration::from_secs(30),
        SurrealDBWrapper::wait_until_ready(),
    )
    .await
    .expect("timeout waiting for sdb")
    .expect("sdb ready signal failed");

    let db = sdb::SurrealDBWrapper::connection().await;
    init_db(db).await;

    // 3) Build the coalition. It owns its own lifecycle internally.
    let coalition =
        Coalition::<ChatMessage>::new(vec![AGENT_ALICE.to_string(), AGENT_BOB.to_string()])
            .await
            .expect("coalition creation");

    // 4) Resolve handles and exchange typed messages.
    let alice = coalition
        .agent(AGENT_ALICE)
        .await
        .expect("alice in coalition");
    let bob = coalition.agent(AGENT_BOB).await.expect("bob in coalition");

    // Grab a receiver on the delivery bus BEFORE sending so no delivery is
    // missed.
    let inbox = coalition.inbox();

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

    // Drain exactly 2 deliveries off the shared MPMC bus, keyed by recipient.
    // Each recv is bounded by a timeout so a missed delivery fails fast rather
    // than hanging the test.
    let mut deliveries: std::collections::HashMap<String, Delivery<ChatMessage>> =
        std::collections::HashMap::new();
    for _ in 0..2 {
        let d = timeout(Duration::from_secs(5), inbox.recv())
            .await
            .expect("inbox delivery timed out")
            .expect("inbox bus closed unexpectedly");
        deliveries.insert(d.recipient.clone(), d);
    }
    let to_bob = deliveries.get(AGENT_BOB).expect("delivery to bob");
    assert_eq!(
        to_bob.message.payload,
        ChatMessage {
            content: "Hello from Alice!".to_string()
        }
    );
    assert_eq!(
        to_bob.message.r#in,
        Some(RecordId::new(AGENT_TABLE, AGENT_ALICE))
    );
    let to_alice = deliveries.get(AGENT_ALICE).expect("delivery to alice");
    assert_eq!(
        to_alice.message.payload,
        ChatMessage {
            content: "Hello from Bob!".to_string()
        }
    );
    assert_eq!(
        to_alice.message.r#in,
        Some(RecordId::new(AGENT_TABLE, AGENT_BOB))
    );

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

    // ------------------------------------------------------------------
    // Additional scenarios.
    //
    // Run sequentially against the SAME container: `sdb.rs` holds a single
    // process-global `CONNECTION` OnceCell and `sdb_server.rs` pins a fixed
    // container name + port, so parallel `#[tokio::test]` fns would collide.
    // Consolidating keeps one container and a deterministic order (matches the
    // `rust-v2:testing` "quality over quantity" guidance).
    // ------------------------------------------------------------------
    scenario_unknown_agent(db, &alice).await;
    scenario_schema_enforcement(db).await;
    scenario_handshake_race().await;
    scenario_fanout().await;
    scenario_listen_loop_dropped().await;
    scenario_bus_close().await;

    // 6) Shutdown — coalition first (agent drain), then the sdb task.
    coalition.shutdown().await;
    harness_token.cancel();
    harness_tracker.close();
    harness_tracker.wait().await;
}

/// **Send to unknown agent.** `Agent::send` to a name with no `agent` record
/// currently *succeeds*: the `RELATE` creates an edge whose `out` dangles at a
/// non-existent agent, and no `agent` record is auto-created. This locks in the
/// documented current behavior (see `TODO.md` — whether to reject is open).
async fn scenario_unknown_agent(db: &Surreal<any::Any>, alice: &Agent) {
    alice
        .send(
            "ghost",
            ChatMessage {
                content: "into the void".to_string(),
            },
        )
        .await
        .expect("send to unknown agent currently succeeds");

    // The dangling edge exists, pointing `out` at the non-existent agent.
    let mut resp = db
        .query("SELECT *, in, out FROM message WHERE in = agent:alice AND out = agent:ghost")
        .await
        .unwrap();
    let edges: Vec<Message<ChatMessage>> = resp.take(0).unwrap();
    assert_eq!(edges.len(), 1, "dangling edge to unknown agent should exist");
    assert_eq!(edges[0].out, Some(RecordId::new(AGENT_TABLE, "ghost")));

    // No `agent` record was auto-created for the unknown recipient.
    let mut resp = db
        .query("SELECT VALUE id FROM agent WHERE id = agent:ghost")
        .await
        .unwrap();
    let ids: Vec<RecordId> = resp.take(0).unwrap();
    assert!(
        ids.is_empty(),
        "no agent record should be created for an unknown recipient"
    );
}

/// **Schema enforcement.** `message TYPE RELATION IN agent OUT agent` must
/// reject a non-`agent` endpoint, and `created TYPE datetime` must reject a
/// non-datetime value.
async fn scenario_schema_enforcement(db: &Surreal<any::Any>) {
    // `in` endpoint is from table `thing`, not `agent` → rejected.
    let res = db
        .query("RELATE thing:x->message->agent:alice CONTENT { created: time::now(), payload: {} }")
        .await;
    let rejected = match res {
        Ok(r) => r.check().is_err(),
        Err(_) => true,
    };
    assert!(
        rejected,
        "RELATE with a non-agent `in` endpoint must be rejected"
    );

    // `created` is not a datetime → rejected.
    let res = db
        .query(
            "RELATE agent:alice->message->agent:bob \
             CONTENT { created: 'not-a-datetime', payload: {} }",
        )
        .await;
    let rejected = match res {
        Ok(r) => r.check().is_err(),
        Err(_) => true,
    };
    assert!(rejected, "non-datetime `created` must be rejected");
}

/// **Delivery races the handshake.** Send *immediately* after `Coalition::new`
/// returns — no sleep. The readiness handshake must guarantee the recipient's
/// subscription is already live, so the first message is never lost.
async fn scenario_handshake_race() {
    let coalition = Coalition::<ChatMessage>::new(vec!["carol".to_string(), "dave".to_string()])
        .await
        .expect("coalition creation");
    let inbox = coalition.inbox();
    let carol = coalition.agent("carol").await.expect("carol in coalition");

    carol
        .send(
            "dave",
            ChatMessage {
                content: "race".to_string(),
            },
        )
        .await
        .expect("carol → dave send");

    let d = timeout(Duration::from_secs(5), inbox.recv())
        .await
        .expect("delivery timed out — handshake failed to prevent a lost first message")
        .expect("inbox bus closed unexpectedly");
    assert_eq!(d.recipient, "dave");
    assert_eq!(
        d.message.payload,
        ChatMessage {
            content: "race".to_string()
        }
    );

    coalition.shutdown().await;
}

/// **N-agent fan-out.** One sender to many recipients; assert every recipient's
/// delivery lands on the shared MPMC bus exactly once.
async fn scenario_fanout() {
    const WORKERS: [&str; 4] = ["w1", "w2", "w3", "w4"];
    let mut names = vec!["hub".to_string()];
    names.extend(WORKERS.iter().map(|w| w.to_string()));

    let coalition = Coalition::<ChatMessage>::new(names)
        .await
        .expect("coalition creation");
    let inbox = coalition.inbox();
    let hub = coalition.agent("hub").await.expect("hub in coalition");

    for w in WORKERS {
        hub.send(
            w,
            ChatMessage {
                content: format!("hi {w}"),
            },
        )
        .await
        .unwrap_or_else(|e| panic!("hub → {w} send: {e}"));
    }

    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for _ in 0..WORKERS.len() {
        let d = timeout(Duration::from_secs(5), inbox.recv())
            .await
            .expect("fan-out delivery timed out")
            .expect("inbox bus closed unexpectedly");
        *seen.entry(d.recipient).or_default() += 1;
    }
    for w in WORKERS {
        assert_eq!(seen.get(w), Some(&1), "worker {w} should receive exactly once");
    }

    coalition.shutdown().await;
}

/// **Typed-error path: `ListenLoopDropped`.** A name that the typed `RecordId`
/// API stores fine but that breaks the interpolated `LIVE SELECT` (a space ⇒
/// `agent:ghost rider` is a parse error). The listen loop errors *before*
/// signalling ready, dropping its ready sender, which `Coalition::new` surfaces
/// as `ListenLoopDropped` rather than hanging until `ReadyTimeout`.
async fn scenario_listen_loop_dropped() {
    match Coalition::<ChatMessage>::new(vec!["ghost rider".to_string()]).await {
        Ok(_) => panic!("a malformed agent name should fail coalition startup"),
        Err(e) => assert!(
            matches!(e, Error::ListenLoopDropped { .. }),
            "expected ListenLoopDropped, got {e:?}"
        ),
    }
}

/// **Bus close semantics.** After every agent shuts down, the last bus sender
/// drops and `inbox().recv()` returns `Err`.
async fn scenario_bus_close() {
    let coalition = Coalition::<ChatMessage>::new(vec!["erin".to_string()])
        .await
        .expect("coalition creation");
    let inbox = coalition.inbox();

    coalition.shutdown().await;

    let closed = timeout(Duration::from_secs(5), inbox.recv())
        .await
        .expect("recv did not resolve after shutdown");
    assert!(closed.is_err(), "inbox bus must be closed after shutdown");
}
