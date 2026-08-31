pub mod client;
pub mod metrics;
pub mod request;
pub mod run;
pub mod ssl;

use crate::cli::actions::client::APP_USER_AGENT;
use crate::cli::actions::metrics::ServiceMetrics;
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
    pub stop_exhaustion_reported: bool,
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

/// Kills a command's process group when its future is cancelled.
///
/// This guard must be declared after the `Child` it protects. Locals drop in
/// reverse order, so the guard signals the group while its leader still exists;
/// letting `Child` reap the leader first could allow its process-group ID to be
/// reused before `killpg` runs.
struct ProcessGroupGuard {
    #[cfg(unix)]
    pid: Option<u32>,
}

impl ProcessGroupGuard {
    const fn new(pid: Option<u32>) -> Self {
        Self {
            #[cfg(unix)]
            pid,
        }
    }

    fn kill(&self) {
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            kill_process_group(pid);
        }
    }

    fn kill_and_disarm(&mut self) {
        self.kill();
        self.disarm();
    }

    fn disarm(&mut self) {
        #[cfg(unix)]
        {
            self.pid = None;
        }
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        self.kill();
    }
}

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
    let timeout_setting = if context.is_some() {
        "'if_not.timeout'"
    } else {
        "the service 'timeout'"
    };

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
    // Keep this after `child`: the drop order is a safety invariant documented
    // on `ProcessGroupGuard`.
    let mut process_group = ProcessGroupGuard::new(child_pid);
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
        process_group.kill_and_disarm();

        return Err(anyhow!(
            "command exceeded {timeout_setting} of {timeout:?}: {cmd}"
        ));
    };

    let (status, stderr) = output?;
    process_group.disarm();

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

/// Serializes fallback command execution within a declared group.
///
/// Each service is scanned in its own task with its own failure counter, so
/// when several fail on the same tick - a shared dependency going down, or a
/// burst of transient errors that trips multiple thresholds at once - their
/// `if_not.cmd` scripts run concurrently. For services that share something -
/// one restart script writing to one log file, or one host that cannot take
/// several heavy restarts at once - that interleaves their output and fires a
/// burst of restarts at the same instant.
///
/// 4.1.0 solved this with a single process-wide lock, which also serialized
/// services that share nothing: a slow restart starved every other failing
/// service behind it, up to being skipped once the wait exceeded its
/// `if_not.timeout`. The hazard is *shared* resources, so the lock is scoped to
/// the same thing - services that declare the same `if_not.group` run one at a
/// time, and everything else runs immediately.
///
/// Entries are never removed. The configuration is fixed for the lifetime of
/// the process, so a group's lock must outlive every scan that uses it, and the
/// map is bounded by the number of distinct groups declared.
#[derive(Debug, Default)]
struct FallbackGroupLocks {
    groups: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl FallbackGroupLocks {
    /// The lock for `group`, creating it on first use.
    ///
    /// The registry guard is held only for the lookup and never across the
    /// command itself, so a running fallback cannot stop another group from
    /// even finding its lock. A tokio mutex rather than a `std` one: it has no
    /// poisoning, so there is no `Result` to unwrap under the crate's
    /// `unwrap_used`/`expect_used` lints.
    async fn lock_for(&self, group: &str) -> Arc<Mutex<()>> {
        let mut groups = self.groups.lock().await;

        Arc::clone(groups.entry(group.to_string()).or_default())
    }
}

static FALLBACK_GROUP_LOCKS: LazyLock<FallbackGroupLocks> =
    LazyLock::new(FallbackGroupLocks::default);

/// The address of a shared endpoint that accepts and immediately drops every
/// connection, so a request to it always fails.
///
/// One listener for the whole test binary, rather than one per call. Nothing is
/// ever served, so every caller can share it whatever scheme or path it asks
/// for - which keeps this to a single accept loop instead of a detached thread
/// per test that could never be stopped.
///
/// Preferred over binding a port and releasing it: that leaves a window in
/// which another process can claim the port between setup and the request, and
/// the test then fails against whatever answered.
#[cfg(test)]
#[allow(clippy::expect_used)]
static RESET_ENDPOINT_ADDR: LazyLock<String> = LazyLock::new(|| {
    let listener =
        std::net::TcpListener::bind("127.0.0.1:0").expect("Failed to bind the reset endpoint");
    let addr = listener
        .local_addr()
        .expect("Failed to read the reset endpoint address")
        .to_string();

    std::thread::spawn(move || {
        while let Ok((stream, _)) = listener.accept() {
            drop(stream);
        }
    });

    addr
});

/// A URL on the shared reset endpoint - see [`RESET_ENDPOINT_ADDR`].
#[cfg(test)]
pub(crate) fn resetting_endpoint_url(scheme: &str, path: &str) -> String {
    format!("{scheme}://{}{path}", *RESET_ENDPOINT_ADDR)
}

/// Holds `group`'s lock, standing in for another member of that group whose
/// fallback command is still running.
///
/// Taking the guard directly - rather than spawning a blocker and polling
/// `try_lock` - is what makes the contention tests deterministic: polling would
/// only prove *someone* holds the lock. Each test passes a group name of its
/// own, so what it observes is its own contention and not a race with whatever
/// else the suite happens to be running, which is a hazard the single
/// process-wide lock used to force on every one of them.
///
/// Lives here rather than in this module's `tests` so that the scan-loop tests
/// in `run.rs` contend against the same registry the production path uses,
/// instead of reaching into it a second way that could drift from this one.
#[cfg(test)]
pub(crate) async fn hold_group_lock(group: &str) -> tokio::sync::OwnedMutexGuard<()> {
    FALLBACK_GROUP_LOCKS
        .lock_for(group)
        .await
        .lock_owned()
        .await
}

/// A fallback command that never ran because it never got its group's lock.
///
/// Carried as a typed error rather than a message the caller has to match on,
/// because the distinction decides whether the attempt counts against `stop`:
/// `stop` bounds how many times the fallback actions *execute*, and this one
/// did not.
///
/// Only a grouped command can reach this state. An ungrouped one takes no lock,
/// so it always runs.
#[derive(Debug)]
pub(crate) struct FallbackSkipped {
    timeout: Duration,
    cmd: String,
    group: String,
}

impl std::fmt::Display for FallbackSkipped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "fallback command skipped: waited 'if_not.timeout' of {:?} for another fallback command in group '{}' to finish: {}",
            self.timeout, self.group, self.cmd
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
/// private mutex, instead of depending on which group a caller happens to use.
async fn run_command_under_lock(
    lock: &Mutex<()>,
    cmd: &str,
    context: &FallbackContext<'_>,
    timeout: Duration,
    group: &str,
) -> Result<i32> {
    // The guard is a tokio mutex, so it is safe to hold across the await
    // below.
    let Ok(_guard) = time::timeout(timeout, lock.lock()).await else {
        return Err(FallbackSkipped {
            timeout,
            cmd: cmd.to_string(),
            group: group.to_string(),
        }
        .into());
    };

    execute_shell_command(cmd, Some(context), timeout).await
}

/// Call the fallback command if the service is not reachable
///
/// `group` decides whether this command queues at all. A declared group runs
/// its members one at a time, so a shared restart script cannot interleave its
/// output or stampede a shared host. No group means no lock: the command starts
/// as soon as the check fails, which is what keeps an unrelated service's slow
/// restart irrelevant to this one.
pub(crate) async fn execute_fallback_command(
    cmd: &str,
    context: &FallbackContext<'_>,
    timeout: Duration,
    group: Option<&str>,
) -> Result<i32> {
    let Some(group) = group else {
        return execute_shell_command(cmd, Some(context), timeout).await;
    };

    let lock = FALLBACK_GROUP_LOCKS.lock_for(group).await;

    run_command_under_lock(&lock, cmd, context, timeout, group).await
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
    /// What became of the configured command.
    pub(crate) command: CommandOutcome,

    /// The HTTP request was issued. False only when no `http` is configured:
    /// the alert never queues, so nothing can hold it back. An alert that was
    /// sent and answered with an error still counts as an execution.
    pub(crate) http_ran: bool,

    /// At least one action that ran reported failure: a command that exited
    /// non-zero or could not be spawned, or an alert answered with a non-2xx
    /// status.
    ///
    /// Deliberately separate from the returned error and from `stop`
    /// accounting. A command that runs and fails has still had its turn, so it
    /// spends its attempt; what changes is only how the attempt is reported.
    /// A skipped command never ran, so it is not a failure - it is counted
    /// under its own outcome.
    pub(crate) action_failed: bool,
}

/// What happened to the `if_not.cmd` of a single fallback attempt.
///
/// The three states are mutually exclusive, which is why this is an enum
/// rather than a pair of flags: a command cannot both have run and have been
/// skipped, and `stop` accounting depends on telling those two apart.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommandOutcome {
    /// No `cmd` is configured, so there was never anything to run.
    #[default]
    NotConfigured,

