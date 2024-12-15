use anyhow::Result;
use epazote::cli::{actions, actions::Action, start};

// Main function
#[tokio::main]
async fn main() -> Result<()> {
    // Start the program
    let (action, globals) = start()?;

    match action {
        Action::Run { .. } => actions::run::handle(action, globals).await?,
    }

    Ok(())
}
