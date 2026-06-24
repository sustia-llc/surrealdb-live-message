use config::{Config, ConfigError, Environment, File};
use serde::Deserialize;
use std::env;
use std::sync::LazyLock;

pub static SETTINGS: LazyLock<Settings> =
    LazyLock::new(|| Settings::new().expect("Failed to setup settings"));

#[derive(Debug, Clone, Deserialize)]
pub struct Logger {
    pub level: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Docker {
    pub platform: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Sdb {
    pub host: String,
    pub port: u16,
    pub image: String,
    pub tag: String,
    pub container_name: String,
    pub namespace: String,
    pub database: String,
    pub username: String,
    pub password: String,
    pub endpoint: String,
    /// Retention window (seconds) for the durable message log. Drives both the
    /// `message` table `CHANGEFEED` window (how far back a reconnecting agent can
    /// replay) and the periodic age-out sweep (`DELETE message WHERE created <
    /// now - retention`). Defaults to 24h. An agent offline longer than this
    /// loses the messages sent while it was gone.
    #[serde(default = "Sdb::default_message_retention_secs")]
    pub message_retention_secs: u64,
}

impl Sdb {
    fn default_message_retention_secs() -> u64 {
        86_400 // 24h
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Settings {
    pub environment: String,
    pub logger: Logger,
    pub docker: Docker,
    pub sdb: Sdb,
}

impl Settings {
    pub fn new() -> Result<Self, ConfigError> {
        let run_mode = env::var("RUN_MODE").unwrap_or_else(|_| "development".into());

        let builder = Config::builder()
            .add_source(File::with_name("config/default"))
            .add_source(File::with_name(&format!("config/{run_mode}")).required(false))
            .add_source(Environment::default().separator("__"));

        builder.build()?.try_deserialize()
    }
}
