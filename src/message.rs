use serde::{Deserialize, Serialize};
use surrealdb_types::{Datetime, RecordId, SurrealValue};

pub const MESSAGE_TABLE: &str = "message";

/// A payload-generic message edge in the agent graph.
///
/// `T` is the user's payload type. It must derive `SurrealValue`, so that a
/// `message` record decodes into `Message<T>`.
///
/// The `SurrealValue` derive reads the field `r#in` from the wire key `in`,
/// and a bare `SELECT * FROM message` returns `in` and `out`.
#[derive(Debug, Serialize, Deserialize, SurrealValue)]
pub struct Message<T: SurrealValue> {
    /// The edge record's own id. Populated on **delivery** (the durable-log
    /// catch-up reconstructs it from the changefeed record, which carries `id`);
    /// `None` is fine when a payload is first sent (`RELATE` assigns the id).
    /// Lets consumers identify / deduplicate a delivery put on the bus more
    /// than once.
    pub id: Option<RecordId>,
    pub r#in: Option<RecordId>,
    pub out: Option<RecordId>,
    pub payload: T,
    pub created: Option<Datetime>,
}
