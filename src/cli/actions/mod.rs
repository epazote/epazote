pub mod client;
pub mod metrics;
pub mod request;
pub mod run;
pub mod ssl;

use crate::cli::actions::client::APP_USER_AGENT;
use crate::cli::config;
use anyhow::{Result, anyhow};
use std::{collections::HashMap, env, path::PathBuf, sync::Arc, sync::LazyLock};
use tokio::{process::Command, sync::Mutex};
use tracing::{info, warn};

#[derive(Debug)]
pub enum Action {
    Run { config: PathBuf, port: u16 },
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FallbackState {
    pub consecutive_failures: usize,
    pub fallback_executions: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackServiceType {
    Http,
    Command,
}

impl FallbackServiceType {
    const fn as_env_value(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Command => "command",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FallbackContext<'a> {
    pub service_name: &'a str,
    pub service_type: FallbackServiceType,
    pub expected_status: Option<i32>,
    pub actual_status: Option<i32>,
    pub error: &'a str,
    pub failure_count: usize,
    pub threshold: usize,
    pub url: Option<&'a str>,
    pub test: Option<&'a str>,
}

impl FallbackContext<'_> {
    fn env_vars(&self) -> Vec<(&'static str, String)> {
        let mut vars = vec![
            ("EPAZOTE_SERVICE_NAME", self.service_name.to_string()),
            (
                "EPAZOTE_SERVICE_TYPE",
                self.service_type.as_env_value().to_string(),
            ),
            ("EPAZOTE_ERROR", self.error.to_string()),
            ("EPAZOTE_FAILURE_COUNT", self.failure_count.to_string()),
            ("EPAZOTE_THRESHOLD", self.threshold.to_string()),
        ];

        if let Some(expected_status) = self.expected_status {
            vars.push(("EPAZOTE_EXPECTED_STATUS", expected_status.to_string()));
        }

        if let Some(actual_status) = self.actual_status {
            vars.push(("EPAZOTE_ACTUAL_STATUS", actual_status.to_string()));
        }

        if let Some(url) = self.url {
            vars.push(("EPAZOTE_URL", url.to_string()));
        }

        if let Some(test) = self.test {
            vars.push(("EPAZOTE_TEST", test.to_string()));
        }

        vars
    }
}

static SYSTEM_SHELL: LazyLock<String> =
    LazyLock::new(|| env::var("SHELL").unwrap_or_else(|_| "sh".to_string()));

async fn execute_shell_command(cmd: &str, context: Option<&FallbackContext<'_>>) -> Result<i32> {
    let mut command = Command::new(SYSTEM_SHELL.as_str());
    command.arg("-c").arg(cmd);

    if let Some(context) = context {
        command.envs(context.env_vars());
    }

    let output = command.output().await?;

    let exit_code = match output.status.code() {
        Some(code) => code,
        None => Err(anyhow!("Process terminated by signal"))?,
    };

    Ok(exit_code)
}

pub(crate) async fn execute_command(cmd: &str) -> Result<i32> {
    execute_shell_command(cmd, None).await
}

/// Call the fallback command if the service is not reachable
pub(crate) async fn execute_fallback_command(
    cmd: &str,
    context: &FallbackContext<'_>,
) -> Result<i32> {
    execute_shell_command(cmd, Some(context)).await
}

static FALLBACK_HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .user_agent(APP_USER_AGENT)
        .build()
        .unwrap_or_default()
});

/// Call the fallback HTTP request if the service is not reachable
async fn execute_fallback_http(url: &str) -> Result<i32> {
    let response = FALLBACK_HTTP_CLIENT.get(url).send().await?;

    let status = response.status();

    // Consume the body to release the connection back to the pool
    let _ = response.bytes().await;

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
        warn!(
            "Service '{}' failure count {}/{} below threshold, skipping fallback",
            service_name, state.consecutive_failures, threshold
        );
        return false;
    }

    // Check if we should stop processing
    if let Some(stop) = action.stop
        && state.fallback_executions >= stop
    {
        warn!(
            "Service '{}' reached stop limit ({}), skipping fallback",
            service_name, stop
        );
        return false;
    }

    state.fallback_executions += 1;

    let stop_info = action
        .stop
        .map_or_else(|| "unlimited".to_string(), |s| s.to_string());

    info!(
        "Service '{}' threshold reached ({}/{}), executing fallback (execution #{}/{})",
        service_name, state.consecutive_failures, threshold, state.fallback_executions, stop_info
    );

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

