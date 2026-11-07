//! A `match` outside the crate on every variant of
//! `surrealdb_live_message::error::Error`, with no wildcard arm.

use surrealdb_live_message::error::Error;

fn name(error: &Error) -> &'static str {
    match error {
        Error::Connect(_) => "Connect",
        Error::UseNsDb(_) => "UseNsDb",
        Error::Auth(_) => "Auth",
        Error::Schema(_) => "Schema",
        Error::AgentCreate { .. } => "AgentCreate",
        Error::AgentCreateEmpty { .. } => "AgentCreateEmpty",
        Error::AgentCreateTimeout { .. } => "AgentCreateTimeout",
        Error::InvalidAgentName { .. } => "InvalidAgentName",
        Error::DuplicateAgentName { .. } => "DuplicateAgentName",
        Error::Send { .. } => "Send",
        Error::UnknownRecipient { .. } => "UnknownRecipient",
        Error::LiveQuery { .. } => "LiveQuery",
        Error::Stream { .. } => "Stream",
        Error::LiveQueryIdMissing { .. } => "LiveQueryIdMissing",
        Error::LiveKill { .. } => "LiveKill",
        Error::LiveEndUnconfirmed { .. } => "LiveEndUnconfirmed",
        Error::LiveKillTimeout { .. } => "LiveKillTimeout",
        Error::CursorLoad { .. } => "CursorLoad",
        Error::CursorSave { .. } => "CursorSave",
        Error::CatchUp { .. } => "CatchUp",
        Error::ReadyTimeout { .. } => "ReadyTimeout",
        Error::ListenLoopDropped { .. } => "ListenLoopDropped",
        Error::Cancelled { .. } => "Cancelled",
        Error::ReadyChannelClosed => "ReadyChannelClosed",
        Error::HealthCheck => "HealthCheck",
        Error::StartupTimeout => "StartupTimeout",
        Error::Docker(_) => "Docker",
        Error::Http(_) => "Http",
    }
}

fn main() {
    println!("{}", name(&Error::HealthCheck));
}
