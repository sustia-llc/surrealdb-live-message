use serde::{Deserialize, Serialize};
use surrealdb::sql::{Datetime, Thing};

#[derive(Debug, Serialize, Deserialize)]
pub enum Payload {
    Text(TextPayload),
    Image(ImagePayload),
    Video(VideoPayload),
}

// Define the payload structures for each message type
#[derive(Debug, Serialize, Deserialize)]
pub struct TextPayload {
    pub content: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ImagePayload {
    pub url: String,
    pub caption: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct VideoPayload {
    pub url: String,
    pub duration: u32, // Duration in seconds
}

pub const MESSAGE_TABLE: &str = "message";

#[derive(Debug, Serialize, Deserialize)]
pub struct Message {
    // pub id: Thing,
    pub r#in: Thing,
    pub out: Thing,
    pub payload: Payload,
    pub created: Option<Datetime>,
}