use serde::{Deserialize, Serialize};
use surrealdb_types::{Datetime, RecordId, SurrealValue};

#[derive(Debug, Serialize, Deserialize, SurrealValue)]
pub enum Payload {
    Text(TextPayload),
    Image(ImagePayload),
    Video(VideoPayload),
}

// Define the payload structures for each message type
#[derive(Debug, Serialize, Deserialize, SurrealValue)]
pub struct TextPayload {
    pub content: String,
}

#[derive(Debug, Serialize, Deserialize, SurrealValue)]
pub struct ImagePayload {
    pub url: String,
    pub caption: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, SurrealValue)]
pub struct VideoPayload {
    pub url: String,
    pub duration: u32, // Duration in seconds
}

pub const MESSAGE_TABLE: &str = "message";

#[derive(Debug, Serialize, Deserialize, SurrealValue)]
pub struct Message {
    // pub id: RecordId,
    pub r#in: Option<RecordId>,
    pub out: Option<RecordId>,
    pub payload: Payload,
    pub created: Option<Datetime>,
}