async fn get_fallback_state<S: BuildHasher>(
    service_name: &str,
    counters: &Arc<Mutex<HashMap<String, FallbackState, S>>>,
) -> Option<FallbackState> {
    let counters = counters.lock().await;
    counters.get(service_name).copied()
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use mockito::Server;
    use std::{fs, os::unix::fs::PermissionsExt};

    #[tokio::test]
    async fn test_execute_command() {
        let exit_code = execute_command("exit 0")
            .await
            .expect("Failed to execute command");
        assert_eq!(exit_code, 0);

        let exit_code = execute_command("exit 1")
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

        let context = FallbackContext {
            service_name: "test",
            service_type: FallbackServiceType::Command,
            expected_status: Some(0),
            actual_status: Some(1),
            error: "command_failed",
            failure_count: 1,
            threshold: 1,
            url: None,
            test: Some("exit 1"),
        };

        let exit_code =
            execute_fallback_command(script_path.to_str().expect("Invalid path"), &context)
                .await
                .expect("Failed to execute script");

        assert_eq!(exit_code, 7);
    }

    #[tokio::test]
    async fn test_execute_fallback_command_sets_context_env_vars() {
        let tempdir = tempfile::Builder::new()
            .prefix("epazote-env-dir-")
            .tempdir_in(".")
            .expect("Failed to create temp dir");
        let script_path = tempdir.path().join("script.sh");
        let output_path = tempdir.path().join("env.txt");
        fs::write(
            &script_path,
            format!(
                "#!/bin/sh\nprintenv EPAZOTE_SERVICE_NAME > {}\nprintenv EPAZOTE_SERVICE_TYPE >> {}\nprintenv EPAZOTE_ERROR >> {}\nprintenv EPAZOTE_FAILURE_COUNT >> {}\nprintenv EPAZOTE_THRESHOLD >> {}\nprintenv EPAZOTE_ACTUAL_STATUS >> {}\n",
                output_path.display(),
                output_path.display(),
                output_path.display(),
                output_path.display(),
                output_path.display(),
                output_path.display()
            ),
        )
        .expect("Failed to write script");

        let mut permissions = fs::metadata(&script_path)
            .expect("Failed to stat script")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).expect("Failed to chmod script");

        let context = FallbackContext {
            service_name: "vmagent",
            service_type: FallbackServiceType::Http,
            expected_status: Some(200),
            actual_status: Some(503),
            error: "status_mismatch",
            failure_count: 3,
            threshold: 3,
            url: Some("http://127.0.0.1:8429/api/v1/targets"),
            test: None,
        };

        let exit_code =
            execute_fallback_command(script_path.to_str().expect("Invalid path"), &context)
                .await
                .expect("Failed to execute script");

        assert_eq!(exit_code, 0);

        let output = fs::read_to_string(output_path).expect("Failed to read env output");
        assert_eq!(
            output.lines().collect::<Vec<_>>(),
            vec!["vmagent", "http", "status_mismatch", "3", "3", "503"]
        );
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
    async fn test_get_fallback_state() {
        let counters = Arc::new(Mutex::new(HashMap::new()));
        let action = config::Action {
            threshold: Some(2),
            ..Default::default()
        };

        assert!(!should_continue_fallback("test", &counters, &action).await);

        let state = get_fallback_state("test", &counters)
            .await
            .expect("State not found");
        assert_eq!(state.consecutive_failures, 1);
        assert_eq!(state.fallback_executions, 0);
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
