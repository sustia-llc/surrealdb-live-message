use serde::{Deserialize, Serialize};
use surrealdb::sql::{Datetime, Id, Thing};
use surrealdb::opt::Resource;

use crate::connection;

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
    pub id: Thing,
    pub payload: Payload,
    pub from: Thing,
    pub created: Option<Datetime>,
    pub updated: Option<Datetime>,
}

pub const MESSAGE_HISTORY_TABLE: &str = "message_history";
#[derive(Debug, Serialize, Deserialize)]
pub struct MessageHistory {
    id: Thing,
    message: Message,
    created: Option<Datetime>,
}

pub async fn save_message_history(message: Message) -> surrealdb::Result<()> {
    let db = connection().await.to_owned();

    db.create(Resource::from(MESSAGE_HISTORY_TABLE))
        .content(MessageHistory {
            id: Thing::from((MESSAGE_HISTORY_TABLE, Id::rand())),
            message,
            created: Some(Datetime::default()),
        })
        .await?;

    Ok(())
}