    /// The command was run. A command that ran and failed - or could not even
    /// be spawned - still counts: it had its turn, and refunding those would
    /// retry a broken command forever.
    Ran,

    /// A configured command never got its group's lock, so it was never run.
    /// This is the only way an action can be *prevented* from executing, and
    /// the only thing the `stop` budget has to compensate for.
    ///
    /// Only reachable for a command that declares an `if_not.group`. An
    /// ungrouped command takes no lock, so it always runs and always spends
    /// its attempt.
    Skipped,
}

impl FallbackOutcome {
    /// The `outcome` label this attempt is counted under.
    ///
    /// Labels follow the most severe thing that happened: an action that
    /// failed outranks a command that was held back, and a held-back command
    /// outranks actions that did succeed. This is deliberately independent of
    /// [`Self::must_not_count_against_stop`]: the label describes the attempt,
    /// while the refund depends on whether anything executed.
    pub(crate) const fn metric_label(self) -> &'static str {
        if self.action_failed {
            metrics::FALLBACK_FAILURE
        } else if matches!(self.command, CommandOutcome::Skipped) {
            metrics::FALLBACK_SKIPPED
        } else {
            metrics::FALLBACK_SUCCESS
        }
    }

    /// True when a configured action was prevented from running and nothing
    /// ran in its place, so the attempt executed nothing and must be handed
    /// back to the service's `stop` budget.
    ///
    /// Actionless `if_not` blocks are rejected during configuration validation,
    /// so every fallback reaching this point has something that can run.
    pub(crate) const fn must_not_count_against_stop(self) -> bool {
        matches!(self.command, CommandOutcome::Skipped) && !self.http_ran
    }
}

/// Run the configured fallback actions for a failed service.
///
/// Both actions are optional and independent; whichever are present in the
/// `if_not` configuration run concurrently. Only the command can queue, and
/// only against its own `if_not.group`, so a command waiting its turn cannot
/// hold up the HTTP alert - which is often the only way an operator learns
/// anything happened at all.
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
            return (CommandOutcome::NotConfigured, false, None);
        };

        match execute_fallback_command(cmd, context, timeout, action.group_name()).await {
            Ok(exit_code) => {
                // A recovery command reports whether it repaired the service
                // the only way it can: its exit status. Treating every exit as
                // a success published `outcome="success"` for a restart that
                // returned non-zero, so a service could be recorded as
                // repeatedly recovered while it was down and spending its
                // `stop` budget on a command that failed every time.
                if exit_code == 0 {
                    info!("Executed fallback command for {service_name} with exit code 0");
                    (CommandOutcome::Ran, false, None)
                } else {
                    // Error level, not warn: packaged installs default to
                    // ERROR, and a restart script that keeps failing is the
                    // service not being repaired. Under `warn!` this was
                    // invisible - a service could be down, its recovery
                    // failing every time, and the log stayed empty.
                    error!(
                        "Fallback command for {service_name} ran but exited with code {exit_code}"
                    );
                    (CommandOutcome::Ran, true, None)
                }
            }
            Err(error) => {
                // A command that never got its group's lock never ran; every
                // other failure happened while it was running, or trying to.
                let skipped = error.downcast_ref::<FallbackSkipped>().is_some();
                warn!("Fallback command for {service_name} failed: {error}");
                if skipped {
                    (CommandOutcome::Skipped, false, Some(error))
                } else {
                    (CommandOutcome::Ran, true, Some(error))
                }
            }
        }
    };

    let http = async {
        let Some(url) = action.http.as_ref() else {
            return (false, false, None);
        };

        match execute_fallback_http(url, timeout).await {
            // The alert was delivered only if the endpoint accepted it. A 404
            // from a mistyped webhook, or a 500 from a broken one, is a request
            // that arrived and was refused - not an operator who was told.
            Ok(status) if is_success_status(status) => {
                info!(
                    "Executed fallback HTTP request for {service_name} with status code {status}"
                );
                (true, false, None)
            }
            Ok(status) => {
                // Error level for the reason above: a refused alert is an
                // operator who was never told.
                error!(
                    "Fallback HTTP request for {service_name} was answered with status code {status}"
                );
                (true, true, None)
            }
            Err(error) => {
                warn!("Fallback HTTP request for {service_name} failed: {error}");
                (true, true, Some(error))
            }
        }
    };

    // The actions are independent: a failing command must not skip a
    // configured HTTP alert, so both always run to completion and the
    // command's error is the one reported when both fail.
    let ((command_outcome, command_failed, command_error), (http_ran, http_failed, http_error)) =
        tokio::join!(command, http);

    let outcome = FallbackOutcome {
        command: command_outcome,
        http_ran,
        action_failed: command_failed || http_failed,
    };

    (outcome, command_error.or(http_error).map_or(Ok(()), Err))
}

/// Whether a fallback HTTP alert was accepted by its endpoint.
///
/// Only 2xx counts. Everything else - a redirect that was never followed, a
/// 404 from a mistyped webhook path, a 5xx from a broken receiver - means the
/// request was delivered and refused, which is not an alert anyone received.
const fn is_success_status(status: i32) -> bool {
    status >= 200 && status < 300
}

use std::hash::BuildHasher;

/// Counts a failed check against the service's failure streak.
///
/// Separate from [`should_continue_fallback`] because the streak is a property
/// of the *service*, not of its recovery configuration. While the two were
/// combined, `epazote_consecutive_failures` was only ever incremented for
/// services declaring an `if_not`, so a service with no fallback published a
/// permanent `0` however long it had been down - contradicting the metric it
/// was exported under and making the streak unusable for alerting on exactly
/// the services that cannot repair themselves.
async fn record_check_failure<S: BuildHasher>(
    service_name: &str,
    counters: &Arc<Mutex<HashMap<String, FallbackState, S>>>,
) -> usize {
    let mut counters = counters.lock().await;
    let state = counters.entry(service_name.to_string()).or_default();
    state.consecutive_failures += 1;
    state.consecutive_failures
}

/// Check if stop limit is reached and if we should continue
///
/// The failed check is counted by [`record_check_failure`] before this is
/// called, so the streak read here already includes the current failure.
async fn should_continue_fallback<S: BuildHasher>(
    service_name: &str,
    counters: &Arc<Mutex<HashMap<String, FallbackState, S>>>,
    action: &config::Action,
) -> bool {
    let mut counters = counters.lock().await;
    let state = counters.entry(service_name.to_string()).or_default();

    let threshold = action.threshold.unwrap_or(1);
    if state.consecutive_failures < threshold {
        warn!(
            "Service '{}' failure count {}/{} below threshold, skipping fallback",
            service_name, state.consecutive_failures, threshold
        );
        return false;
    }

    // Once the budget is spent, report the transition only once. Repeating an
    // ERROR on every later scan turns a persistent outage into a journal
    // storm without adding information.
    if let Some(stop) = action.stop
        && state.fallback_executions >= stop
    {
        if !state.stop_exhaustion_reported {
            error!(
                "Service '{}' reached stop limit ({}), skipping fallback",
                service_name, stop
            );
            state.stop_exhaustion_reported = true;
        }
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
        state.fallback_executions = 0;
        state.stop_exhaustion_reported = false;
    }
}

