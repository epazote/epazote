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
use tracing::{debug, error, info, warn};

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
///
/// The result of the signal is checked rather than discarded. A refused kill
/// leaves the descendants running for as long as epazote does - the exact leak
/// the process group exists to prevent, repeating on every scan - and
/// swallowing it leaves nothing in the log to explain a host slowly filling
/// with orphaned restart scripts. How much a non-zero return says about
/// *which* members survived is platform-specific, so it is reported rather
/// than interpreted.
#[cfg(unix)]
fn kill_process_group(pid: u32) {
    let Ok(group) = i32::try_from(pid) else {
        return;
    };

    // `killpg(0, ..)` signals the *caller's* own group, which would take down
    // epazote itself. A real child pid is never 0, so refuse it outright.
    if group <= 0 {
        return;
    }

    // Safe: `group` is the child's own pid, and `process_group(0)` made it the
    // leader of a new group, so this can only signal that group.
    if unsafe { libc::killpg(group, libc::SIGKILL) } == 0 {
        return;
    }

    let error = std::io::Error::last_os_error();

    // The group already exited on its own, which is the outcome we wanted.
    if error.raw_os_error() == Some(libc::ESRCH) {
        return;
    }

    error!("Failed to kill process group {group}, it may have leaked: {error}");
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

/// Serializes fallback command execution across every service.
///
/// Each service is scanned in its own task with its own failure counter, so
/// when several fail on the same tick - a shared dependency going down, or a
/// burst of transient errors that trips multiple thresholds at once - their
/// `if_not.cmd` scripts would otherwise run concurrently. That interleaves
/// their output into a shared log file and, far worse, fires a burst of
/// service restarts at the same instant. Holding this lock for the duration of
/// each fallback command runs them one at a time, so a restart script acts on
/// a settled system and its log stays readable.
static FALLBACK_CMD_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// A fallback command that never ran because it never got the lock.
///
/// Carried as a typed error rather than a message the caller has to match on,
/// because the distinction decides whether the attempt counts against `stop`:
/// `stop` bounds how many times the fallback actions *execute*, and this one
/// did not.
#[derive(Debug)]
pub(crate) struct FallbackSkipped {
    timeout: Duration,
    cmd: String,
}

impl std::fmt::Display for FallbackSkipped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "fallback command skipped: waited the fallback 'timeout' of {:?} for another service's fallback command to finish: {}",
            self.timeout, self.cmd
        )
    }
}

impl std::error::Error for FallbackSkipped {}

/// Waits for `lock`, then runs `cmd` under it.
///
/// `timeout` is deliberately a per-phase budget, not a total: the wait for the
/// lock is bounded by it, and a command that does get the lock then gets the
/// whole of it to run in. A fallback can therefore occupy up to twice
/// `timeout` in the worst case.
///
/// The alternative - one deadline shared by both phases - was rejected: a
/// command that waited out most of the budget would start with a sliver of
/// time and be killed part-way through, which for a restart script means
/// stopping a service without starting it again. A recovery that begins must
/// be allowed to finish.
///
/// Bounding the wait at all matters because the caller is a service's scan
/// loop: an unbounded wait behind a queue of slow restarts would stop probing
/// that service entirely for as long as the queue takes. Giving up is safe
/// because the failure counter has already been incremented, so the next scan
/// retries; the attempt is handed back to `stop` by the caller, since a
/// command that never ran is not an execution.
///
/// Takes the lock as a parameter so this policy can be exercised against a
/// private mutex, instead of racing every other test on the process-wide one.
async fn run_command_under_lock(
    lock: &Mutex<()>,
    cmd: &str,
    context: &FallbackContext<'_>,
    timeout: Duration,
) -> Result<i32> {
    // The guard is a tokio mutex, so it is safe to hold across the await
    // below.
    let Ok(_guard) = time::timeout(timeout, lock.lock()).await else {
        return Err(FallbackSkipped {
            timeout,
            cmd: cmd.to_string(),
        }
        .into());
    };

    execute_shell_command(cmd, Some(context), timeout).await
}

