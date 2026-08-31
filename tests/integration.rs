#![allow(deprecated, clippy::unwrap_used, clippy::expect_used)]
use assert_cmd::cargo::CommandCargoExt;
use reqwest::Client;
use std::process::{Child, Command};
use std::time::Duration;
use tempfile::NamedTempFile;
use tokio::time::sleep;

/// Ask Epazote to shut down through the same signal path used by service
/// managers, then fall back to a hard kill if it fails to exit promptly.
async fn stop_epazote(child: &mut Child) {
    #[cfg(unix)]
    {
        let pid = i32::try_from(child.id()).expect("epazote pid should fit in i32");
        // Safe: this targets the exact child process spawned by this test.
        let result = unsafe { libc::kill(pid, libc::SIGTERM) };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            assert_eq!(
                error.raw_os_error(),
                Some(libc::ESRCH),
                "failed to signal epazote: {error}"
            );
        }
    }

    #[cfg(not(unix))]
    {
        child.kill().expect("Failed to stop epazote");
    }

    let mut exit_status = None;

    for _ in 0..100 {
        if let Some(status) = child.try_wait().expect("Failed to inspect epazote process") {
            exit_status = Some(status);
            break;
        }

        sleep(Duration::from_millis(20)).await;
    }

    // Kill first so a failing test cannot leak the process it just caught, then
    // assert. Every test that stops epazote is therefore also a regression test
    // for graceful shutdown - which otherwise rests on the single process-group
    // assertion in `test_a_refunded_skip_does_not_exhaust_the_stop_budget`.
    if exit_status.is_none() {
        child
            .kill()
            .expect("Epazote ignored its shutdown signal and could not be killed");
    }

    let status = exit_status
        .expect("epazote did not stop within 2s of SIGTERM: graceful shutdown is broken");

    // A signalled shutdown is an orderly one, so it must report success. An
    // error here would tell a service manager the process fell over when it was
    // asked to stop.
    assert!(
        status.success(),
        "epazote must exit cleanly when asked to stop, got: {status}"
    );
}

/// The address of a shared endpoint that accepts and immediately drops every
/// connection, so a request to it always fails.
///
/// One listener for the whole test binary, rather than one per call. Nothing is
/// ever served, so every caller can share it whatever path it asks for - which
/// keeps this to a single accept loop instead of a detached thread per test
/// that could never be stopped.
///
/// Preferred over binding a port and releasing it: that leaves a window in
/// which another process can claim the port between setup and the request, and
/// the test then fails against whatever answered.
static RESET_ENDPOINT_ADDR: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
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
fn resetting_endpoint_url(path: &str) -> String {
    format!("http://{}{path}", *RESET_ENDPOINT_ADDR)
}

#[cfg(unix)]
fn process_group_exists(group: i32) -> bool {
    // Safe: signal 0 only checks whether the exact process group exists.
    if unsafe { libc::killpg(group, 0) } == 0 {
        return true;
    }

    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(unix)]
async fn assert_process_group_stopped(group: i32) {
    let mut stopped = false;

    for _ in 0..100 {
        if !process_group_exists(group) {
            stopped = true;
            break;
        }

        sleep(Duration::from_millis(20)).await;
    }

    if !stopped {
        // Keep a failing regression test from leaking the process it detected.
        // Safe: `group` came from the fallback shell started by this test.
        let _ = unsafe { libc::killpg(group, libc::SIGKILL) };
    }

    assert!(
        stopped,
        "the fallback process group {group} survived epazote shutdown"
    );
}

#[cfg(unix)]
async fn assert_recorded_process_group_stopped(path: &std::path::Path) {
    let group = std::fs::read_to_string(path)
        .expect("the fallback must write its process-group id")
        .trim()
        .parse::<i32>()
        .expect("the fallback process-group id must be numeric");

    assert_process_group_stopped(group).await;
}

