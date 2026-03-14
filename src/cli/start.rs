use crate::cli::{actions::Action, commands, dispatch::handler, telemetry};
use anyhow::Result;

/// Start the CLI
///
/// # Errors
///
/// Returns an error if telemetry initialization fails or command handling encounters an issue.
pub fn start() -> Result<Action> {
    commands::normalize_env_vars();
    let matches = commands::new().get_matches();
    let json_logs = matches.get_flag("json-logs");

    let verbosity_level = match matches.get_count("verbose") {
        0 => None,
        1 => Some(tracing::Level::INFO),
        2 => Some(tracing::Level::DEBUG),
        _ => Some(tracing::Level::TRACE),
    };

    telemetry::init(verbosity_level, json_logs)?;

    let action = handler(&matches);

    Ok(action)
}