/// Call the fallback command if the service is not reachable
pub(crate) async fn execute_fallback_command(
    cmd: &str,
    context: &FallbackContext<'_>,
    timeout: Duration,
) -> Result<i32> {
    // One fallback script at a time, process-wide, so simultaneous failures
    // cannot stampede restarts or interleave their log output.
    run_command_under_lock(&FALLBACK_CMD_LOCK, cmd, context, timeout).await
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

/// What a fallback attempt actually did.
///
/// `stop` bounds how many times the fallback actions *execute*, so the budget
/// has to be charged on what ran - never inferred from which error came back.
/// [`execute_fallbacks`] reports the command's error in preference to the HTTP
/// one, so a skipped command hides a perfectly successful alert behind
/// [`FallbackSkipped`]; reading the budget off that error would refund an
/// attempt that did alert, and `stop` would stop capping anything.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FallbackOutcome {
    /// The command got the lock and was run. False when no `cmd` is
    /// configured, and when one was skipped waiting for the lock. A command
    /// that ran and failed - or could not even be spawned - still counts: it
    /// had its turn, and refunding those would retry a broken command forever.
    pub(crate) command_ran: bool,

    /// A configured command never got the lock, so it was never run. This is
    /// the only way an action can be *prevented* from executing, and the only
    /// thing the `stop` budget has to compensate for.
    pub(crate) command_skipped: bool,

    /// The HTTP request was issued. False only when no `http` is configured:
    /// the alert takes no lock, so nothing can hold it back. An alert that was
    /// sent and answered with an error still counts as an execution.
    pub(crate) http_ran: bool,
}

impl FallbackOutcome {
    /// True when a configured action was prevented from running and nothing
    /// ran in its place, so the attempt executed nothing and must be handed
    /// back to the service's `stop` budget.
    ///
    /// An `if_not` block with no actions at all is deliberately *not* refunded:
    /// nothing was held back, so there is nothing to compensate for, and the
    /// budget goes on behaving exactly as it always has.
    pub(crate) const fn must_not_count_against_stop(self) -> bool {
        self.command_skipped && !self.command_ran && !self.http_ran
    }
}

