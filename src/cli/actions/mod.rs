pub mod client;
pub mod metrics;
pub mod request;
pub mod run;
pub mod ssl;

use anyhow::{anyhow, Result};
use std::{env, path::PathBuf};
use tokio::process::Command;

#[derive(Debug)]
pub enum Action {
    Run { config: PathBuf, port: u16 },
}

async fn execute_fallback_command(cmd: &str) -> Result<i32> {
    let shell = env::var("SHELL").unwrap_or_else(|_| "sh".to_string());
    let output = Command::new(shell).arg("-c").arg(cmd).output().await?;

    let exit_code = match output.status.code() {
        Some(code) => code,
        None => Err(anyhow!("Process terminated by signal"))?,
    };

    Ok(exit_code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_execute_fallback_command() {
        let exit_code = execute_fallback_command("exit 0").await.unwrap();
        assert_eq!(exit_code, 0);

        let exit_code = execute_fallback_command("exit 1").await.unwrap();
        assert_eq!(exit_code, 1);
    }
}
