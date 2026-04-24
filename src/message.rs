use serde::{Deserialize, Serialize};
use surrealdb_types::{Datetime, RecordId, SurrealValue};

pub const MESSAGE_TABLE: &str = "message";

/// A payload-generic message edge in the agent graph.
///
/// `T` is the user's payload type. It must derive `SurrealValue` so the
/// live-query stream can deserialize it from the `message` table.
#[derive(Debug, Serialize, Deserialize, SurrealValue)]
pub struct Message<T: SurrealValue> {
    pub r#in: Option<RecordId>,
    pub out: Option<RecordId>,
    pub payload: T,
    pub created: Option<Datetime>,
}
