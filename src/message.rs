use serde::{Deserialize, Serialize};
use surrealdb_types::{Datetime, RecordId, SurrealValue};

pub const MESSAGE_TABLE: &str = "message";

/// A payload-generic message edge in the agent graph.
///
/// `T` is the user's payload type. It must derive `SurrealValue` so the
/// live-query stream can deserialize it from the `message` table.
///
/// **Two SDK gotchas folded into this type:**
///
/// 1. **`SurrealValue` derive uses raw Rust identifiers as wire keys.**
///    A field named `r#in` serializes to/from `"r#in"`, not `"in"`. SurrealDB
///    emits `"in"` for the edge source, so the Rust field must be renamed
///    via `#[surreal(rename = "in")]` (serde rename is ignored by the
///    `SurrealValue` derive). Without this, `Option<RecordId>` silently
///    deserializes to `None`.
///
/// 2. **A bare `SELECT *` / `LIVE SELECT *` on edge records omits `in`/`out`.**
///    Read the edge pointers with an explicit projection —
///    `SELECT *, in, out FROM message WHERE ...`. The two-tier durable bus
///    sidesteps this on the delivery path: `SHOW CHANGES` changeset records
///    carry `id`/`in`/`out` natively, so the wake-up subscription in
///    `agents::Agent::listen_loop` is only `LIVE SELECT id`.
#[derive(Debug, Serialize, Deserialize, SurrealValue)]
pub struct Message<T: SurrealValue> {
    /// The edge record's own id. Populated on **delivery** (the durable-log
    /// catch-up reconstructs it from the changefeed record, which carries `id`);
    /// `None` is fine when a payload is first sent (`RELATE` assigns the id).
    /// Lets consumers identify / deduplicate a delivery under the at-least-once
    /// guarantee of the two-tier durable bus.
    pub id: Option<RecordId>,
    #[surreal(rename = "in")]
    pub r#in: Option<RecordId>,
    pub out: Option<RecordId>,
    pub payload: T,
    pub created: Option<Datetime>,
}