#[tokio::test]
async fn test_epazote_integration() {
    // 1. Start Mockito Server
    let mut server = mockito::Server::new_async().await;
    let mock_url = server.url();

    let _m = server
        .mock("GET", "/health")
        .with_status(200)
        .create_async()
        .await;

    // 2. Create Config File
    let config_content = format!(
        r"
services:
  test_service:
    url: {mock_url}/health
    every: 1s
    expect:
      status: 200
"
    );

    let config_file = NamedTempFile::new().expect("Failed to create temp file");
    std::fs::write(config_file.path(), config_content).expect("Failed to write config");

    // 3. Pick a random port for metrics (to avoid conflicts)
    // Using port 0 lets OS pick, but epazote needs to know it.
    // We'll pick a likely free port or just let the OS bind and we'd need to parse logs,
    // but epazote takes -p. Let's pick 19090 and hope.
    // Better: let's try to bind a TcpListener to 0, get the port, drop it, and use that.
    let metrics_port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };

    // 4. Spawn Epazote
    let mut cmd = Command::cargo_bin("epazote").expect("Failed to find binary");

    let mut child = cmd
        .arg("-c")
        .arg(config_file.path())
        .arg("-p")
        .arg(metrics_port.to_string())
        .spawn()
        .expect("Failed to start epazote");

    // Give it some time to start and scrape
    // Retry loop to check metrics
    let client = Client::new();
    let metrics_url = format!("http://localhost:{metrics_port}/metrics");

    let mut success = false;
    for _ in 0..10 {
        sleep(Duration::from_secs(1)).await;

        if let Ok(response) = client.get(&metrics_url).send().await
            && response.status().is_success()
        {
            let text = response.text().await.unwrap_or_default();
            println!("Metrics: {text}"); // For debugging if needed

            // Check for specific metric
            if text.contains(r#"epazote_status{service_name="test_service"} 1"#) {
                success = true;
                break;
            }
        }
    }

    // 5. Cleanup
    stop_epazote(&mut child).await;
    let _ = child.wait(); // Wait for it to exit

    assert!(
        success,
        "Failed to verify epazote metrics indicating success"
    );
}

#[tokio::test]
async fn test_epazote_if_not_cmd_integration() {
    // 1. Start Mockito Server that returns failure
    let mut server = mockito::Server::new_async().await;
    let mock_url = server.url();

    let _m = server
        .mock("GET", "/fail")
        .with_status(500)
        .create_async()
        .await;

    // 2. Create a temporary marker file path
    let marker_file = tempfile::NamedTempFile::new().expect("Failed to create marker file");
    let marker_path = marker_file.path().to_owned();
    // Remove the file so we can detect when epazote creates/touches it
    std::fs::remove_file(&marker_path).expect("Failed to remove initial marker file");

    // 3. Create Config File with if_not cmd
    let config_content = format!(
        r"
services:
  fail_service:
    url: {mock_url}/fail
    every: 1s
    expect:
      status: 200
      if_not:
        cmd: touch {}
",
        marker_path.to_str().expect("Invalid marker path")
    );

    let config_file = NamedTempFile::new().expect("Failed to create config file");
    std::fs::write(config_file.path(), config_content).expect("Failed to write config");

    // 4. Pick a random port for metrics
    let metrics_port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };

    // 5. Spawn Epazote
    let mut cmd = Command::cargo_bin("epazote").expect("Failed to find binary");

    let mut child = cmd
        .arg("-c")
        .arg(config_file.path())
        .arg("-p")
        .arg(metrics_port.to_string())
        .spawn()
        .expect("Failed to start epazote");

    // 6. Wait for the marker file to be created by the fallback command
    let mut success = false;
    for _ in 0..10 {
        sleep(Duration::from_secs(1)).await;
        if marker_path.exists() {
            success = true;
            break;
        }
    }

    // 7. Cleanup
    stop_epazote(&mut child).await;
    let _ = child.wait();

    assert!(
        success,
        "Fallback command was not executed (marker file not found)"
    );
}