/// Gives back the `stop` attempt taken by a fallback attempt in which nothing
/// ran.
///
/// `should_continue_fallback` counts the attempt before the fallback runs, so a
/// command skipped while waiting for its group's lock has already spent one
/// even though it executed nothing - and `stop` bounds *executions*. That
/// matters because contention only happens when several members of one group
/// fail at once, which is exactly what the group exists for: without this, a
/// service could exhaust its whole `stop` budget on attempts that never
/// restarted anything, and then be skipped for the rest of the outage while
/// still down, never having been restarted once.
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
        state.stop_exhaustion_reported = false;
    }
}

/// Runs the fallback actions and keeps the `stop` budget honest.
///
/// Wraps [`execute_fallbacks`] so that an attempt in which *nothing ran* does
/// not consume one of the service's `stop` attempts. `stop` bounds how many
/// times the fallback actions execute, and a command skipped waiting for its
/// group's lock never executed.
///
/// The decision is made from the [`FallbackOutcome`] the actions report, not
/// from the error that came back. Those are not interchangeable: when a
/// command is skipped while an `if_not.http` alert is configured, the alert is
/// still sent - it never queues - but the command's [`FallbackSkipped`] is
/// the error that wins. Refunding on that error would hand the attempt back
/// even though the service did alert, and with the budget restored on every
/// scan `stop: N` would cap nothing for as long as the group stayed contended -
/// which is exactly the burst of simultaneous failures the group exists to keep
/// quiet.
///
/// Refunding matters in the opposite case for the same reason: contention only
/// happens during such a burst, so charging skips would let a service burn its
/// whole budget on attempts that restarted nothing and then be abandoned while
/// still down, having never been restarted once.
///
/// None of this arises for an ungrouped command: it takes no lock, so it always
/// runs and always spends its attempt.
///
/// The metric label is a separate decision. A skipped command is recorded as
/// `skipped` even when a successful HTTP action ran alongside it: the alert
/// spends the attempt, but it does not make the command run. If an action that
/// did run failed, `failure` takes precedence over the simultaneous skip.
///
/// `consecutive_failures` is never refunded: the check really did fail, and it
/// still counts toward `threshold`.
///
/// The error is returned either way, but it describes the *fallback*, not the
/// check. Every production caller deliberately drops it: a scan that completed
/// and merely failed its expectations is not a scan error, and one that did
/// fail has its own error to report. It is logged here instead, at error level
/// so the default verbosity still carries it, and returned only so a caller
/// that wants to inspect what the recovery did - the tests - still can.
pub(crate) async fn execute_fallbacks_tracking_stop<S: BuildHasher>(
    action: &config::Action,
    context: &FallbackContext<'_>,
    service_name: &str,
    counters: &Arc<Mutex<HashMap<String, FallbackState, S>>>,
    metrics: &ServiceMetrics,
) -> Result<()> {
    let (outcome, result) = execute_fallbacks(action, context, service_name).await;

    if outcome.must_not_count_against_stop() {
        warn!(
            "Service '{service_name}' fallback command never ran and no alert went out, so the attempt does not count against 'stop'"
        );

        restore_fallback_execution(service_name, counters).await;
    }

    // Classify from what the actions reported, never from `result`: the
    // command's error wins over the HTTP one, so a held-back command hides a
    // delivered alert behind `FallbackSkipped`. Every real failure already
    // sets `action_failed`; the error added only the skip to `failure`.
    metrics.record_fallback(service_name, outcome.metric_label());

    // Reported here, and at error level, because the default verbosity is
    // ERROR: the per-action `warn!` above it is invisible with the packaged
    // default. This used to reach an operator only by being
    // propagated to the scan loop and printed as `Error scanning service ...`,
    // which named the recovery as the reason the *check* failed and discarded
    // the check's own error. The line has to survive that fix, as what it
    // actually is.
    if let Err(error) = &result {
        error!("Fallback for service '{service_name}' did not complete: {error}");
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

    /// One failed check, in the order the scan loop performs it.
    ///
    /// Counting the streak and deciding on recovery are deliberately separate
    /// in production - a service with no `if_not` still has to count its
    /// failures - so a test that called only the second half would assert a
    /// sequence that never happens.
    async fn failed_check<S: BuildHasher>(
        service_name: &str,
        counters: &Arc<Mutex<HashMap<String, FallbackState, S>>>,
        action: &config::Action,
    ) -> bool {
        record_check_failure(service_name, counters).await;
        should_continue_fallback(service_name, counters, action).await
    }

    #[test]
    fn test_fallback_metric_label_uses_failure_then_skip_precedence() {
        let cases = [
            (
                FallbackOutcome {
                    command: CommandOutcome::Ran,
                    http_ran: false,
                    action_failed: false,
                },
                metrics::FALLBACK_SUCCESS,
                "a successful command with no alert",
            ),
            (
                FallbackOutcome {
                    command: CommandOutcome::NotConfigured,
                    http_ran: true,
                    action_failed: false,
                },
                metrics::FALLBACK_SUCCESS,
                "a successful alert with no command",
            ),
            (
                FallbackOutcome {
                    command: CommandOutcome::Skipped,
                    http_ran: false,
                    action_failed: false,
                },
                metrics::FALLBACK_SKIPPED,
                "a held-back command with nothing else configured",
            ),
            (
                FallbackOutcome {
                    command: CommandOutcome::Skipped,
                    http_ran: true,
                    action_failed: false,
                },
                metrics::FALLBACK_SKIPPED,
                "a delivered alert does not erase the command skip",
            ),
            (
                FallbackOutcome {
                    command: CommandOutcome::Skipped,
                    http_ran: true,
                    action_failed: true,
                },
                metrics::FALLBACK_FAILURE,
                "a failed alert outranks the simultaneous command skip",
            ),
            (
                FallbackOutcome {
                    command: CommandOutcome::Ran,
                    http_ran: true,
                    action_failed: true,
                },
                metrics::FALLBACK_FAILURE,
                "any action failure makes the whole attempt a failure",
            ),
        ];

        for (outcome, expected, why) in cases {
            assert_eq!(outcome.metric_label(), expected, "{why}");
        }
    }

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

    /// Cancelling a service task during shutdown must kill the whole fallback
    /// process group, not only drop the shell that Epazote spawned.
    #[tokio::test]
    #[cfg(unix)]
    async fn test_cancelling_command_execution_kills_descendants() {
        let tempdir = tempfile::Builder::new()
            .prefix("epazote-cancel-pgroup-")
            .tempdir_in(".")
            .expect("Failed to create temp dir");
        let marker = tempdir.path().join("descendant.txt");
        let started = tempdir.path().join("started.txt");
        // The descendant announces itself so the abort below lands after it
        // provably exists. Sleeping a fixed 200ms instead let the abort arrive
        // before the shell had forked, in which case the missing marker proved
        // nothing at all.
        let cmd = format!(
            "( echo ready > {started} ; sleep 2 && echo alive > {marker} ) & sleep 30",
            started = started.display(),
            marker = marker.display()
        );

        let execution =
            tokio::spawn(async move { execute_command(&cmd, Duration::from_secs(30)).await });

        let mut descendant_running = false;
        for _ in 0..100 {
            if started.exists() {
                descendant_running = true;
                break;
            }

            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        assert!(
            descendant_running,
            "the descendant never started, so cancelling proves nothing"
        );

        execution.abort();
        let result = execution.await;
        assert!(
            matches!(result, Err(error) if error.is_cancelled()),
            "the command task must be cancelled"
        );

        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(
            !marker.exists(),
            "cancelling command execution must kill every process in its group"
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
            group: None,
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
    async fn test_health_check_command_timeout_names_service_setting() {
        let started = std::time::Instant::now();
        let result = execute_command("sleep 30", Duration::from_millis(200)).await;

        let error = result.expect_err("a hanging command must return an error");
        let message = format!("{error:#}");
        assert!(
            message.contains("the service 'timeout'"),
            "a health-check timeout must name the service setting, got: {message}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "execute_command should return near the timeout, took {:?}",
            started.elapsed()
        );
    }

    /// The generic command runner serves both health checks and fallbacks, but
    /// their timeout settings are different. A fallback diagnostic must point
    /// at the key that can actually change its budget.
    #[tokio::test]
    async fn test_fallback_command_timeout_names_if_not_setting() {
        let error = execute_fallback_command(
            "sleep 30",
            &failing_context(),
            Duration::from_millis(200),
            None,
        )
        .await
        .expect_err("a hanging fallback command must return an error");

        let message = format!("{error:#}");
        assert!(
            message.contains("'if_not.timeout'"),
            "a fallback timeout must name its fallback setting, got: {message}"
        );
        assert!(
            !message.contains("the service 'timeout'"),
            "a fallback timeout must not point at the health-check setting, got: {message}"
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
            None,
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
            None,
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

    /// Writes a `start`/`end` pair around a short sleep, so overlapping runs
    /// are visible as two `start` lines landing back to back.
    fn overlap_probe_cmd(log: &str) -> String {
        format!("printf 'start\\n' >> {log}; sleep 0.3; printf 'end\\n' >> {log}")
    }

    #[tokio::test]
    async fn test_fallback_commands_in_the_same_group_do_not_overlap() {
        // Two services in one group failing on the same tick must not run their
        // restart scripts concurrently: that is the whole reason to declare a
        // group, and without it a shared script's log interleaves and a shared
        // host takes a burst of restarts at once.
        let tempdir = tempfile::Builder::new()
            .prefix("epazote-overlap-dir-")
            .tempdir_in(".")
            .expect("Failed to create temp dir");
        let log_path = tempdir.path().join("order.log");
        let log = log_path.to_str().expect("Invalid path");

        let cmd = overlap_probe_cmd(log);

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

        // A group name unique to this test, so the serialization observed here
        // is this test's own and not contention with a concurrently running
        // one.
        let group = Some("same-group-serializes");

        let first = execute_fallback_command(&cmd, &context, Duration::from_secs(30), group);
        let second = execute_fallback_command(&cmd, &context, Duration::from_secs(30), group);
        let (first, second) = tokio::join!(first, second);
        first.expect("first fallback command failed");
        second.expect("second fallback command failed");

        let output = fs::read_to_string(&log_path).expect("Failed to read order log");
        assert_eq!(
            output.lines().collect::<Vec<_>>(),
            vec!["start", "end", "start", "end"],
            "fallback commands in the same group overlapped: {output:?}"
        );
    }

    /// Regression for the defect this whole design exists to fix: 4.1.0
    /// serialized *every* fallback command process-wide, so a service waited
    /// behind the restart of another it shared nothing with - and was skipped
    /// outright once that wait exceeded its `if_not.timeout`. A command with no
    /// `group` declares no conflict, so it must take no lock and start
    /// immediately.
    #[tokio::test]
    async fn test_ungrouped_fallback_commands_run_concurrently() {
        let tempdir = tempfile::Builder::new()
            .prefix("epazote-concurrent-dir-")
            .tempdir_in(".")
            .expect("Failed to create temp dir");
        let log_path = tempdir.path().join("order.log");
        let log = log_path.to_str().expect("Invalid path");

        let cmd = overlap_probe_cmd(log);
        let context = failing_context();

        let first = execute_fallback_command(&cmd, &context, Duration::from_secs(30), None);
        let second = execute_fallback_command(&cmd, &context, Duration::from_secs(30), None);
        let (first, second) = tokio::join!(first, second);
        first.expect("first fallback command failed");
        second.expect("second fallback command failed");

        let output = fs::read_to_string(&log_path).expect("Failed to read order log");
        assert_eq!(
            output.lines().collect::<Vec<_>>(),
            vec!["start", "start", "end", "end"],
            "ungrouped fallback commands must not queue behind each other: {output:?}"
        );
    }

    /// Groups partition the queue rather than merely renaming one global one:
    /// a member of one group must not wait on a member of another.
    #[tokio::test]
    async fn test_fallback_commands_in_different_groups_run_concurrently() {
        let tempdir = tempfile::Builder::new()
            .prefix("epazote-groups-dir-")
            .tempdir_in(".")
            .expect("Failed to create temp dir");
        let log_path = tempdir.path().join("order.log");
        let log = log_path.to_str().expect("Invalid path");

        let cmd = overlap_probe_cmd(log);
        let context = failing_context();

        let first = execute_fallback_command(
            &cmd,
            &context,
            Duration::from_secs(30),
            Some("distinct-group-a"),
        );
        let second = execute_fallback_command(
            &cmd,
            &context,
            Duration::from_secs(30),
            Some("distinct-group-b"),
        );
        let (first, second) = tokio::join!(first, second);
        first.expect("first fallback command failed");
        second.expect("second fallback command failed");

        let output = fs::read_to_string(&log_path).expect("Failed to read order log");
        assert_eq!(
            output.lines().collect::<Vec<_>>(),
            vec!["start", "start", "end", "end"],
            "different groups must not serialize against each other: {output:?}"
        );
    }

    /// The registry must hand out one lock per group name, or "same group"
    /// would not serialize anything.
    #[tokio::test]
    async fn test_group_locks_are_shared_by_name_and_distinct_across_names() {
        let locks = FallbackGroupLocks::default();

        let first = locks.lock_for("shared").await;
        let again = locks.lock_for("shared").await;
        let other = locks.lock_for("other").await;

        assert!(
            Arc::ptr_eq(&first, &again),
            "the same group name must resolve to the same lock"
        );
        assert!(
            !Arc::ptr_eq(&first, &other),
            "different group names must resolve to different locks"
        );
    }

    // --- End-to-end coverage: real YAML through Config into the lock ---
    //
    // The tests above drive `execute_fallback_command` with a group passed in
    // by hand, which proves the lock behaves - but not that a `group:` key in a
    // configuration file ever reaches it. Everything below parses real YAML
    // with `Config::new` and runs the fallbacks through `execute_fallbacks`, so
    // a break anywhere in that chain - the field, `Action::group_name`, or the
    // call in `execute_fallbacks` - fails here rather than shipping.

    /// Parses YAML exactly as the binary does, including validation.
    fn config_from_yaml(yaml: &str) -> config::Config {
        let file = tempfile::NamedTempFile::new().expect("Failed to create temp config");
        fs::write(file.path(), yaml).expect("Failed to write temp config");

        config::Config::new(file.path().to_path_buf()).expect("Failed to load config")
    }

    /// The `if_not` block of a configured service.
    fn fallback_action<'a>(config: &'a config::Config, service: &str) -> &'a config::Action {
        config
            .get_service(service)
            .expect("service not found")
            .expect
            .if_not
            .as_ref()
            .expect("if_not not found")
    }

    /// A shell command that brackets a short sleep with `start-`/`end-` markers,
    /// so overlap is visible as two `start-` lines with no `end-` between them.
    ///
    /// Deliberately free of quotes and backslashes so it embeds in YAML as a
    /// plain scalar, keeping the test configs readable as configuration rather
    /// than as escaping.
    fn marker_cmd(log: &std::path::Path, name: &str) -> String {
        let log = log.to_str().expect("Invalid log path");

        format!("echo start-{name} >> {log}; sleep 0.3; echo end-{name} >> {log}")
    }

    fn marker_context(service_name: &str) -> FallbackContext<'_> {
        FallbackContext {
            service_name,
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

    fn marker_dir() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("epazote-config-group-")
            .tempdir_in(".")
            .expect("Failed to create temp dir")
    }

    fn read_markers(log: &std::path::Path) -> Vec<String> {
        fs::read_to_string(log)
            .expect("Failed to read marker log")
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Runs every named service's fallbacks at once, as a shared dependency
    /// failing would.
    async fn run_fallbacks_together(config: &config::Config, services: &[&str]) {
        let contexts: Vec<_> = services.iter().map(|name| marker_context(name)).collect();

        let runs = services
            .iter()
            .zip(&contexts)
            .map(|(name, context)| execute_fallbacks(fallback_action(config, name), context, name));

        for (outcome, result) in futures::future::join_all(runs).await {
            assert!(result.is_ok(), "fallback failed: {result:?}");
            assert!(
                outcome.command != CommandOutcome::Skipped,
                "no command should have been skipped in this scenario"
            );
        }
    }

    /// The markers belonging to `names`, in the order they were written.
    fn markers_for(markers: &[String], names: &[&str]) -> Vec<String> {
        markers
            .iter()
            .filter(|line| names.iter().any(|name| line.ends_with(&format!("-{name}"))))
            .cloned()
            .collect()
    }

    /// Asserts two services took turns: one ran to completion before the other
    /// started, in whichever order they happened to acquire the lock.
    fn assert_serialized(markers: &[String], first: &str, second: &str) {
        let observed = markers_for(markers, &[first, second]);

        let one = vec![
            format!("start-{first}"),
            format!("end-{first}"),
            format!("start-{second}"),
            format!("end-{second}"),
        ];
        let other = vec![
            format!("start-{second}"),
            format!("end-{second}"),
            format!("start-{first}"),
            format!("end-{first}"),
        ];

        assert!(
            observed == one || observed == other,
            "'{first}' and '{second}' share a group and must not overlap, got {observed:?}"
        );
    }

    /// Asserts two services overlapped: both started before either finished.
    fn assert_concurrent(markers: &[String], first: &str, second: &str) {
        let observed = markers_for(markers, &[first, second]);

        let starts_first = observed
            .iter()
            .take(2)
            .filter(|line| line.starts_with("start-"))
            .count();

        assert_eq!(
            starts_first, 2,
            "'{first}' and '{second}' declare no shared group and must overlap, got {observed:?}"
        );
    }

    /// A config declaring no groups at all must behave as epazote did before
    /// 4.1.0: every fallback starts the moment its check fails.
    ///
    /// This is the regression for issue #22. Under 4.1.0's process-wide lock
    /// these two services - which share nothing - queued behind each other.
    #[tokio::test]
    async fn test_config_without_any_group_runs_fallbacks_concurrently() {
        let dir = marker_dir();
        let log = dir.path().join("markers.log");

        let yaml = format!(
            "
services:
  alpha:
    test: true
    every: 30s
    expect:
      status: 0
      if_not:
        cmd: {alpha}
  beta:
    test: true
    every: 30s
    expect:
      status: 0
      if_not:
        cmd: {beta}
",
            alpha = marker_cmd(&log, "alpha"),
            beta = marker_cmd(&log, "beta"),
        );

        let config = config_from_yaml(&yaml);

        assert_eq!(fallback_action(&config, "alpha").group_name(), None);
        assert_eq!(fallback_action(&config, "beta").group_name(), None);

        run_fallbacks_together(&config, &["alpha", "beta"]).await;

        assert_concurrent(&read_markers(&log), "alpha", "beta");
    }

    /// The same two services, changed only by adding a shared `group`, must now
    /// take turns. Nothing else about the configuration differs, so the group
    /// key is the only thing that can account for the change.
    #[tokio::test]
    async fn test_same_config_with_a_shared_group_serializes_fallbacks() {
        let dir = marker_dir();
        let log = dir.path().join("markers.log");

        let yaml = format!(
            "
services:
  alpha:
    test: true
    every: 30s
    expect:
      status: 0
      if_not:
        group: config-shared-group
        cmd: {alpha}
  beta:
    test: true
    every: 30s
    expect:
      status: 0
      if_not:
        group: config-shared-group
        cmd: {beta}
",
            alpha = marker_cmd(&log, "alpha"),
            beta = marker_cmd(&log, "beta"),
        );

        let config = config_from_yaml(&yaml);

        assert_eq!(
            fallback_action(&config, "alpha").group_name(),
            Some("config-shared-group")
        );
        assert_eq!(
            fallback_action(&config, "beta").group_name(),
            Some("config-shared-group")
        );

        run_fallbacks_together(&config, &["alpha", "beta"]).await;

        assert_serialized(&read_markers(&log), "alpha", "beta");
    }

    /// Two groups in one file must not serialize against each other. A group
    /// that queued behind an unrelated group would just be the process-wide
    /// lock again, wearing a different name.
    #[tokio::test]
    async fn test_distinct_groups_in_one_config_do_not_serialize_against_each_other() {
        let dir = marker_dir();
        let log = dir.path().join("markers.log");

        let yaml = format!(
            "
services:
  alpha:
    test: true
    every: 30s
    expect:
      status: 0
      if_not:
        group: config-distinct-one
        cmd: {alpha}
  beta:
    test: true
    every: 30s
    expect:
      status: 0
      if_not:
        group: config-distinct-two
        cmd: {beta}
",
            alpha = marker_cmd(&log, "alpha"),
            beta = marker_cmd(&log, "beta"),
        );

        let config = config_from_yaml(&yaml);

        run_fallbacks_together(&config, &["alpha", "beta"]).await;

        assert_concurrent(&read_markers(&log), "alpha", "beta");
    }

    /// The realistic case: one file mixing a group with ungrouped services.
    ///
    /// Both halves must hold at once - the grouped pair takes turns while the
    /// ungrouped services ignore them entirely. Getting only one half right is
    /// exactly how this feature fails: serializing everything is 4.1.0, and
    /// serializing nothing is 4.0.
    #[tokio::test]
    async fn test_mixed_config_serializes_only_the_grouped_services() {
        let dir = marker_dir();
        let log = dir.path().join("markers.log");

        let yaml = format!(
            "
services:
  db-primary:
    test: true
    every: 30s
    expect:
      status: 0
      if_not:
        group: config-mixed-db
        cmd: {primary}
  db-replica:
    test: true
    every: 30s
    expect:
      status: 0
      if_not:
        group: config-mixed-db
        cmd: {replica}
  edge-cache:
    test: true
    every: 30s
    expect:
      status: 0
      if_not:
        cmd: {cache}
  marketing:
    test: true
    every: 30s
    expect:
      status: 0
      if_not:
        cmd: {marketing}
",
            primary = marker_cmd(&log, "primary"),
            replica = marker_cmd(&log, "replica"),
            cache = marker_cmd(&log, "cache"),
            marketing = marker_cmd(&log, "marketing"),
        );

        let config = config_from_yaml(&yaml);

        run_fallbacks_together(
            &config,
            &["db-primary", "db-replica", "edge-cache", "marketing"],
        )
        .await;

        let markers = read_markers(&log);

        assert_serialized(&markers, "primary", "replica");
        assert_concurrent(&markers, "cache", "marketing");

        // The ungrouped services must not have waited on the group either: a
        // command that queued would only start after some other command had
        // finished, so its start cannot follow any `end-` marker.
        let first_end = markers
            .iter()
            .position(|line| line.starts_with("end-"))
            .expect("no command finished");

        for service in ["cache", "marketing"] {
            let start = markers
                .iter()
                .position(|line| line == &format!("start-{service}"))
                .expect("ungrouped service never started");

            assert!(
                start < first_end,
                "ungrouped '{service}' waited for a command to finish: {markers:?}"
            );
        }
    }

    /// A group declared in YAML must lock against *that* name, not merely some
    /// lock. Holding the group's lock by name from outside must be enough to
    /// starve the configured service, which is only true if the name survives
    /// the whole path from file to registry.
    #[tokio::test]
    async fn test_group_from_config_locks_against_its_declared_name() {
        let dir = marker_dir();
        let log = dir.path().join("markers.log");

        let yaml = format!(
            "
services:
  blocked:
    test: true
    every: 30s
    expect:
      status: 0
      if_not:
        group: config-named-lock
        timeout: 1s
        cmd: {cmd}
",
            cmd = marker_cmd(&log, "blocked"),
        );

        let config = config_from_yaml(&yaml);
        let held = hold_group_lock("config-named-lock").await;

        let context = marker_context("blocked");
        let (outcome, result) =
            execute_fallbacks(fallback_action(&config, "blocked"), &context, "blocked").await;

        drop(held);

        assert!(
            outcome.command == CommandOutcome::Skipped,
            "a command whose declared group is held must be skipped"
        );
        assert!(result.is_err(), "a skipped command must report an error");
        assert!(
            !log.exists(),
            "the command must never have run while its group was held"
        );
        assert!(
            outcome.must_not_count_against_stop(),
            "nothing ran, so the attempt must be refunded to the stop budget"
        );
    }

    /// The mirror image: an ungrouped service takes no lock, so no amount of
    /// contention elsewhere can starve it. This is the property that makes the
    /// ungrouped default safe - it cannot be skipped, so it cannot be delayed
    /// into a timeout by an unrelated slow restart.
    #[tokio::test]
    async fn test_ungrouped_service_from_config_is_never_starved_by_a_held_group() {
        let dir = marker_dir();
        let log = dir.path().join("markers.log");

        let yaml = format!(
            "
services:
  free:
    test: true
    every: 30s
    expect:
      status: 0
      if_not:
        timeout: 5s
        cmd: {cmd}
",
            cmd = marker_cmd(&log, "free"),
        );

        let config = config_from_yaml(&yaml);
        let held = hold_group_lock("config-unrelated-busy-group").await;

        let context = marker_context("free");
        let (outcome, result) =
            execute_fallbacks(fallback_action(&config, "free"), &context, "free").await;

        drop(held);

        assert!(result.is_ok(), "an ungrouped command must run: {result:?}");
        assert!(
            outcome.command == CommandOutcome::Ran,
            "an ungrouped command must execute"
        );
        assert!(
            outcome.command != CommandOutcome::Skipped,
            "an ungrouped command can never be skipped"
        );
        assert!(
            !outcome.must_not_count_against_stop(),
            "an ungrouped command always runs, so it always spends its stop attempt"
        );
        assert_eq!(read_markers(&log), vec!["start-free", "end-free"]);
    }

    /// Giving every service one group is the documented way back to 4.1.0's
    /// process-wide serialization, so it has to actually work.
    #[tokio::test]
    async fn test_one_group_for_every_service_restores_full_serialization() {
        let dir = marker_dir();
        let log = dir.path().join("markers.log");

        let yaml = format!(
            "
services:
  one:
    test: true
    every: 30s
    expect:
      status: 0
      if_not:
        group: config-everything
        cmd: {one}
  two:
    test: true
    every: 30s
    expect:
      status: 0
      if_not:
        group: config-everything
        cmd: {two}
  three:
    test: true
    every: 30s
    expect:
      status: 0
      if_not:
        group: config-everything
        cmd: {three}
",
            one = marker_cmd(&log, "one"),
            two = marker_cmd(&log, "two"),
            three = marker_cmd(&log, "three"),
        );

        let config = config_from_yaml(&yaml);

        run_fallbacks_together(&config, &["one", "two", "three"]).await;

        let markers = read_markers(&log);

        assert_serialized(&markers, "one", "two");
        assert_serialized(&markers, "one", "three");
        assert_serialized(&markers, "two", "three");
    }

    /// Regression: a skipped command keeps its `stop` attempt when an `http`
    /// alert went out alongside it. The alert takes no lock and was really
    /// sent, so it is an execution - refunding it would uncap alerting for as
    /// long as the contention lasted.
    #[tokio::test]
    async fn test_grouped_skip_with_an_http_alert_still_spends_its_stop_attempt() {
        let mut server = Server::new_async().await;
        let mock = server
            .mock("GET", "/alert")
            .with_status(200)
            .create_async()
            .await;

        let dir = marker_dir();
        let log = dir.path().join("markers.log");

        let yaml = format!(
            "
services:
  alerted:
    test: true
    every: 30s
    expect:
      status: 0
      if_not:
        group: config-skip-with-alert
        timeout: 1s
        cmd: {cmd}
        http: {url}/alert
",
            cmd = marker_cmd(&log, "alerted"),
            url = server.url(),
        );

        let config = config_from_yaml(&yaml);
        let held = hold_group_lock("config-skip-with-alert").await;

        let context = marker_context("alerted");
        let (outcome, _) =
            execute_fallbacks(fallback_action(&config, "alerted"), &context, "alerted").await;

        drop(held);
        mock.assert_async().await;

        assert!(
            outcome.command == CommandOutcome::Skipped,
            "the command must have been skipped"
        );
        assert!(outcome.http_ran, "the alert must still have been sent");
        assert!(
            !outcome.must_not_count_against_stop(),
            "an alert that was sent is an execution, so the attempt is not refunded"
        );
    }

    /// Regression: a delivered alert spends the fallback attempt, but it does
    /// not turn the command that never got its group lock into a failure.
    #[tokio::test]
    async fn test_a_skipped_command_with_a_delivered_alert_is_not_recorded_as_a_failure() {
        let mut server = Server::new_async().await;
        let mock = server
            .mock("GET", "/alert")
            .with_status(200)
            .create_async()
            .await;

        let dir = marker_dir();
        let log = dir.path().join("markers.log");

        let yaml = format!(
            "
services:
  alerted:
    test: true
    every: 30s
    expect:
      status: 0
      if_not:
        group: metrics-skip-with-alert
        timeout: 1s
        stop: 1
        cmd: {cmd}
        http: {url}/alert
",
            cmd = marker_cmd(&log, "alerted"),
            url = server.url(),
        );

        let config = config_from_yaml(&yaml);
        let action = fallback_action(&config, "alerted");
        let held = hold_group_lock("metrics-skip-with-alert").await;
        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let metrics = ServiceMetrics::new().expect("metrics");

        assert!(
            failed_check("alerted", &counters, action).await,
            "the first failed check must run the fallback"
        );

        let error = execute_fallbacks_tracking_stop(
            action,
            &marker_context("alerted"),
            "alerted",
            &counters,
            &metrics,
        )
        .await
        .expect_err("the command skip must still be reported");

        drop(held);
        mock.assert_async().await;

        assert!(
            error.downcast_ref::<FallbackSkipped>().is_some(),
            "the returned error must preserve the typed command skip, got: {error}"
        );

        let outcomes = recorded_fallback_outcomes(&metrics, "alerted");

        assert_eq!(
            outcomes,
            [0, 0, 1],
            "the delivered alert spends the attempt, but the held-back command makes its metric outcome skipped"
        );
        assert_eq!(
            outcomes.into_iter().sum::<u64>(),
            1,
            "one fallback attempt must be recorded exactly once"
        );

        let state = get_fallback_state("alerted", &counters)
            .await
            .expect("the service must have fallback state");
        assert_eq!(
            state.fallback_executions, 1,
            "the delivered alert must keep the stop attempt spent"
        );
        assert!(
            !failed_check("alerted", &counters, action).await,
            "stop: 1 must prevent a second fallback after the delivered alert"
        );
    }

    /// Regression: a fallback command in a contended group can sit queued
    /// behind another member's restart for as long as that restart's timeout
    /// (5 minutes by default). The HTTP alert must not inherit that wait - it
    /// is often the only way an operator learns anything happened - so it runs
    /// concurrently with the command.
    #[tokio::test]
    async fn test_fallback_http_alert_is_not_delayed_by_a_queued_command() {
        let mut server = Server::new_async().await;
        let mock = server
            .mock("GET", "/alert")
            .with_status(200)
            .create_async()
            .await;

        let group = "alert-not-delayed";

        // Stand in for another member of this group whose command is still
        // running. The mutex is fair, so the command below queues behind this
        // guard and nothing else can overtake it.
        let guard = hold_group_lock(group).await;

        let action = config::Action {
            cmd: Some("exit 0".to_string()),
            http: Some(format!("{}/alert", server.url())),
            stop: None,
            threshold: Some(1),
            group: Some(group.to_string()),
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
            "the HTTP alert must fire while the fallback command is still queued behind its group"
        );

        drop(guard);
    }

    /// Regression: waiting for a group's fallback lock is bounded. The caller
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

        let group = "bounded-wait";

        // Held for the whole test, standing in for another member of this group
        // whose fallback outlasts the waiter's patience.
        let guard = hold_group_lock(group).await;

        let cmd = format!("touch {}", marker.display());
        let started = std::time::Instant::now();
        let error =
            execute_fallback_command(&cmd, &context, Duration::from_millis(200), Some(group))
                .await
                .expect_err("a fallback that never got the lock must report an error");

        assert!(
            error.to_string().contains("fallback command skipped"),
            "the error must say the command was skipped, got: {error}"
        );
        assert!(
            error.to_string().contains(group),
            "the error must name the group that blocked it, got: {error}"
        );
        assert!(
            error.to_string().contains("'if_not.timeout'"),
            "the error must name the setting that bounds the group wait, got: {error}"
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
    /// `execute_fallback_command` applies, but keeping it off the shared
    /// registry means no other test can perturb the timings this one measures.
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
        let exit_code = run_command_under_lock(&lock, "sleep 1", &context, budget, "full-budget")
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
            // Fails immediately rather than by timing out, so this test stays
            // about error precedence and nothing else.
            cmd: Some("kill -9 $$".to_string()),
            http: Some(resetting_endpoint_url("http", "/alert")),
            stop: None,
            threshold: Some(1),
            // Ungrouped, so the command takes no lock and can never be skipped
            // - the only way it fails here is by running.
            group: None,
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
            http: Some(resetting_endpoint_url("http", "/alert")),
            stop: None,
            threshold: Some(1),
            // Ungrouped, so the command never queues; the reset endpoint fails
            // the request immediately either way.
            group: None,
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
    /// its group's lock, and that must not take the alert down with it. This is
    /// exactly the moment an operator most needs to hear about the failure -
    /// the group is busy enough that recovery is being dropped.
    #[tokio::test]
    async fn test_fallback_http_alert_fires_when_the_command_is_skipped_for_the_lock() {
        let mut server = Server::new_async().await;
        let mock = server
            .mock("GET", "/alert")
            .with_status(200)
            .create_async()
            .await;

        let context = failing_context();
        let group = "alert-on-skip";

        // Held for the whole test, so the command below can never get its turn.
        let guard = hold_group_lock(group).await;

        let action = config::Action {
            cmd: Some("exit 0".to_string()),
            http: Some(format!("{}/alert", server.url())),
            stop: None,
            threshold: Some(1),
            group: Some(group.to_string()),
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
                expected_outcome: metrics::FALLBACK_SKIPPED,
                why: "nothing ran: the command never got the lock and no alert was configured, so the attempt must be handed back",
            },
            StopCase {
                service: "skipped-with-alert",
                with_cmd: true,
                alert: Alert::Reachable,
                contended: true,
                expected_executions: 1,
                expected_outcome: metrics::FALLBACK_SKIPPED,
                why: "the alert takes no lock and was sent, so the attempt is spent even though the command was skipped",
            },
            StopCase {
                service: "skipped-with-undeliverable-alert",
                with_cmd: true,
                alert: Alert::Undeliverable,
                contended: true,
                expected_executions: 1,
                expected_outcome: metrics::FALLBACK_FAILURE,
                why: "the alert was issued and merely failed - nothing held it back - so it is an execution and spends the attempt, exactly like a command that runs and fails",
            },
            StopCase {
                service: "ran-without-alert",
                with_cmd: true,
                alert: Alert::None,
                contended: false,
                expected_executions: 1,
                expected_outcome: metrics::FALLBACK_SUCCESS,
                why: "a command that ran is an execution and spends its attempt",
            },
            StopCase {
                service: "ran-with-alert",
                with_cmd: true,
                alert: Alert::Reachable,
                contended: false,
                expected_executions: 1,
                expected_outcome: metrics::FALLBACK_SUCCESS,
                why: "both actions ran, so the attempt is spent",
            },
            StopCase {
                service: "alert-only",
                with_cmd: false,
                alert: Alert::Reachable,
                contended: false,
                expected_executions: 1,
                expected_outcome: metrics::FALLBACK_SUCCESS,
                why: "an alert with no command to wait for always executes",
            },
            StopCase {
                service: "alert-only-undeliverable",
                with_cmd: false,
                alert: Alert::Undeliverable,
                contended: false,
                expected_executions: 1,
                expected_outcome: metrics::FALLBACK_FAILURE,
                why: "an alert that failed to arrive still ran, so it spends the attempt just as a delivered one does",
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
        /// Hold the row's group lock so a configured command never gets a turn.
        contended: bool,
        expected_executions: usize,
        expected_outcome: &'static str,
        why: &'static str,
    }

    /// Fallback outcomes in success/failure/skipped order.
    fn recorded_fallback_outcomes(metrics: &ServiceMetrics, service: &str) -> [u64; 3] {
        let recorded = |outcome: &str| {
            metrics
                .epazote_fallback_executions_total
                .with_label_values(&[service, outcome])
                .get()
        };

        [
            recorded(metrics::FALLBACK_SUCCESS),
            recorded(metrics::FALLBACK_FAILURE),
            recorded(metrics::FALLBACK_SKIPPED),
        ]
    }

    fn assert_recorded_stop_outcome(metrics: &ServiceMetrics, case: &StopCase) {
        let observed = recorded_fallback_outcomes(metrics, case.service);
        let expected = [
            u64::from(case.expected_outcome == metrics::FALLBACK_SUCCESS),
            u64::from(case.expected_outcome == metrics::FALLBACK_FAILURE),
            u64::from(case.expected_outcome == metrics::FALLBACK_SKIPPED),
        ];

        assert_eq!(
            observed, expected,
            "[{}] fallback outcome must reflect what the actions reported: {}",
            case.service, case.why
        );
        assert_eq!(
            observed.into_iter().sum::<u64>(),
            1,
            "[{}] one fallback attempt must be recorded exactly once",
            case.service
        );
    }

    /// What a row's `if_not.http` points at.
    ///
    /// Delivery is a separate axis from configuration because only one thing
    /// can *prevent* an action from executing - a contended group command lock -
    /// and an alert never takes it. An alert that was issued and failed still ran,
    /// so it still spends the attempt; modelling it as a third state keeps
    /// "no alert configured" and "alert that did not arrive" from being
    /// confused for each other.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Alert {
        /// No `http` configured.
        None,
        /// A mock that answers, so the request is issued and delivered.
        Reachable,
        /// A reset endpoint: the request is issued but never delivered.
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
                Alert::Undeliverable => Some(resetting_endpoint_url("http", "/alert")),
            },
            // A single attempt, so spending it wrongly is unrecoverable.
            stop: Some(1),
            threshold: Some(1),
            // Only a grouped command can be held back at all, so contention is
            // expressed by declaring one. The case's own service name keeps
            // that group unique to this case.
            group: case.contended.then(|| case.service.to_string()),
            timeout: Some(if case.contended {
                // Short, so the queued command gives up while the guard is
                // held.
                Duration::from_millis(200)
            } else {
                // Generous, so an uncontended command can only fail by running.
                Duration::from_secs(30)
            }),
        };

        // Stand in for another member of this group whose fallback is still
        // running.
        let guard = if case.contended {
            Some(hold_group_lock(case.service).await)
        } else {
            None
        };

        assert!(
            failed_check(case.service, &counters, &action).await,
            "[{}] the first failed check must be allowed to run its fallback",
            case.service
        );

        let metrics = ServiceMetrics::new().expect("metrics");

        let result = execute_fallbacks_tracking_stop(
            &action,
            &failing_context(),
            case.service,
            &counters,
            &metrics,
        )
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

        assert_recorded_stop_outcome(&metrics, case);

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
            failed_check(case.service, &counters, &action).await,
            case.expected_executions == 0,
            "[{}] the next failed check must only run a fallback while the budget is unspent",
            case.service
        );
    }

    /// Regression: the recorded outcome must follow what the recovery actions
    /// actually reported, not merely whether epazote managed to invoke them.
    ///
    /// `execute_fallbacks` returned `Ok` for any completed command and any
    /// answered request, so a restart that exited non-zero and a webhook that
    /// answered `500` were both counted under `outcome="success"`. A dashboard
    /// then showed a service being repeatedly and successfully recovered while
    /// it was down, and while its `stop` budget drained on a command that
    /// failed every single time - the exact condition these metrics were added
    /// to make visible.
    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn test_recorded_outcome_follows_what_the_actions_reported() {
        struct Case {
            service: &'static str,
            cmd: Option<&'static str>,
            /// Status the mock alert endpoint answers with, if `http` is set.
            http_status: Option<usize>,
            expect_success: bool,
            why: &'static str,
        }

        let mut server = Server::new_async().await;
        // A path per case, so each mock asserts exactly one request. That
        // matters for the non-2xx cases: a connection error would also be
        // recorded as a failure, and without proof the request arrived the
        // test could pass while testing nothing.
        let ok = server
            .mock("GET", "/http-ok")
            .with_status(200)
            .create_async()
            .await;
        let boom = server
            .mock("GET", "/http-5xx")
            .with_status(500)
            .create_async()
            .await;
        let boom_with_cmd = server
            .mock("GET", "/cmd-ok-http-5xx")
            .with_status(500)
            .create_async()
            .await;

        let cases = [
            Case {
                service: "cmd-ok",
                cmd: Some("exit 0"),
                http_status: None,
                expect_success: true,
                why: "a command that exits 0 repaired the service",
            },
            Case {
                service: "cmd-nonzero",
                cmd: Some("exit 7"),
                http_status: None,
                expect_success: false,
                why: "a command that exits non-zero reported that it did not repair the service",
            },
            Case {
                service: "http-ok",
                cmd: None,
                http_status: Some(200),
                expect_success: true,
                why: "a 2xx means the alert was accepted",
            },
            Case {
                service: "http-5xx",
                cmd: None,
                http_status: Some(500),
                expect_success: false,
                why: "a 500 means the alert was delivered and refused, so nobody was told",
            },
            Case {
                service: "cmd-ok-http-5xx",
                cmd: Some("exit 0"),
                http_status: Some(500),
                expect_success: false,
                why: "one failed action is enough to make the attempt a failure",
            },
        ];

        for case in cases {
            let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
                Arc::new(Mutex::new(HashMap::new()));

            let action = config::Action {
                cmd: case.cmd.map(ToString::to_string),
                http: case
                    .http_status
                    .map(|_| format!("{}/{}", server.url(), case.service)),
                stop: Some(5),
                threshold: Some(1),
                group: None,
                timeout: Some(Duration::from_secs(30)),
            };

            let metrics = ServiceMetrics::new().expect("metrics");

            assert!(
                failed_check(case.service, &counters, &action).await,
                "[{}] the fallback must be due for this case to test anything",
                case.service
            );

            let _ = execute_fallbacks_tracking_stop(
                &action,
                &failing_context(),
                case.service,
                &counters,
                &metrics,
            )
            .await;

            let recorded = |outcome: &str| {
                metrics
                    .epazote_fallback_executions_total
                    .with_label_values(&[case.service, outcome])
                    .get()
            };

            let (expected_success, expected_failure) =
                if case.expect_success { (1, 0) } else { (0, 1) };

            assert_eq!(
                (
                    recorded(metrics::FALLBACK_SUCCESS),
                    recorded(metrics::FALLBACK_FAILURE)
                ),
                (expected_success, expected_failure),
                "[{}] {}",
                case.service,
                case.why
            );

            assert_eq!(
                recorded(metrics::FALLBACK_SKIPPED),
                0,
                "[{}] nothing was contended, so nothing may be recorded as skipped",
                case.service
            );
        }

        ok.assert_async().await;
        boom.assert_async().await;
        boom_with_cmd.assert_async().await;
    }

    /// Regression: the failure streak belongs to the service, not to its
    /// recovery configuration.
    ///
    /// Counting used to happen inside `should_continue_fallback`, which is
    /// only reached when an `if_not` exists, so a service without one
    /// published `epazote_consecutive_failures 0` no matter how long it had
    /// been failing - and a service that cannot repair itself is precisely the
    /// one an operator needs to alert on.
    #[tokio::test]
    async fn test_failures_are_counted_without_an_if_not() {
        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));

        for expected in 1..=3 {
            record_check_failure("no-fallback", &counters).await;

            let state = get_fallback_state("no-fallback", &counters)
                .await
                .expect("state must exist once a check has failed");

            assert_eq!(
                state.consecutive_failures, expected,
                "every failed check must advance the streak with no if_not configured"
            );

            assert_eq!(
                state.fallback_executions, 0,
                "counting a failure must never spend a fallback attempt"
            );
        }

        reset_fallback_state("no-fallback", &counters).await;

        let state = get_fallback_state("no-fallback", &counters)
            .await
            .expect("state survives a reset");

        assert_eq!(
            state.consecutive_failures, 0,
            "a recovered service must clear its streak"
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
            // Ungrouped, so it can only fail by running - never by being
            // skipped waiting for a turn.
            group: None,
            timeout: Some(Duration::from_secs(30)),
        };

        assert!(failed_check("ran", &counters, &action).await);

        let metrics = ServiceMetrics::new().expect("metrics");

        let error = execute_fallbacks_tracking_stop(
            &action,
            &failing_context(),
            "ran",
            &counters,
            &metrics,
        )
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
            !failed_check("ran", &counters, &action).await,
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

        let should_continue = failed_check("test", &counters, &action).await;
        assert!(should_continue);

        let should_continue = failed_check("test", &counters, &action).await;
        assert!(should_continue);

        let should_continue = failed_check("test", &counters, &action).await;
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
        assert!(!failed_check("test", &counters, &action).await);
        assert!(!failed_check("test", &counters, &action).await);

        // At threshold - should execute once
        assert!(failed_check("test", &counters, &action).await);

        // Should stop after first execution
        assert!(!failed_check("test", &counters, &action).await);
        // Later scans remain stopped without reporting exhaustion again.
        assert!(!failed_check("test", &counters, &action).await);

        let counters = counters.lock().await;
        let state = counters.get("test").expect("State not found");
        assert_eq!(state.consecutive_failures, 5);
        assert_eq!(state.fallback_executions, 1);
        assert!(state.stop_exhaustion_reported);
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
        assert!(!failed_check("test", &counters, &action).await);

        // At threshold but stop:0 means never execute
        assert!(!failed_check("test", &counters, &action).await);
        assert!(!failed_check("test", &counters, &action).await);

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

        assert!(!failed_check("test", &counters, &action).await);
        assert!(!failed_check("test", &counters, &action).await);
        assert!(failed_check("test", &counters, &action).await);

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

        assert!(!failed_check("test", &counters, &action).await);
        assert!(failed_check("test", &counters, &action).await);
        assert!(!failed_check("test", &counters, &action).await);

        reset_fallback_state("test", &counters).await;

        assert!(!failed_check("test", &counters, &action).await);
        assert!(failed_check("test", &counters, &action).await);

        let counters = counters.lock().await;
        let state = counters.get("test").expect("State not found");
        assert_eq!(state.consecutive_failures, 2);
        assert_eq!(state.fallback_executions, 1);
        assert!(!state.stop_exhaustion_reported);
    }

    #[tokio::test]
    async fn test_get_fallback_state() {
        let counters = Arc::new(Mutex::new(HashMap::new()));
        let action = config::Action {
            threshold: Some(2),
            ..Default::default()
        };

        assert!(!failed_check("test", &counters, &action).await);

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
