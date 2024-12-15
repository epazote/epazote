use crate::cli::{actions::Action, commands, dispatch::handler, globals::GlobalArgs, telemetry};
use anyhow::Result;

/// Start the CLI
pub fn start() -> Result<(Action, GlobalArgs)> {
    telemetry::init(None)?;

    let global_args = GlobalArgs::new();

    let matches = commands::new().get_matches();

    let action = handler(&matches)?;

    Ok((action, global_args))
}