/// Regression, end to end through the scan loop: a fallback that fails must not
/// be counted as a scan error, and must still say so where an operator can see
/// it.
///
/// Both halves need the real binary. `epazote_failures_total` is incremented
/// only by the scan loop, so a `scan_service`-level test cannot observe it; and
/// the diagnostic's whole point is that it survives the *default* verbosity the
/// packaged service runs with, which only the binary applies.
///
/// The service here answers `500` against `expect.status: 200` - a check that
/// completed and merely failed its expectation, which is deliberately not a scan
/// error - while its `if_not.http` resets the connection so the fallback itself
/// fails.
#[tokio::test]
async fn test_failing_fallback_is_not_a_scan_error_and_is_still_reported() {
    let mut server = mockito::Server::new_async().await;
    let mock_url = server.url();

    let _m = server
        .mock("GET", "/fail")
        .with_status(500)
        .create_async()
        .await;

    let reset_alert_url = resetting_endpoint_url("/hook");

    let config_content = format!(
        r"
services:
  alerting_service:
    url: {mock_url}/fail
    every: 1s
    expect:
      status: 200
      if_not:
        http: {reset_alert_url}
        timeout: 2s
"
    );

    let config_file = NamedTempFile::new().expect("Failed to create config file");
    std::fs::write(config_file.path(), config_content).expect("Failed to write config");

    let metrics_port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };

    let mut cmd = Command::cargo_bin("epazote").expect("Failed to find binary");

    // No `-v`: the diagnostic has to reach an operator at the default verbosity,
    // which is what the packaged unit runs with.
    let mut child = cmd
        .arg("-c")
        .arg(config_file.path())
        .arg("-p")
        .arg(metrics_port.to_string())
        // The tracing subscriber writes to stdout, not stderr.
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("Failed to start epazote");

    let client = Client::new();
    let metrics_url = format!("http://localhost:{metrics_port}/metrics");

    let mut metrics = String::new();
    for _ in 0..15 {
        sleep(Duration::from_secs(1)).await;

        if let Ok(response) = client.get(&metrics_url).send().await
            && response.status().is_success()
        {
            let text = response.text().await.unwrap_or_default();

            // Wait until the fallback has actually been attempted and failed,
            // so the counter assertion below is read after the event that used
            // to move it.
            if metric_value(&text, ALERT_FAILURE) >= 1.0 {
                metrics = text;
                break;
            }
        }
    }

    stop_epazote(&mut child).await;
    let output = child
        .wait_with_output()
        .expect("Failed to collect epazote output");
    let logs = String::from_utf8_lossy(&output.stdout);

    assert!(
        !metrics.is_empty(),
        "the fallback never reported a failure, so nothing was exercised; logs:\n{logs}"
    );

    assert!(
        metrics.contains(r#"epazote_failures_total{service_name="alerting_service"} 0"#),
        "a check that answered and merely failed its expectation is not a scan error, \
         however its fallback turned out; metrics:\n{metrics}"
    );

    assert!(
        metrics.contains(r#"epazote_status{service_name="alerting_service"} 0"#),
        "the service must still be reported DOWN; metrics:\n{metrics}"
    );

    assert!(
        logs.contains("Fallback for service 'alerting_service' did not complete"),
        "the fallback's failure must still reach the operator at the default \
         verbosity, named as a fallback; logs:\n{logs}"
    );

    assert!(
        !logs.contains("Error scanning service 'alerting_service'"),
        "the fallback's failure must not be reported as a scan error; logs:\n{logs}"
    );
}

/// The other direction of the same contract: a scan that genuinely could not be
/// completed *must* increment `epazote_failures_total`, and must say why.
///
/// Every other assertion on this counter in the suite pins it at `0` - seeding,
/// the no-scan-error regressions, and the test above. Nothing pinned it going
/// up, so deleting the single `.inc()` in the scan loop left the whole suite
/// green and the metric silently dead. The increment lives only in `run_service`
/// (`run.rs:154-167`), which no unit test drives, so this has to be the binary.
///
/// Paired with `test_failing_fallback_is_not_a_scan_error_and_is_still_reported`,
/// the two pin both directions: a fallback's failure never moves it, a real scan
/// error always does.
#[tokio::test]
async fn test_a_real_scan_error_increments_the_failure_counter() {
    // The peer accepts and drops the request, so the scan cannot be completed.
    let reset_service_url = resetting_endpoint_url("/health");

    let config_content = format!(
        r"
services:
  unreachable_service:
    url: {reset_service_url}
    every: 1s
    timeout: 1s
    expect:
      status: 200
"
    );

    let config_file = NamedTempFile::new().expect("Failed to create config file");
    std::fs::write(config_file.path(), config_content).expect("Failed to write config");

    let metrics_port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };

    let mut cmd = Command::cargo_bin("epazote").expect("Failed to find binary");

    let mut child = cmd
        .arg("-c")
        .arg(config_file.path())
        .arg("-p")
        .arg(metrics_port.to_string())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("Failed to start epazote");

    let client = Client::new();
    let metrics_url = format!("http://localhost:{metrics_port}/metrics");

    let mut counted = false;
    for _ in 0..15 {
        sleep(Duration::from_secs(1)).await;

        if let Ok(response) = client.get(&metrics_url).send().await
            && response.status().is_success()
        {
            let text = response.text().await.unwrap_or_default();

            // Seeded at 0, so anything above it is a real increment.
            if text.lines().any(|line| {
                line.starts_with(r#"epazote_failures_total{service_name="unreachable_service"}"#)
                    && !line.ends_with(" 0")
            }) {
                counted = true;
                break;
            }
        }
    }

    stop_epazote(&mut child).await;
    let output = child
        .wait_with_output()
        .expect("Failed to collect epazote output");
    let logs = String::from_utf8_lossy(&output.stdout);

    assert!(
        counted,
        "a request that could not be made is exactly what this counter counts; logs:\n{logs}"
    );

    assert!(
        logs.contains("Error scanning service 'unreachable_service'"),
        "the scan must also name the request it could not make; logs:\n{logs}"
    );
}

/// Regression, composed through the scan loop: a refunded skip must not flicker
/// `epazote_fallback_exhausted` to `1`.
///
/// The gauge is computed in `run_service` from `fallback_executions` against
/// `stop`, while the refund happens further down in
/// `execute_fallbacks_tracking_stop`. Both halves are tested on their own; their
/// composition was not. If the refund ever stopped landing before the gauge is
/// read, a service would be published as permanently abandoned on the strength
/// of attempts that restarted nothing - which is the failure the refund exists
/// to prevent, wearing the badge of the metric added to make it visible.
///
/// `threshold` staggers the two services rather than racing them: the holder
/// acts on its first failed check and keeps the lock, so by the time the queued
/// service's third failure arrives the group is certainly contended.
#[tokio::test]
async fn test_a_refunded_skip_does_not_exhaust_the_stop_budget() {
    let group_pid_file = NamedTempFile::new().expect("Failed to create process-group file");
    let group_pid_path = group_pid_file.path().to_owned();
    std::fs::remove_file(&group_pid_path).expect("Failed to remove process-group file");

    let config_content = format!(
        r#"
services:
  lock_holder:
    test: "exit 1"
    every: 1s
    expect:
      status: 0
      if_not:
        group: integration-refund
        threshold: 1
        timeout: 2m
        cmd: "echo $$ > {group_pid}; sleep 60"

  queued_service:
    test: "exit 1"
    every: 1s
    expect:
      status: 0
      if_not:
        group: integration-refund
        threshold: 3
        stop: 1
        timeout: 1s
        cmd: "true"
"#,
        group_pid = group_pid_path.display()
    );

    let config_file = NamedTempFile::new().expect("Failed to create config file");
    std::fs::write(config_file.path(), config_content).expect("Failed to write config");

    let metrics_port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };

    let mut cmd = Command::cargo_bin("epazote").expect("Failed to find binary");

    let mut child = cmd
        .arg("-c")
        .arg(config_file.path())
        .arg("-p")
        .arg(metrics_port.to_string())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("Failed to start epazote");

    let client = Client::new();
    let metrics_url = format!("http://localhost:{metrics_port}/metrics");

    let mut metrics = String::new();
    for _ in 0..20 {
        sleep(Duration::from_secs(1)).await;

        if let Ok(response) = client.get(&metrics_url).send().await
            && response.status().is_success()
        {
            let text = response.text().await.unwrap_or_default();

            // One skip is enough to know contention has started; the point of
            // the test is what happens on the ones after it.
            if metric_value(&text, SKIPPED) >= 1.0 {
                metrics = text;
                break;
            }
        }
    }

    // Let several more failed checks go by. With `stop: 1` and no refund, the
    // first skip alone would spend the budget and every later attempt would be
    // refused outright - so a rising skip count is the observable proof that the
    // refund reached the gauge.
    if !metrics.is_empty() {
        sleep(Duration::from_secs(4)).await;

        if let Ok(response) = client.get(&metrics_url).send().await
            && response.status().is_success()
        {
            metrics = response.text().await.unwrap_or_default();
        }
    }

    stop_epazote(&mut child).await;
    let output = child
        .wait_with_output()
        .expect("Failed to collect epazote output");
    let logs = String::from_utf8_lossy(&output.stdout);

    #[cfg(unix)]
    assert_recorded_process_group_stopped(&group_pid_path).await;

    assert!(
        !metrics.is_empty(),
        "the queued service was never held back, so nothing was exercised; logs:\n{logs}"
    );

    let skipped = metric_value(&metrics, SKIPPED);

    assert!(
        skipped >= 2.0,
        "with 'stop: 1', a skip that was not refunded would spend the budget and every \
         later attempt would be refused, capping this at 1; got {skipped}, metrics:\n{metrics}"
    );

    assert!(
        metrics.contains(r#"epazote_fallback_exhausted{service_name="queued_service"} 0"#),
        "every attempt was refunded, so the budget is unspent and the service must not be \
         published as abandoned; metrics:\n{metrics}"
    );

    assert!(
        metrics.contains(
            r#"epazote_fallback_executions_total{outcome="success",service_name="queued_service"} 0"#
        ),
        "the queued command never ran, so nothing may be recorded as a success; \
         metrics:\n{metrics}"
    );
}

const SKIPPED: &str =
    r#"epazote_fallback_executions_total{outcome="skipped",service_name="queued_service"}"#;
const DOOMED_STREAK: &str = r#"epazote_consecutive_failures{service_name="doomed_service"}"#;
const ALERT_FAILURE: &str =
    r#"epazote_fallback_executions_total{outcome="failure",service_name="alerting_service"}"#;

/// The value of the sample whose name and labels are `prefix`, or `0` when the
/// series is absent.
fn metric_value(metrics: &str, prefix: &str) -> f64 {
    metrics
        .lines()
        .find_map(|line| line.strip_prefix(prefix))
        .and_then(|rest| rest.trim().parse().ok())
        .unwrap_or(0.0)
}

/// Regression: recovery that is failing must be visible at the *default*
/// verbosity used by a packaged install unless the operator overrides it.
///
/// `telemetry.rs` defaults to `Level::ERROR` and raises only
/// `epazote::cli::config` to `warn`, so every `warn!` under `cli::actions` is
/// dropped. This exact config - a service that is down, whose restart script
/// exits non-zero every time, until its `stop` budget is spent and it is
/// abandoned for the rest of the outage - produced *zero bytes* of log output.
/// The metrics recorded all of it; the log said nothing.
///
/// Asserted through the binary because the filter is what is being tested, and
/// only the binary installs it.
#[tokio::test]
async fn test_failing_recovery_is_visible_at_the_default_verbosity() {
    let mut alert_server = mockito::Server::new_async().await;
    let _refused_alert = alert_server
        .mock("GET", "/alert")
        .with_status(503)
        .create_async()
        .await;

    let config_content = format!(
        r#"
services:
  doomed_service:
    test: "exit 1"
    every: 1s
    expect:
      status: 0
      if_not:
        threshold: 1
        stop: 2
        timeout: 5s
        cmd: "exit 7"

  refused_alert:
    test: "exit 1"
    every: 1s
    expect:
      status: 0
      if_not:
        threshold: 1
        stop: 1
        timeout: 5s
        http: {alert_url}/alert
"#,
        alert_url = alert_server.url()
    );

    let config_file = NamedTempFile::new().expect("Failed to create config file");
    std::fs::write(config_file.path(), config_content).expect("Failed to write config");

    let metrics_port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };

    let mut cmd = Command::cargo_bin("epazote").expect("Failed to find binary");

    // No `-v`, deliberately: that is the whole point.
    let mut child = cmd
        .arg("-c")
        .arg(config_file.path())
        .arg("-p")
        .arg(metrics_port.to_string())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("Failed to start epazote");

    let client = Client::new();
    let metrics_url = format!("http://localhost:{metrics_port}/metrics");

    // Wait until the budget is spent, so both events have had to happen.
    let mut metrics = String::new();
    for _ in 0..15 {
        sleep(Duration::from_secs(1)).await;

        if let Ok(response) = client.get(&metrics_url).send().await
            && response.status().is_success()
        {
            let text = response.text().await.unwrap_or_default();

            if text.contains(r#"epazote_fallback_exhausted{service_name="doomed_service"} 1"#) {
                metrics = text;
                break;
            }
        }
    }

    let streak_at_exhaustion = metric_value(&metrics, DOOMED_STREAK);

    // Let several more checks run past the point of exhaustion, so the
    // once-only assertion below is read after the scans that used to repeat it.
    sleep(Duration::from_secs(3)).await;

    let streak_after = match client.get(&metrics_url).send().await {
        Ok(response) if response.status().is_success() => {
            metric_value(&response.text().await.unwrap_or_default(), DOOMED_STREAK)
        }
        _ => 0.0,
    };

    stop_epazote(&mut child).await;
    let output = child
        .wait_with_output()
        .expect("Failed to collect epazote output");
    let logs = String::from_utf8_lossy(&output.stdout);

    assert!(
        !metrics.is_empty(),
        "the stop budget was never spent, so nothing was exercised; logs:\n{logs}"
    );

    // Without this, a scan loop that stalled after exhaustion would satisfy the
    // once-only assertion below for entirely the wrong reason.
    assert!(
        streak_after > streak_at_exhaustion,
        "checks must have gone on failing past exhaustion for the silence to mean \
         anything; streak went {streak_at_exhaustion} -> {streak_after}, logs:\n{logs}"
    );

    assert_recovery_failures_are_visible(&logs);
}

