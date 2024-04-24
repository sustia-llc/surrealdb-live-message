use crate::settings::SETTINGS;
use std::env;

pub fn setup() {
    if env::var_os("RUST_LOG").is_none() {
        let app_name = env::var("CARGO_PKG_NAME").expect("Failed to get app name");
        let level = SETTINGS.logger.level.as_str();
        let env = format!("{app_name }={level}");

        env::set_var("RUST_LOG", env);
    }

    tracing_subscriber::fmt::init();
}
