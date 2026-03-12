pub mod client;
pub mod metrics;
pub mod request;
pub mod run;
pub mod ssl;

use crate::cli::actions::client::APP_USER_AGENT;
use crate::cli::config;
use anyhow::{Result, anyhow};
use std::{collections::HashMap, env, path::PathBuf, sync::Arc};
use tokio::{process::Command, sync::Mutex};
use tracing::debug;

#[derive(Debug)]
pub enum Action {
    Run { config: PathBuf, port: u16 },
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FallbackState {
    pub consecutive_failures: usize,
    pub fallback_executions: usize,
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

/// Call the fallback HTTP request if the service is not reachable
async fn execute_fallback_http(url: &str) -> Result<i32> {
    let client = reqwest::Client::builder()
        .user_agent(APP_USER_AGENT)
        .build()?;

    let response = client.get(url).send().await?;

    let status = response.status();

    Ok(i32::from(status.as_u16()))
}

use std::hash::BuildHasher;

/// Check if stop limit is reached and if we should continue
async fn should_continue_fallback<S: BuildHasher>(
    service_name: &str,
    counters: &Arc<Mutex<HashMap<String, FallbackState, S>>>,
    action: &config::Action,
) -> bool {
    let mut counters = counters.lock().await;
    let state = counters.entry(service_name.to_string()).or_default();
    state.consecutive_failures += 1;

    let threshold = action.threshold.unwrap_or(1);
    if state.consecutive_failures < threshold {
        debug!(
            "Service '{}' failure count {}/{} below threshold, skipping fallback",
            service_name, state.consecutive_failures, threshold
        );
        return false;
    }

    // Check if we should stop processing
    if let Some(stop) = action.stop
        && state.fallback_executions >= stop
    {
        debug!(
            "Service '{}' reached stop limit ({}), skipping fallback",
            service_name, stop
        );
        return false;
    }

    state.fallback_executions += 1;

    true
}

async fn reset_fallback_state<S: BuildHasher>(
    service_name: &str,
    counters: &Arc<Mutex<HashMap<String, FallbackState, S>>>,
) {
    let mut counters = counters.lock().await;
    if let Some(state) = counters.get_mut(service_name) {
        state.consecutive_failures = 0;
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use mockito::Server;
    use std::{fs, os::unix::fs::PermissionsExt};

    #[tokio::test]
    async fn test_execute_fallback_command() {
        let exit_code = execute_fallback_command("exit 0")
            .await
            .expect("Failed to execute command");
        assert_eq!(exit_code, 0);

        let exit_code = execute_fallback_command("exit 1")
            .await
            .expect("Failed to execute command");
        assert_eq!(exit_code, 1);
    }

    #[tokio::test]
    async fn test_execute_fallback_command_runs_executable_script() {
        let tempdir = tempfile::Builder::new()
            .prefix("epazote-script-dir-")
            .tempdir_in(".")
            .expect("Failed to create temp dir");
        let script_path = tempdir.path().join("script.sh");
        fs::write(&script_path, "#!/bin/sh\nexit 7\n").expect("Failed to write script");

        let mut permissions = fs::metadata(&script_path)
            .expect("Failed to stat script")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).expect("Failed to chmod script");

        let exit_code = execute_fallback_command(script_path.to_str().expect("Invalid path"))
            .await
            .expect("Failed to execute script");

        assert_eq!(exit_code, 7);
    }

    #[tokio::test]
    async fn test_should_continue_fallback() {
        let counters = Arc::new(Mutex::new(HashMap::new()));
        let action = config::Action {
            stop: Some(2),
            ..Default::default()
        };

        let should_continue = should_continue_fallback("test", &counters, &action).await;
        assert!(should_continue);

        let should_continue = should_continue_fallback("test", &counters, &action).await;
        assert!(should_continue);

        let should_continue = should_continue_fallback("test", &counters, &action).await;
        assert!(!should_continue);
    }

    #[tokio::test]
    async fn test_should_continue_fallback_threshold() {
        let counters = Arc::new(Mutex::new(HashMap::new()));
        let action = config::Action {
            threshold: Some(3),
            ..Default::default()
        };

        assert!(!should_continue_fallback("test", &counters, &action).await);
        assert!(!should_continue_fallback("test", &counters, &action).await);
        assert!(should_continue_fallback("test", &counters, &action).await);

        let counters = counters.lock().await;
        let state = counters.get("test").expect("State not found");
        assert_eq!(state.consecutive_failures, 3);
        assert_eq!(state.fallback_executions, 1);
    }

    #[tokio::test]
    async fn test_reset_fallback_state() {
        let counters = Arc::new(Mutex::new(HashMap::new()));
        let action = config::Action {
            threshold: Some(2),
            ..Default::default()
        };

        assert!(!should_continue_fallback("test", &counters, &action).await);
        reset_fallback_state("test", &counters).await;
        assert!(!should_continue_fallback("test", &counters, &action).await);
        assert!(should_continue_fallback("test", &counters, &action).await);
    }

    #[tokio::test]
    async fn test_execute_fallback_http() {
        let mut server = Server::new_async().await;
        let _m = server.mock("GET", "/status/200").with_status(200).create();

        let exit_code = execute_fallback_http(format!("{}/status/200", &server.url()).as_str())
            .await
            .expect("Failed to execute HTTP fallback");

        assert_eq!(exit_code, 200);

        // bad request
        let exit_code = execute_fallback_http(format!("{}/status/400", &server.url()).as_str())
            .await
            .expect("Failed to execute HTTP fallback");

        assert_eq!(exit_code, 501);
    }

    #[tokio::test]
    async fn test_execute_fallback_http_error() {
        let rs = execute_fallback_http("telnet://0").await;

        assert!(rs.is_err());
    }
}
