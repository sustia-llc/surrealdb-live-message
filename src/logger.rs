use crate::settings::SETTINGS;
use std::str::FromStr;
use tracing::Level;

pub fn setup() {
    let level = match Level::from_str(SETTINGS.logger.level.as_str()) {
        Ok(level) => level,
        Err(_) => {
            eprintln!("Invalid log level: {}, defaulting to INFO", SETTINGS.logger.level);
            Level::INFO
        }
    };

    tracing_subscriber::fmt()
        .with_max_level(level)
        .init();
}
