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
/// 2. **`LIVE SELECT *` on edge records omits `in`/`out`.** The subscription
///    must be `LIVE SELECT *, in, out FROM message WHERE ...`. See
///    `agents::Agent::listen_loop`.
#[derive(Debug, Serialize, Deserialize, SurrealValue)]
pub struct Message<T: SurrealValue> {
    #[surreal(rename = "in")]
    pub r#in: Option<RecordId>,
    pub out: Option<RecordId>,
    pub payload: T,
    pub created: Option<Datetime>,
}
