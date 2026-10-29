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
    Connect(#[source] Box<surrealdb::Error>),

    #[error("failed to select namespace/database")]
    UseNsDb(#[source] Box<surrealdb::Error>),

    #[error("failed to authenticate with SurrealDB")]
    Auth(#[source] Box<surrealdb::Error>),

    #[error("failed to apply SurrealDB schema")]
    Schema(#[source] Box<surrealdb::Error>),

    #[error("failed to create agent '{agent}'")]
    AgentCreate {
        agent: String,
        #[source]
        source: Box<surrealdb::Error>,
    },

    #[error("creating agent '{agent}' returned no record")]
    AgentCreateEmpty { agent: String },

    /// `Agent::new` for `agent`, called by `Coalition::new`, did not finish
    /// within `timeout`.
    #[error("creating agent '{agent}' did not finish within {timeout:?}")]
    AgentCreateTimeout { agent: String, timeout: Duration },

    #[error(
        "invalid agent name '{name}': must be a non-empty record-id key \
         (ASCII alphanumeric or underscore)"
    )]
    InvalidAgentName { name: String },

    #[error("agent name '{name}' appears more than once in one coalition")]
    DuplicateAgentName { name: String },

    #[error("failed to send message to '{to}'")]
    Send {
        to: String,
        #[source]
        source: Box<surrealdb::Error>,
    },

    #[error("recipient agent '{to}' does not exist")]
    UnknownRecipient { to: String },

    #[error("LIVE SELECT registration failed for agent '{agent}'")]
    LiveQuery {
        agent: String,
        #[source]
        source: Box<surrealdb::Error>,
    },

    #[error("failed to open message stream for agent '{agent}'")]
    Stream {
        agent: String,
        #[source]
        source: Box<surrealdb::Error>,
    },

    #[error("LIVE SELECT for agent '{agent}' returned no live-query id")]
    LiveQueryIdMissing { agent: String },

    #[error("failed to KILL live query {live_id} for agent '{agent}'")]
    LiveKill {
        agent: String,
        live_id: surrealdb_types::Uuid,
        #[source]
        source: Box<surrealdb::Error>,
    },

    #[error("KILL of live query {live_id} for agent '{agent}' did not finish within {timeout:?}")]
    LiveKillTimeout {
        agent: String,
        live_id: surrealdb_types::Uuid,
        timeout: Duration,
    },

    #[error("failed to load the durable-log cursor for agent '{agent}'")]
    CursorLoad {
        agent: String,
        #[source]
        source: Box<surrealdb::Error>,
    },

    #[error("failed to persist the durable-log cursor for agent '{agent}'")]
    CursorSave {
        agent: String,
        #[source]
        source: Box<surrealdb::Error>,
    },

    #[error("durable-log catch-up (SHOW CHANGES) failed for agent '{agent}'")]
    CatchUp {
        agent: String,
        #[source]
        source: Box<surrealdb::Error>,
    },

    #[error("agent '{agent}' listen_loop did not signal ready within {timeout:?}")]
    ReadyTimeout { agent: String, timeout: Duration },

    #[error("agent '{agent}' listen_loop ended before its ready signal without a cancel")]
    ListenLoopDropped { agent: String },

    /// The coalition's token was cancelled before every listen loop was
    /// ready. `agent` names the agent whose startup the cancel interrupted,
    /// or is `None` for a cancel `Coalition::new` found outside any one
    /// agent's startup.
    #[error("coalition token was cancelled before {who} was ready", who = cancelled_who(.agent))]
    Cancelled { agent: Option<String> },

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

/// `agent 'name'` for `Some(name)`, `every agent` for `None`.
fn cancelled_who(agent: &Option<String>) -> String {
    agent
        .as_deref()
        .map_or_else(|| "every agent".to_string(), |a| format!("agent '{a}'"))
}

/// Render `err` and every error in its [`source`](std::error::Error::source)
/// chain, outermost first, joined with `": "`.
pub fn chain(err: &(dyn std::error::Error + 'static)) -> String {
    std::iter::successors(Some(err), |e| e.source())
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(": ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`chain`] renders the inner surrealdb error of [`Error::LiveKill`],
    /// including that error's own `cause`.
    #[test]
    fn chain_renders_live_kill_source() {
        let inner = surrealdb::Error::internal("kill refused".to_string())
            .with_cause(surrealdb::Error::internal("session closed".to_string()));
        let err = Error::LiveKill {
            agent: "frank".to_string(),
            live_id: surrealdb_types::Uuid::new_v4(),
            source: Box::new(inner),
        };
        let rendered = chain(&err);
        for needle in [
            "failed to KILL live query",
            "kill refused",
            "session closed",
        ] {
            assert!(
                rendered.contains(needle),
                "chain(LiveKill) must contain {needle:?}; rendered {rendered:?}"
            );
        }
    }
}
