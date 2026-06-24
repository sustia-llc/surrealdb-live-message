//! Typed error surface for the library.
//!
//! Library code returns [`Result`] (aliasing this crate's [`Error`]). The
//! application layer (`main.rs`, integration tests) keeps using `anyhow`, which
//! absorbs these via the blanket `From<E: std::error::Error + Send + Sync +
//! 'static>` impl — every variant's `#[source]`/`#[from]` type is `Send + Sync
//! + 'static`, so `Error` is too.

use std::time::Duration;

/// All fallible operations in the library funnel through this enum.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("failed to connect to SurrealDB")]
    Connect(#[source] surrealdb::Error),

    #[error("failed to select namespace/database")]
    UseNsDb(#[source] surrealdb::Error),

    #[error("failed to authenticate with SurrealDB")]
    Auth(#[source] surrealdb::Error),

    #[error("failed to apply SurrealDB schema")]
    Schema(#[source] surrealdb::Error),

    #[error("failed to create agent '{agent}'")]
    AgentCreate {
        agent: String,
        #[source]
        source: surrealdb::Error,
    },

    #[error("creating agent '{agent}' returned no record")]
    AgentCreateEmpty { agent: String },

    #[error(
        "invalid agent name '{name}': must be a non-empty record-id key \
         (ASCII alphanumeric or underscore)"
    )]
    InvalidAgentName { name: String },

    #[error("failed to send message to '{to}'")]
    Send {
        to: String,
        #[source]
        source: surrealdb::Error,
    },

    #[error("recipient agent '{to}' does not exist")]
    UnknownRecipient { to: String },

    #[error("LIVE SELECT registration failed for agent '{agent}'")]
    LiveQuery {
        agent: String,
        #[source]
        source: surrealdb::Error,
    },

    #[error("failed to open message stream for agent '{agent}'")]
    Stream {
        agent: String,
        #[source]
        source: surrealdb::Error,
    },

    #[error("failed to load the durable-log cursor for agent '{agent}'")]
    CursorLoad {
        agent: String,
        #[source]
        source: surrealdb::Error,
    },

    #[error("failed to persist the durable-log cursor for agent '{agent}'")]
    CursorSave {
        agent: String,
        #[source]
        source: surrealdb::Error,
    },

    #[error("durable-log catch-up (SHOW CHANGES) failed for agent '{agent}'")]
    CatchUp {
        agent: String,
        #[source]
        source: surrealdb::Error,
    },

    #[error("agent '{agent}' listen_loop did not signal ready within {timeout:?}")]
    ReadyTimeout { agent: String, timeout: Duration },

    #[error("agent '{agent}' listen_loop dropped before ready signal")]
    ListenLoopDropped { agent: String },

    #[error("database ready channel closed")]
    ReadyChannelClosed,

    #[error("SurrealDB health check failed after all attempts")]
    HealthCheck,

    #[error("timed out waiting for SurrealDB to start")]
    StartupTimeout,

    #[error(transparent)]
    Docker(#[from] bollard::errors::Error),

    #[error(transparent)]
    Http(#[from] reqwest::Error),
}

/// Crate-wide result alias over [`Error`].
pub type Result<T> = std::result::Result<T, Error>;
