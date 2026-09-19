use anyhow::Result;
use epazote::cli::{actions, actions::Action, start, telemetry};

// Main function
#[tokio::main]
async fn main() -> Result<()> {
    // Start the program
    let action = start()?;

    let action_result = match action {
        Action::Run { .. } => actions::run::handle(action).await,
    };
    let shutdown_result = telemetry::shutdown();

    action_result?;
    shutdown_result
}