/// The log assertions of
/// [`test_failing_recovery_is_visible_at_the_default_verbosity`], split out to
/// keep that test within the crate's function-length lint.
fn assert_recovery_failures_are_visible(logs: &str) {
    assert!(
        logs.contains("Fallback command for doomed_service ran but exited with code 7"),
        "a restart script that keeps failing must reach the operator without '-v'; \
         logs:\n{logs}"
    );

    assert!(
        logs.contains("Fallback HTTP request for refused_alert was answered with status code 503"),
        "an alert endpoint that refuses the request must reach the operator without '-v'; \
         logs:\n{logs}"
    );

    assert!(
        logs.contains("Service 'doomed_service' reached stop limit"),
        "a service epazote has given up on must say so without '-v': the status gauge \
         reads the same either way; logs:\n{logs}"
    );

    assert_eq!(
        logs.matches("Service 'doomed_service' reached stop limit")
            .count(),
        1,
        "stop exhaustion is a state transition and must be logged once per outage; logs:\n{logs}"
    );
}

#[tokio::test]
async fn test_epazote_if_not_threshold_integration() {
    let mut server = mockito::Server::new_async().await;
    let mock_url = server.url();

    let _m = server
        .mock("GET", "/fail-threshold")
        .with_status(500)
        .create_async()
        .await;

    let marker_file = tempfile::NamedTempFile::new().expect("Failed to create marker file");
    let marker_path = marker_file.path().to_owned();
    std::fs::remove_file(&marker_path).expect("Failed to remove initial marker file");

    let config_content = format!(
        r"
services:
  fail_service:
    url: {mock_url}/fail-threshold
    every: 1s
    expect:
      status: 200
      if_not:
        threshold: 3
        stop: 1
        cmd: touch {}
",
        marker_path.to_str().expect("Invalid marker path")
    );

    let config_file = NamedTempFile::new().expect("Failed to create config file");
    std::fs::write(config_file.path(), config_content).expect("Failed to write config");

    let metrics_port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };

    let mut cmd = Command::cargo_bin("epazote").expect("Failed to find binary");

    let mut child = cmd
        .arg("-c")
        .arg(config_file.path())
        .arg("-p")
        .arg(metrics_port.to_string())
        .spawn()
        .expect("Failed to start epazote");

    sleep(Duration::from_secs(2)).await;
    assert!(
        !marker_path.exists(),
        "Fallback command executed before threshold was reached"
    );

    let mut success = false;
    for _ in 0..5 {
        sleep(Duration::from_secs(1)).await;
        if marker_path.exists() {
            success = true;
            break;
        }
    }

    stop_epazote(&mut child).await;
    let _ = child.wait();

    assert!(
        success,
        "Fallback command was not executed after threshold was reached"
    );
}

/// The example config ships as the first thing a new user copies, and nothing
/// checked that epazote still accepts it. It had drifted: a placeholder `cmd:`
/// with no value parsed as *no command*, so the example demonstrated a service
/// that looks like it configures recovery and never repairs anything. Parsing
/// it here keeps the documented example and the parser from diverging again.
#[test]
fn test_shipped_example_config_parses() {
    let example = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("epazote.yml");

    epazote::cli::config::Config::new(example)
        .expect("the shipped epazote.yml must parse with the current config rules");
}

#[test]
fn test_systemd_unit_does_not_override_default_verbosity() {
    let unit =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("contrib/systemd/epazote.service");
    let unit = std::fs::read_to_string(unit).expect("Failed to read packaged systemd unit");

    assert!(
        !unit.lines().any(|line| line
            .trim_start()
            .starts_with("Environment=\"EPAZOTE_VERBOSE=")),
        "the unit must leave verbosity to epazote's ERROR default and the optional environment file"
    );
}
