pub mod client;
pub mod metrics;
pub mod request;
pub mod run;
pub mod ssl;

use crate::cli::actions::client::APP_USER_AGENT;
use crate::cli::config;
use anyhow::{Result, anyhow};
use std::{
    collections::HashMap, env, path::PathBuf, process::Stdio, sync::Arc, sync::LazyLock,
    time::Duration,
};
use tokio::{
    io::AsyncReadExt as _,
    process::{ChildStderr, Command},
    sync::Mutex,
    time,
};
use tracing::{debug, info, warn};

#[derive(Debug)]
pub enum Action {
    Run {
        config: PathBuf,
        bind: String,
        port: u16,
    },
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

// Cap on how much of a command's stderr is retained for logging. The pipe is
// always drained to EOF so the child never blocks on a full pipe, but only
// this much is kept in memory.
const STDERR_CAPTURE_LIMIT: usize = 8 * 1024;

/// Kills the whole process group of a timed-out command. Killing only the
/// spawned shell leaves any children it started running, so a `sleep` inside
/// `cmd; other` survives and leaks on every scan.
#[cfg(unix)]
fn kill_process_group(pid: u32) {
    let Ok(group) = i32::try_from(pid) else {
        return;
    };

    // Safe: `group` is the child's own pid, and `process_group(0)` made it the
    // leader of a new group, so this can only signal that group.
    unsafe {
        libc::killpg(group, libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill_process_group(_pid: u32) {}

/// Drains `stderr` to EOF, keeping at most `STDERR_CAPTURE_LIMIT` bytes.
async fn capture_stderr(stderr: Option<ChildStderr>) -> Result<Vec<u8>> {
    let mut kept = Vec::new();

    let Some(mut stderr) = stderr else {
        return Ok(kept);
    };

    let mut buffer = [0u8; 4096];

    loop {
        let read = stderr.read(&mut buffer).await?;

        if read == 0 {
            break;
        }

        // Keep draining past the cap so the child is never blocked writing,
        // but stop growing the buffer.
        if kept.len() < STDERR_CAPTURE_LIMIT
            && let Some(chunk) = buffer.get(..read)
        {
            let room = STDERR_CAPTURE_LIMIT - kept.len();
            kept.extend_from_slice(chunk.get(..room.min(read)).unwrap_or(chunk));
        }
    }

    Ok(kept)
}

async fn execute_shell_command(
    cmd: &str,
    context: Option<&FallbackContext<'_>>,
    timeout: Duration,
) -> Result<i32> {
    let mut command = Command::new(SYSTEM_SHELL.as_str());
    command.arg("-c").arg(cmd);

    // stdout is never inspected, so discard it instead of buffering it.
    command.stdout(Stdio::null());
    command.stderr(Stdio::piped());

    // Without this a command that outlives the timeout below keeps running
    // after the future is dropped, leaking a process on every scan.
    command.kill_on_drop(true);

    // Put the child in its own process group so a timeout can signal its
    // descendants too, not just the shell.
    #[cfg(unix)]
    command.process_group(0);

    if let Some(context) = context {
        command.envs(context.env_vars());
    }

    let mut child = command.spawn()?;
    let child_pid = child.id();
    let stderr = child.stderr.take();

    // A command with no timeout blocks the service task forever, silently
    // stopping every future scan for that service.
    let result = time::timeout(timeout, async {
        let stderr = capture_stderr(stderr).await?;
        let status = child.wait().await?;

        Ok::<_, anyhow::Error>((status, stderr))
    })
    .await;

    let Ok(output) = result else {
        if let Some(pid) = child_pid {
            kill_process_group(pid);
        }

        return Err(anyhow!(
            "command exceeded the service 'timeout' of {timeout:?}: {cmd}"
        ));
    };

    let (status, stderr) = output?;

    let exit_code = match status.code() {
        Some(code) => code,
        None => Err(anyhow!("Process terminated by signal"))?,
    };

    if !stderr.is_empty() {
        let stderr = String::from_utf8_lossy(&stderr);
        let stderr = stderr.trim_end();

        // Commands legitimately write to stderr while succeeding, so only a
        // failing command warrants a warning.
        if exit_code == 0 {
            debug!("Command stderr: {stderr}");
        } else {
            warn!("Command stderr: {stderr}");
        }
    }

    Ok(exit_code)
}

pub(crate) async fn execute_command(cmd: &str, timeout: Duration) -> Result<i32> {
    execute_shell_command(cmd, None, timeout).await
}

/// Call the fallback command if the service is not reachable
pub(crate) async fn execute_fallback_command(
    cmd: &str,
    context: &FallbackContext<'_>,
    timeout: Duration,
) -> Result<i32> {
    execute_shell_command(cmd, Some(context), timeout).await
}

static FALLBACK_HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .user_agent(APP_USER_AGENT)
        .build()
        .unwrap_or_default()
});

/// Call the fallback HTTP request if the service is not reachable
async fn execute_fallback_http(url: &str, timeout: Duration) -> Result<i32> {
    // The shared client has no timeout of its own, so an unresponsive alert
    // endpoint would hang both the request and the body read, stalling every
    // later scan for this service.
    let response = FALLBACK_HTTP_CLIENT
        .get(url)
        .timeout(timeout)
        .send()
        .await?;

    let status = response.status();

    // Consume the body to release the connection back to the pool. The request
    // was delivered, so a body that fails afterwards does not undo the alert -
    // but it must not be swallowed either, or a partial response reads as a
    // clean success in the logs.
    if let Err(error) = response.bytes().await {
        warn!(
            "Fallback HTTP request to {url} answered {status} but its body could not be read: {error}"
        );
    }

    Ok(i32::from(status.as_u16()))
}

/// Run the configured fallback actions (command then HTTP) for a failed service.
///
/// Both actions are optional and independent; whichever are present in the
/// `if_not` configuration are executed in order.
pub(crate) async fn execute_fallbacks(
    action: &config::Action,
    context: &FallbackContext<'_>,
    service_name: &str,
) -> Result<()> {
    let mut first_error = None;

    if let Some(cmd) = &action.cmd {
        // Recovery actions get their own budget, not the health-probe timeout.
        match execute_fallback_command(cmd, context, action.fallback_timeout()).await {
            Ok(exit_code) => {
                info!("Executed fallback command for {service_name} with exit code {exit_code}");
            }
            Err(error) => {
                // The actions are independent: a failing command must not
                // skip a configured HTTP alert, which is often the only way
                // the failure gets noticed.
                warn!("Fallback command for {service_name} failed: {error}");
                first_error = Some(error);
            }
        }
    }

    if let Some(http) = &action.http {
        match execute_fallback_http(http, action.fallback_timeout()).await {
            Ok(status) => {
                info!(
                    "Executed fallback HTTP request for {service_name} with status code {status}"
                );
            }
            Err(error) => {
                warn!("Fallback HTTP request for {service_name} failed: {error}");
                first_error = first_error.or(Some(error));
            }
        }
    }

    first_error.map_or(Ok(()), Err)
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

    state.fallback_executions += 1;

    // Check if we should stop processing AFTER this execution
    if let Some(stop) = action.stop
        && state.fallback_executions > stop
    {
        warn!(
            "Service '{}' reached stop limit ({}), skipping fallback",
            service_name, stop
        );
        state.fallback_executions -= 1; // Revert the increment since we're not executing
        return false;
    }

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
        state.fallback_executions = 0;
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
        let exit_code = execute_command("exit 0", Duration::from_secs(30))
            .await
            .expect("Failed to execute command");
        assert_eq!(exit_code, 0);

        let exit_code = execute_command("exit 1", Duration::from_secs(30))
            .await
            .expect("Failed to execute command");
        assert_eq!(exit_code, 1);
    }

    /// Regression: the shared fallback client has no timeout of its own, so an
    /// alert endpoint that accepts the connection and never answers would hang
    /// the request and stall every later scan for that service.
    #[tokio::test]
    async fn test_execute_fallback_http_times_out_on_silent_endpoint() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("Failed to bind listener");
        let addr = listener.local_addr().expect("Failed to get local addr");

        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                held.push(stream);
            }
        });

        let result = tokio::time::timeout(
            Duration::from_secs(10),
            execute_fallback_http(&format!("http://{addr}/alert"), Duration::from_millis(300)),
        )
        .await
        .expect("the fallback request must not hang past its timeout");

        assert!(
            result.is_err(),
            "a silent endpoint should fail the fallback request"
        );
    }

    /// Regression: killing only the spawned shell leaves its children running.
    /// A timed-out command must take its whole process group with it.
    #[tokio::test]
    #[cfg(unix)]
    async fn test_execute_command_timeout_kills_descendants() {
        let tempdir = tempfile::Builder::new()
            .prefix("epazote-pgroup-")
            .tempdir_in(".")
            .expect("Failed to create temp dir");
        let marker = tempdir.path().join("descendant.txt");

        // The grandchild outlives the timeout and writes the marker itself, so
        // this cannot pass merely because the shell stopped early.
        let cmd = format!("( sleep 3; echo alive > {} ) & sleep 30", marker.display());

        let result = execute_command(&cmd, Duration::from_millis(200)).await;
        assert!(result.is_err(), "command should time out");

        tokio::time::sleep(Duration::from_secs(5)).await;

        assert!(
            !marker.exists(),
            "descendants of a timed-out command must be killed, not just the shell"
        );
    }

    /// Regression: a failing fallback command must not skip the HTTP action.
    /// They are documented as independent, and the HTTP alert is often the
    /// only way an operator learns the recovery failed.
    #[tokio::test]
    async fn test_execute_fallbacks_runs_http_when_command_times_out() {
        let mut server = Server::new_async().await;
        let mock = server
            .mock("GET", "/alert")
            .with_status(200)
            .create_async()
            .await;

        let action = config::Action {
            cmd: Some("sleep 30".to_string()),
            http: Some(format!("{}/alert", server.url())),
            stop: None,
            threshold: Some(1),
            timeout: Some(Duration::from_millis(200)),
        };

        let context = FallbackContext {
            service_name: "independent",
            service_type: FallbackServiceType::Command,
            expected_status: Some(0),
            actual_status: Some(1),
            error: "command_failed",
            failure_count: 1,
            threshold: 1,
            url: None,
            test: None,
        };

        let result = execute_fallbacks(&action, &context, "independent").await;

        assert!(
            result.is_err(),
            "the timed-out command should still be reported"
        );
        mock.assert_async().await;
    }

    /// stderr from a successful command must not be logged as a warning:
    /// plenty of healthy commands write to stderr.
    #[tokio::test]
    async fn test_execute_command_succeeds_with_stderr_output() {
        let exit_code = execute_command("echo noise >&2; exit 0", Duration::from_secs(30))
            .await
            .expect("command should succeed");

        assert_eq!(exit_code, 0);
    }

    /// A command producing far more stderr than the capture limit must still
    /// complete: the pipe is drained even though only part is retained.
    #[tokio::test]
    async fn test_execute_command_drains_large_stderr() {
        let cmd = format!(
            "for i in $(seq 1 {}); do echo 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa' >&2; done; exit 0",
            STDERR_CAPTURE_LIMIT / 8
        );

        let exit_code = execute_command(&cmd, Duration::from_secs(30))
            .await
            .expect("a command writing lots of stderr must not block");

        assert_eq!(exit_code, 0);
    }

    /// Regression: command execution must be bounded by the service timeout.
    /// Without it a hanging command blocks the service task forever.
    #[tokio::test]
    async fn test_execute_command_times_out() {
        let started = std::time::Instant::now();
        let result = execute_command("sleep 30", Duration::from_millis(200)).await;

        let error = result.expect_err("a hanging command must return an error");
        assert!(
            format!("{error:#}").contains("timeout"),
            "unexpected error: {error:#}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "execute_command should return near the timeout, took {:?}",
            started.elapsed()
        );
    }

    /// Regression: a timed-out command must be killed, not left running.
    /// Without `kill_on_drop` the child survives the dropped future and leaks
    /// a process on every scan.
    #[tokio::test]
    async fn test_execute_command_timeout_kills_child() {
        let tempdir = tempfile::Builder::new()
            .prefix("epazote-kill-")
            .tempdir_in(".")
            .expect("Failed to create temp dir");
        let marker = tempdir.path().join("marker.txt");

        let cmd = format!("sleep 2; echo alive > {}", marker.display());
        let result = execute_command(&cmd, Duration::from_millis(200)).await;
        assert!(result.is_err(), "command should time out");

        // Outlive the command's own sleep: if the child survived the timeout
        // it would create the marker here.
        tokio::time::sleep(Duration::from_secs(3)).await;

        assert!(
            !marker.exists(),
            "timed-out command must be killed, but it kept running"
        );
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

        let exit_code = execute_fallback_command(
            script_path.to_str().expect("Invalid path"),
            &context,
            Duration::from_secs(30),
        )
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

        let exit_code = execute_fallback_command(
            script_path.to_str().expect("Invalid path"),
            &context,
            Duration::from_secs(30),
        )
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
    async fn test_should_continue_fallback_stop_one() {
        let counters = Arc::new(Mutex::new(HashMap::new()));
        let action = config::Action {
            stop: Some(1),
            threshold: Some(3),
            ..Default::default()
        };

        // Below threshold
        assert!(!should_continue_fallback("test", &counters, &action).await);
        assert!(!should_continue_fallback("test", &counters, &action).await);

        // At threshold - should execute once
        assert!(should_continue_fallback("test", &counters, &action).await);

        // Should stop after first execution
        assert!(!should_continue_fallback("test", &counters, &action).await);

        let counters = counters.lock().await;
        let state = counters.get("test").expect("State not found");
        assert_eq!(state.consecutive_failures, 4);
        assert_eq!(state.fallback_executions, 1);
    }

    #[tokio::test]
    async fn test_should_continue_fallback_stop_zero() {
        let counters = Arc::new(Mutex::new(HashMap::new()));
        let action = config::Action {
            stop: Some(0),
            threshold: Some(2),
            ..Default::default()
        };

        // Below threshold
        assert!(!should_continue_fallback("test", &counters, &action).await);

        // At threshold but stop:0 means never execute
        assert!(!should_continue_fallback("test", &counters, &action).await);
        assert!(!should_continue_fallback("test", &counters, &action).await);

        let counters = counters.lock().await;
        let state = counters.get("test").expect("State not found");
        assert_eq!(state.consecutive_failures, 3);
        assert_eq!(state.fallback_executions, 0);
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
            stop: Some(1),
            ..Default::default()
        };

        assert!(!should_continue_fallback("test", &counters, &action).await);
        assert!(should_continue_fallback("test", &counters, &action).await);
        assert!(!should_continue_fallback("test", &counters, &action).await);

        reset_fallback_state("test", &counters).await;

        assert!(!should_continue_fallback("test", &counters, &action).await);
        assert!(should_continue_fallback("test", &counters, &action).await);

        let counters = counters.lock().await;
        let state = counters.get("test").expect("State not found");
        assert_eq!(state.consecutive_failures, 2);
        assert_eq!(state.fallback_executions, 1);
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

        let exit_code = execute_fallback_http(
            format!("{}/status/200", server.url()).as_str(),
            Duration::from_secs(30),
        )
        .await
        .expect("Failed to execute HTTP fallback");

        assert_eq!(exit_code, 200);

        // bad request
        let exit_code = execute_fallback_http(
            format!("{}/status/400", server.url()).as_str(),
            Duration::from_secs(30),
        )
        .await
        .expect("Failed to execute HTTP fallback");

        assert_eq!(exit_code, 501);
    }

    #[tokio::test]
    async fn test_execute_fallback_http_error() {
        let rs = execute_fallback_http("telnet://0", Duration::from_secs(30)).await;

        assert!(rs.is_err());
    }
}