/// Run the configured fallback actions for a failed service.
///
/// Both actions are optional and independent; whichever are present in the
/// `if_not` configuration run concurrently. Only the command takes
/// `FALLBACK_CMD_LOCK`, so a command queued behind another service's restart
/// cannot hold up the HTTP alert - which is often the only way an operator
/// learns anything happened at all.
///
/// Returns what actually ran alongside the error, because the two cannot be
/// derived from one another - see [`FallbackOutcome`].
pub(crate) async fn execute_fallbacks(
    action: &config::Action,
    context: &FallbackContext<'_>,
    service_name: &str,
) -> (FallbackOutcome, Result<()>) {
    // Recovery actions get their own budget, not the health-probe timeout.
    let timeout = action.fallback_timeout();

    let command = async {
        let Some(cmd) = action.cmd.as_ref() else {
            return (false, false, None);
        };

        match execute_fallback_command(cmd, context, timeout).await {
            Ok(exit_code) => {
                info!("Executed fallback command for {service_name} with exit code {exit_code}");
                (true, false, None)
            }
            Err(error) => {
                // A command that never got the lock never ran; every other
                // failure happened while it was running, or trying to.
                let skipped = error.downcast_ref::<FallbackSkipped>().is_some();
                warn!("Fallback command for {service_name} failed: {error}");
                (!skipped, skipped, Some(error))
            }
        }
    };

    let http = async {
        let Some(url) = action.http.as_ref() else {
            return (false, None);
        };

        match execute_fallback_http(url, timeout).await {
            Ok(status) => {
                info!(
                    "Executed fallback HTTP request for {service_name} with status code {status}"
                );
                (true, None)
            }
            Err(error) => {
                warn!("Fallback HTTP request for {service_name} failed: {error}");
                (true, Some(error))
            }
        }
    };

    // The actions are independent: a failing command must not skip a
    // configured HTTP alert, so both always run to completion and the
    // command's error is the one reported when both fail.
    let ((command_ran, command_skipped, command_error), (http_ran, http_error)) =
        tokio::join!(command, http);

    let outcome = FallbackOutcome {
        command_ran,
        command_skipped,
        http_ran,
    };

    (outcome, command_error.or(http_error).map_or(Ok(()), Err))
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

/// Gives back the `stop` attempt taken by a fallback attempt in which nothing
/// ran.
///
/// `should_continue_fallback` counts the attempt before the fallback runs, so a
/// command skipped while waiting for the global lock has already spent one
/// even though it executed nothing - and `stop` bounds *executions*. That
/// matters because lock contention only happens during a burst of simultaneous
/// failures, which is exactly what the lock exists for: without this, a service
/// could exhaust its whole `stop` budget on attempts that never restarted
/// anything, and then be skipped for the rest of the outage while still down,
/// never having been restarted once.
///
/// The caller decides when this applies - see
/// [`execute_fallbacks_tracking_stop`], which withholds the refund when an
/// alert was configured and therefore did run.
///
/// Only the `stop` budget is restored. The failed check itself was real, so
/// `consecutive_failures` keeps counting and `threshold` still holds.
async fn restore_fallback_execution<S: BuildHasher>(
    service_name: &str,
    counters: &Arc<Mutex<HashMap<String, FallbackState, S>>>,
) {
    let mut counters = counters.lock().await;
    if let Some(state) = counters.get_mut(service_name) {
        state.fallback_executions = state.fallback_executions.saturating_sub(1);
    }
}

/// Runs the fallback actions and keeps the `stop` budget honest.
///
/// Wraps [`execute_fallbacks`] so that an attempt in which *nothing ran* does
/// not consume one of the service's `stop` attempts. `stop` bounds how many
/// times the fallback actions execute, and a command skipped for the global
/// lock never executed.
///
/// The decision is made from the [`FallbackOutcome`] the actions report, not
/// from the error that came back. Those are not interchangeable: when a
/// command is skipped while an `if_not.http` alert is configured, the alert is
/// still sent - it takes no lock - but the command's [`FallbackSkipped`] is
/// the error that wins. Refunding on that error would hand the attempt back
/// even though the service did alert, and with the budget restored on every
/// scan `stop: N` would cap nothing for as long as the lock stayed contended -
/// which is exactly the burst of simultaneous failures it exists to keep
/// quiet.
///
/// Refunding matters in the opposite case for the same reason: contention only
/// happens during such a burst, so charging skips would let a service burn its
/// whole budget on attempts that restarted nothing and then be abandoned while
/// still down, having never been restarted once.
///
/// `consecutive_failures` is never refunded: the check really did fail, and it
/// still counts toward `threshold`.
///
/// The error is returned either way: the scan failed, and the caller reports
/// it.
pub(crate) async fn execute_fallbacks_tracking_stop<S: BuildHasher>(
    action: &config::Action,
    context: &FallbackContext<'_>,
    service_name: &str,
    counters: &Arc<Mutex<HashMap<String, FallbackState, S>>>,
) -> Result<()> {
    let (outcome, result) = execute_fallbacks(action, context, service_name).await;

    if outcome.must_not_count_against_stop() {
        warn!(
            "Service '{service_name}' fallback command never ran and no alert went out, so the attempt does not count against 'stop'"
        );

        restore_fallback_execution(service_name, counters).await;
    }

    result
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
        //
        // `&&`, not `;`: `killpg` signals group members one at a time, so the
        // subshell can be woken by its `sleep` dying a moment before its own
        // SIGKILL arrives. With `;` it would run the `echo` in that window and
        // report a leak that never happened - the marker has to mean the sleep
        // *completed*.
        let cmd = format!(
            "( sleep 3 && echo alive > {} ) & sleep 30",
            marker.display()
        );

        let result = execute_command(&cmd, Duration::from_millis(200)).await;
        assert!(result.is_err(), "command should time out");

        tokio::time::sleep(Duration::from_secs(5)).await;

        assert!(
            !marker.exists(),
            "descendants of a timed-out command must be killed, not just the shell"
        );
    }

    /// A timed-out command must take every descendant with it, not just the
    /// shell it spawned: the whole group has to be gone once the kill returns.
    #[tokio::test]
    #[cfg(unix)]
    async fn test_kill_process_group_reports_the_group_is_gone() {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("( sleep 30 ) & sleep 30")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);

        let mut child = command.spawn().expect("failed to spawn test group");
        let pid = child.id().expect("child should still be running");
        let group = i32::try_from(pid).expect("pid should fit in i32");

        // Let the shell fork its background subshell before signalling.
        tokio::time::sleep(Duration::from_millis(200)).await;

        kill_process_group(pid);

        // Reap the leader so only genuinely leaked members could answer below.
        let _ = child.wait().await;

        // Signal 0 performs the permission/existence check without sending
        // anything, so a surviving member of the group still reports success.
        // Killed members linger as zombies until they are reparented and
        // reaped, which is asynchronous, so poll instead of sampling once.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut alive = true;

        while std::time::Instant::now() < deadline {
            alive = unsafe { libc::killpg(group, 0) } == 0;

            if !alive {
                break;
            }

            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        assert!(
            !alive,
            "a timed-out command must take its whole process group with it"
        );
    }

    /// `killpg(0, ..)` targets the caller's own process group, which would take
    /// epazote down with the command it was trying to kill.
    #[tokio::test]
    #[cfg(unix)]
    async fn test_kill_process_group_never_signals_its_own_group() {
        // Reaching the `killpg` call with 0 would SIGKILL this test binary, so
        // simply returning from this test proves the guard held.
        kill_process_group(0);
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

        let (_, result) = execute_fallbacks(&action, &context, "independent").await;

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

        // `&&`, not `;`: the kill is not atomic across the process group, so
        // the shell can be woken by its `sleep` dying a moment before its own
        // SIGKILL lands. `;` would then run the `echo` and report a survivor
        // that was in fact killed. `&&` still catches a real survivor, whose
        // `sleep` exits 0.
        let cmd = format!("sleep 2 && echo alive > {}", marker.display());
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
    async fn test_fallback_commands_do_not_overlap() {
        // Two services failing on the same tick would otherwise run their
        // restart scripts concurrently. The global fallback lock must serialize
        // them so a burst of failures cannot stampede restarts or interleave
        // their log output.
        let tempdir = tempfile::Builder::new()
            .prefix("epazote-overlap-dir-")
            .tempdir_in(".")
            .expect("Failed to create temp dir");
        let log_path = tempdir.path().join("order.log");
        let log = log_path.to_str().expect("Invalid path");

        // Each invocation marks its start, holds for a beat, then marks its
        // end. If they overlap, the two "start" lines land back to back.
        let cmd = format!("printf 'start\\n' >> {log}; sleep 0.3; printf 'end\\n' >> {log}");

        let context = FallbackContext {
            service_name: "svc",
            service_type: FallbackServiceType::Command,
            expected_status: Some(0),
            actual_status: Some(1),
            error: "command_failed",
            failure_count: 1,
            threshold: 1,
            url: None,
            test: None,
        };

        let first = execute_fallback_command(&cmd, &context, Duration::from_secs(30));
        let second = execute_fallback_command(&cmd, &context, Duration::from_secs(30));
        let (first, second) = tokio::join!(first, second);
        first.expect("first fallback command failed");
        second.expect("second fallback command failed");

        let output = fs::read_to_string(&log_path).expect("Failed to read order log");
        assert_eq!(
            output.lines().collect::<Vec<_>>(),
            vec!["start", "end", "start", "end"],
            "fallback commands overlapped: {output:?}"
        );
    }

    /// Regression: fallback commands are serialized process-wide, so one can
    /// sit queued behind another service's restart for as long as that
    /// restart's timeout (5 minutes by default). The HTTP alert must not
    /// inherit that wait - it is often the only way an operator learns
    /// anything happened - so it runs concurrently with the command.
    #[tokio::test]
    async fn test_fallback_http_alert_is_not_delayed_by_a_queued_command() {
        let mut server = Server::new_async().await;
        let mock = server
            .mock("GET", "/alert")
            .with_status(200)
            .create_async()
            .await;

        // Stand in for another service's fallback command by holding the
        // global lock here. Taking it directly - rather than spawning a
        // blocker and polling try_lock - is what makes this deterministic:
        // polling only proves *someone* holds the lock, which any concurrently
        // running test could satisfy. The mutex is fair, so the command below
        // queues behind this guard and nothing else can overtake it.
        let guard = FALLBACK_CMD_LOCK.lock().await;

        let action = config::Action {
            cmd: Some("exit 0".to_string()),
            http: Some(format!("{}/alert", server.url())),
            stop: None,
            threshold: Some(1),
            // Short, so the queued command gives up and this test finishes
            // while the guard above is still held.
            timeout: Some(Duration::from_millis(500)),
        };

        let context = FallbackContext {
            service_name: "alerting",
            service_type: FallbackServiceType::Command,
            expected_status: Some(0),
            actual_status: Some(1),
            error: "command_failed",
            failure_count: 1,
            threshold: 1,
            url: None,
            test: None,
        };

        let fallbacks = execute_fallbacks(&action, &context, "alerting");
        let alert_fired = async {
            time::sleep(Duration::from_millis(250)).await;
            mock.matched_async().await
        };

        // Drive both: the fallbacks future is still blocked on the lock while
        // the alert check runs.
        let (_, alert_fired) = tokio::join!(fallbacks, alert_fired);

        assert!(
            alert_fired,
            "the HTTP alert must fire while the fallback command is still queued behind the lock"
        );

        drop(guard);
    }

    /// Regression: waiting for the global fallback lock is bounded. The caller
    /// is a service's scan loop, so an unbounded wait behind a queue of slow
    /// restarts would stop probing that service entirely; giving up lets the
    /// next scan retry instead.
    #[tokio::test]
    async fn test_fallback_command_gives_up_when_the_lock_is_held_too_long() {
        let tempdir = tempfile::Builder::new()
            .prefix("epazote-lock-wait-dir-")
            .tempdir_in(".")
            .expect("Failed to create temp dir");
        let marker = tempdir.path().join("ran");

        let context = FallbackContext {
            service_name: "queued",
            service_type: FallbackServiceType::Command,
            expected_status: Some(0),
            actual_status: Some(1),
            error: "command_failed",
            failure_count: 1,
            threshold: 1,
            url: None,
            test: None,
        };

        // Hold the lock here for the whole test, standing in for another
        // service's fallback that outlasts the waiter's patience. Taking the
        // guard directly keeps this deterministic - polling try_lock would
        // only prove *some* concurrently running test held the lock.
        let guard = FALLBACK_CMD_LOCK.lock().await;

        let cmd = format!("touch {}", marker.display());
        let started = std::time::Instant::now();
        let error = execute_fallback_command(&cmd, &context, Duration::from_millis(200))
            .await
            .expect_err("a fallback that never got the lock must report an error");

        assert!(
            error.to_string().contains("fallback command skipped"),
            "the error must say the command was skipped, got: {error}"
        );
        assert!(
            started.elapsed() < Duration::from_millis(800),
            "the wait must be bounded by the fallback timeout, took {:?}",
            started.elapsed()
        );
        assert!(
            !marker.exists(),
            "a skipped fallback command must not have run"
        );

        drop(guard);
    }

    /// The fallback `timeout` is per phase, not a shared deadline: a command
    /// that spent most of its patience waiting for the lock must still get the
    /// full budget to run in. Under one shared deadline a queued restart would
    /// start with a sliver of time and be killed part-way through - stopping a
    /// service without starting it again.
    ///
    /// Runs against a private mutex: the policy is the same one
    /// `execute_fallback_command` applies, but a test that consumed most of
    /// its budget waiting on the process-wide lock would be at the mercy of
    /// every other test queued on it.
    #[tokio::test]
    async fn test_a_queued_fallback_command_still_gets_its_full_budget() {
        let context = failing_context();
        let lock = Arc::new(Mutex::new(()));

        // Three constraints have to hold at once, and the queue wait can only
        // be as slack as the command is long: the wait must not exhaust the
        // budget (queue < budget), the command must fit inside a fresh budget
        // with room for the shell spawn (command < budget), and the two
        // together must exceed the budget or a shared deadline would pass this
        // test too (queue + command > budget). A shared CI runner spawning
        // processes under load makes tight margins here flaky rather than
        // meaningful, so every margin is a generous fraction of a second.
        let budget = Duration::from_millis(2000);
        let queued_for = Duration::from_millis(1200);

        let guard = Arc::clone(&lock).lock_owned().await;
        tokio::spawn(async move {
            time::sleep(queued_for).await;
            drop(guard);
        });

        let started = std::time::Instant::now();
        let exit_code = run_command_under_lock(&lock, "sleep 1", &context, budget)
            .await
            .expect("a command that waited for the lock must still get its full timeout");

        assert_eq!(exit_code, 0);

        // Guards the third constraint. Without it, shortening either sleep
        // could leave the whole run inside a single budget, and the test would
        // keep passing while no longer telling the two policies apart.
        assert!(
            started.elapsed() > budget,
            "the run must outlast one budget, or it cannot tell a per-phase \
             timeout from a shared deadline; took {:?}",
            started.elapsed()
        );
    }

    /// A closed port, for fallback HTTP requests that must fail.
    fn closed_port_url() -> String {
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("Failed to bind test listener");
        let addr = listener.local_addr().expect("Failed to get local addr");
        drop(listener);

        format!("http://{addr}/alert")
    }

    fn failing_context() -> FallbackContext<'static> {
        FallbackContext {
            service_name: "both-fail",
            service_type: FallbackServiceType::Command,
            expected_status: Some(0),
            actual_status: Some(1),
            error: "command_failed",
            failure_count: 1,
            threshold: 1,
            url: None,
            test: None,
        }
    }

    /// The two fallback actions run concurrently, so their errors no longer
    /// arrive in a fixed order. The command's error must still be the one
    /// reported when both fail: it is the recovery that did not happen, while
    /// the HTTP failure only means the notification did not land.
    #[tokio::test]
    async fn test_execute_fallbacks_reports_the_command_error_when_both_fail() {
        let action = config::Action {
            // Fails immediately rather than by timing out: `timeout` also
            // bounds the wait for the global lock, so a test that failed by
            // timeout would report "skipped" whenever another test happened to
            // hold the lock. A generous budget plus an instant failure keeps
            // this test about error precedence and nothing else.
            cmd: Some("kill -9 $$".to_string()),
            http: Some(closed_port_url()),
            stop: None,
            threshold: Some(1),
            timeout: Some(Duration::from_secs(30)),
        };

        let error = execute_fallbacks(&action, &failing_context(), "both-fail")
            .await
            .1
            .expect_err("both actions failed, so the fallback must report an error");

        assert!(
            error.to_string().contains("terminated by signal"),
            "the command error must win over the HTTP one, got: {error}"
        );
    }

    /// With the command healthy, the HTTP alert's failure is the only thing
    /// left to report - it must not be swallowed by the successful command.
    #[tokio::test]
    async fn test_execute_fallbacks_reports_the_http_error_when_the_command_succeeds() {
        let action = config::Action {
            cmd: Some("exit 0".to_string()),
            http: Some(closed_port_url()),
            stop: None,
            threshold: Some(1),
            // Generous, so the command is never skipped for the lock; the
            // closed port refuses the connection immediately either way.
            timeout: Some(Duration::from_secs(30)),
        };

        let error = execute_fallbacks(&action, &failing_context(), "http-fails")
            .await
            .1
            .expect_err("a failing HTTP alert must be reported");

        assert!(
            !error.to_string().contains("fallback command"),
            "the reported error must be the HTTP one, got: {error}"
        );
    }

    /// The two fixes together: a command can be skipped because it never got
    /// the global lock, and that must not take the alert down with it. This is
    /// exactly the moment an operator most needs to hear about the failure -
    /// the system is busy enough that recovery is being dropped.
    #[tokio::test]
    async fn test_fallback_http_alert_fires_when_the_command_is_skipped_for_the_lock() {
        let mut server = Server::new_async().await;
        let mock = server
            .mock("GET", "/alert")
            .with_status(200)
            .create_async()
            .await;

        let context = failing_context();

        // Hold the lock for the whole test so the command below can never get
        // it. Taking the guard directly keeps this deterministic - polling
        // try_lock would only prove *some* concurrently running test held it.
        let guard = FALLBACK_CMD_LOCK.lock().await;

        let action = config::Action {
            cmd: Some("exit 0".to_string()),
            http: Some(format!("{}/alert", server.url())),
            stop: None,
            threshold: Some(1),
            timeout: Some(Duration::from_millis(200)),
        };

        let error = execute_fallbacks(&action, &context, "skipped")
            .await
            .1
            .expect_err("a skipped command must still be reported");

        assert!(
            error.to_string().contains("fallback command skipped"),
            "the skipped command must be the reported error, got: {error}"
        );
        mock.assert_async().await;

        drop(guard);
    }

    /// `stop` accounting across every combination of the two `if_not` actions,
    /// as a table over the state space that decides it: which actions actually
    /// ran.
    ///
    /// The budget counts executions, so the only question is whether anything
    /// ran at all. Reading that off the returned error instead silently gets
    /// the mixed row wrong: `execute_fallbacks` reports the command's error in
    /// preference to the HTTP one, so a command skipped for the lock masks an
    /// alert that did fire, and the attempt would be refunded even though the
    /// service alerted - uncapping alerts for exactly as long as the
    /// contention lasted.
    #[tokio::test]
    async fn test_stop_accounting_over_every_fallback_combination() {
        let cases = [
            StopCase {
                service: "skipped-without-alert",
                with_cmd: true,
                alert: Alert::None,
                contended: true,
                expected_executions: 0,
                why: "nothing ran: the command never got the lock and no alert was configured, so the attempt must be handed back",
            },
            StopCase {
                service: "skipped-with-alert",
                with_cmd: true,
                alert: Alert::Reachable,
                contended: true,
                expected_executions: 1,
                why: "the alert takes no lock and was sent, so the attempt is spent even though the command was skipped",
            },
            StopCase {
                service: "skipped-with-undeliverable-alert",
                with_cmd: true,
                alert: Alert::Undeliverable,
                contended: true,
                expected_executions: 1,
                why: "the alert was issued and merely failed - nothing held it back - so it is an execution and spends the attempt, exactly like a command that runs and fails",
            },
            StopCase {
                service: "ran-without-alert",
                with_cmd: true,
                alert: Alert::None,
                contended: false,
                expected_executions: 1,
                why: "a command that ran is an execution and spends its attempt",
            },
            StopCase {
                service: "ran-with-alert",
                with_cmd: true,
                alert: Alert::Reachable,
                contended: false,
                expected_executions: 1,
                why: "both actions ran, so the attempt is spent",
            },
            StopCase {
                service: "alert-only",
                with_cmd: false,
                alert: Alert::Reachable,
                contended: false,
                expected_executions: 1,
                why: "an alert with no command to wait for always executes",
            },
            StopCase {
                service: "alert-only-undeliverable",
                with_cmd: false,
                alert: Alert::Undeliverable,
                contended: false,
                expected_executions: 1,
                why: "an alert that failed to arrive still ran, so it spends the attempt just as a delivered one does",
            },
            StopCase {
                service: "no-actions",
                with_cmd: false,
                alert: Alert::None,
                contended: false,
                expected_executions: 1,
                why: "an 'if_not' with no actions has nothing to hand back: nothing was held up, so the budget behaves as it always has",
            },
        ];

        for case in cases {
            assert_stop_accounting(&case).await;
        }
    }

    /// One row of [`test_stop_accounting_over_every_fallback_combination`].
    struct StopCase {
        service: &'static str,
        with_cmd: bool,
        alert: Alert,
        /// Hold the global lock so a configured command never gets a turn.
        contended: bool,
        expected_executions: usize,
        why: &'static str,
    }

    /// What a row's `if_not.http` points at.
    ///
    /// Delivery is a separate axis from configuration because only one thing
    /// can *prevent* an action from executing - the global command lock - and
    /// an alert never takes it. An alert that was issued and failed still ran,
    /// so it still spends the attempt; modelling it as a third state keeps
    /// "no alert configured" and "alert that did not arrive" from being
    /// confused for each other.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Alert {
        /// No `http` configured.
        None,
        /// A mock that answers, so the request is issued and delivered.
        Reachable,
        /// A closed port: the request is issued but never delivered.
        Undeliverable,
    }

    async fn assert_stop_accounting(case: &StopCase) {
        let mut server = Server::new_async().await;
        let mock = server
            .mock("GET", "/alert")
            .with_status(200)
            .expect(usize::from(case.alert == Alert::Reachable))
            .create_async()
            .await;

        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let action = config::Action {
            cmd: case.with_cmd.then(|| "exit 0".to_string()),
            http: match case.alert {
                Alert::None => None,
                Alert::Reachable => Some(format!("{}/alert", server.url())),
                Alert::Undeliverable => Some(closed_port_url()),
            },
            // A single attempt, so spending it wrongly is unrecoverable.
            stop: Some(1),
            threshold: Some(1),
            timeout: Some(if case.contended {
                // Short, so the queued command gives up while the guard is
                // held.
                Duration::from_millis(200)
            } else {
                // Generous, so an uncontended command can only fail by running
                // - never by losing a race for the lock with another test.
                Duration::from_secs(30)
            }),
        };

        // Stand in for another service's fallback still running.
        let guard = if case.contended {
            Some(FALLBACK_CMD_LOCK.lock().await)
        } else {
            None
        };

        assert!(
            should_continue_fallback(case.service, &counters, &action).await,
            "[{}] the first failed check must be allowed to run its fallback",
            case.service
        );

        let result =
            execute_fallbacks_tracking_stop(&action, &failing_context(), case.service, &counters)
                .await;

        if case.contended {
            let error = result.expect_err(case.why);

            assert!(
                error.downcast_ref::<FallbackSkipped>().is_some(),
                "[{}] the skip must be typed so it can be told apart from a command that ran and failed, got: {error}",
                case.service
            );
        } else if case.alert == Alert::Undeliverable {
            assert!(
                result.is_err(),
                "[{}] an alert that never arrived must still be reported",
                case.service
            );
        } else {
            assert!(
                result.is_ok(),
                "[{}] every configured action succeeded, so the fallback must report success",
                case.service
            );
        }

        drop(guard);
        mock.assert_async().await;

        let state = get_fallback_state(case.service, &counters)
            .await
            .expect("the service must have fallback state");

        assert_eq!(
            state.fallback_executions, case.expected_executions,
            "[{}] {}",
            case.service, case.why
        );

        assert_eq!(
            state.consecutive_failures, 1,
            "[{}] the failed check itself was real and must still count toward 'threshold'",
            case.service
        );

        // With `stop: 1`, a refunded attempt is the difference between the
        // service still getting its one real restart and being abandoned while
        // down, having never been restarted once.
        assert_eq!(
            should_continue_fallback(case.service, &counters, &action).await,
            case.expected_executions == 0,
            "[{}] the next failed check must only run a fallback while the budget is unspent",
            case.service
        );
    }

    /// Guards against over-correcting the refund: a fallback command that
    /// actually ran *is* an execution and must still spend its `stop` attempt,
    /// even when it fails outright. Only a command that never ran gets its
    /// attempt back.
    #[tokio::test]
    async fn test_a_fallback_command_that_ran_and_failed_still_consumes_its_stop_attempt() {
        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let action = config::Action {
            // Fails by dying on a signal, so the command genuinely ran.
            cmd: Some("kill -9 $$".to_string()),
            http: None,
            stop: Some(1),
            threshold: Some(1),
            // Generous, so this can only fail by running - never by being
            // skipped for the lock.
            timeout: Some(Duration::from_secs(30)),
        };

        assert!(should_continue_fallback("ran", &counters, &action).await);

        let error = execute_fallbacks_tracking_stop(&action, &failing_context(), "ran", &counters)
            .await
            .expect_err("a command killed by a signal must be reported as a failure");

        assert!(
            error.downcast_ref::<FallbackSkipped>().is_none(),
            "a command that ran must not be reported as skipped, got: {error}"
        );

        let state = get_fallback_state("ran", &counters)
            .await
            .expect("the service must have fallback state");

        assert_eq!(
            state.fallback_executions, 1,
            "a command that ran and failed must still spend its 'stop' attempt"
        );
        assert!(
            !should_continue_fallback("ran", &counters, &action).await,
            "with 'stop: 1' that single attempt is now spent"
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
