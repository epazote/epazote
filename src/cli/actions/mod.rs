pub mod client;
pub mod metrics;
pub mod request;
pub mod run;
pub mod ssl;

use crate::cli::config;
use anyhow::{anyhow, Result};
use std::{collections::HashMap, env, path::PathBuf, sync::Arc};
use tokio::{process::Command, sync::Mutex};
use tracing::debug;

#[derive(Debug)]
pub enum Action {
    Run { config: PathBuf, port: u16 },
}

/// Call the fallback command if the service is not reachable
async fn execute_fallback_command(cmd: &str) -> Result<i32> {
    let shell = env::var("SHELL").unwrap_or_else(|_| "sh".to_string());
    let output = Command::new(shell).arg("-c").arg(cmd).output().await?;

    let exit_code = match output.status.code() {
        Some(code) => code,
        None => Err(anyhow!("Process terminated by signal"))?,
    };

    Ok(exit_code)
}

/// Check if stop limit is reached and if we should continue
async fn should_continue_fallback(
    service_name: &str,
    counters: &Arc<Mutex<HashMap<String, usize>>>,
    action: &config::Action,
) -> bool {
    let mut counters = counters.lock().await;
    let count = counters.entry(service_name.to_string()).or_insert(0);

    // Check if we should stop processing
    if let Some(stop) = action.stop {
        if *count >= stop {
            debug!(
                "Service '{}' reached stop limit ({}), skipping fallback",
                service_name, stop
            );
            return false;
        }
    }

    *count += 1;

    drop(counters); // Explicitly drop the lock

    true
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

    #[tokio::test]
    async fn test_should_continue_fallback() {
        let counters = Arc::new(Mutex::new(HashMap::new()));
        let action = config::Action {
            stop: Some(2),
            ..Default::default()
        };

        let should_continue = should_continue_fallback("test", &counters, &action).await;
        assert_eq!(should_continue, true);

        let should_continue = should_continue_fallback("test", &counters, &action).await;
        assert_eq!(should_continue, true);

        let should_continue = should_continue_fallback("test", &counters, &action).await;
        assert_eq!(should_continue, false);
    }
}
