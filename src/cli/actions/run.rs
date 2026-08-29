use crate::cli::{
    actions::{
        Action, FallbackContext, FallbackServiceType, FallbackState,
        client::build_client,
        execute_command, execute_fallbacks_tracking_stop, get_fallback_state,
        metrics::{ServiceMetrics, metrics_server},
        request::{build_http_request, handle_http_response},
        reset_fallback_state, should_continue_fallback,
        ssl::{SslCheckCache, check_ssl_certificate, new_ssl_check_cache},
    },
    config::{Config, ServiceDetails},
};
use anyhow::{Context, Result, anyhow};
use reqwest::Client;
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::{
    sync::Mutex,
    time::{Instant, MissedTickBehavior, interval},
};
use tracing::{debug, error, info, instrument, warn};

enum ServiceAction {
    Url(Client),
    Command(String),
}

fn expected_command_status(service_details: &ServiceDetails) -> Result<i32> {
    service_details
        .expect
        .expected_status_i32()
        .ok_or_else(|| anyhow!("Command checks require expect.status"))
}

/// Handle the create action
///
/// # Errors
///
/// Returns an error if the configuration is invalid or the metrics server fails to start.
#[instrument(skip(action))]
pub async fn handle(action: Action) -> Result<()> {
    // rustls requires a cryptographic provider
    let _ =
        rustls::crypto::CryptoProvider::install_default(rustls::crypto::ring::default_provider());

    let Action::Run { config, bind, port } = action;

    let config_path = config;

    let config = Config::new(config_path)?;

    // Create service metrics
    let service_metrics = Arc::new(ServiceMetrics::new()?);
    let ssl_check_cache = new_ssl_check_cache();

    let mut service_handles = Vec::new();

    for (service_name, service) in &config.services {
        let service_counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let service_name = service_name.clone();
        let service_details = service.clone();
        let counters = service_counters;
        let ssl_cache = ssl_check_cache.clone();

        let action = if let Some(ref command) = service_details.test {
            ServiceAction::Command(command.clone())
        } else {
            let (builder, _client_config) = build_client(&service_details)?;
            let client = builder.build()?;

            ServiceAction::Url(client)
        };

        // Clone the metrics for this task
        let metrics = service_metrics.clone();

        // Spawn a task for each service
        let handle = tokio::spawn(async move {
            let every = service_details.every;
            run_service(
                service_name,
                service_details,
                action,
                metrics,
                every,
                counters,
                ssl_cache,
            )
            .await;
        });

        service_handles.push(handle);
    }

    // Spawn metrics server
    let metrics_server_handle = tokio::spawn(metrics_server(service_metrics, bind, port));

    info!("Epazote 🌿 is running");

    // Wait for all tasks to complete
    tokio::select! {
        (result, _, _) = futures::future::select_all(service_handles) => {
            return match result {
                Ok(()) => Err(anyhow!("A service monitoring task completed unexpectedly")),
                Err(e) => Err(anyhow!("A service monitoring task panicked: {e}")),
            };
        },
        result = metrics_server_handle => {
            return match result {
                Ok(Ok(())) => Err(anyhow!("Metrics server stopped unexpectedly")),
                Ok(Err(error)) => Err(error).context("Metrics server error"),
                Err(error) => Err(anyhow!("Metrics server task panicked: {error}")),
            };
        }
    }
}

/// Runs the task for a single service
async fn run_service(
    service_name: String,
    service_details: ServiceDetails,
    action: ServiceAction,
    metrics: Arc<ServiceMetrics>,
    interval_duration: Duration,
    counters: Arc<Mutex<HashMap<String, FallbackState>>>,
    ssl_cache: SslCheckCache,
) {
    let mut interval_timer = interval(interval_duration);
    interval_timer.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        interval_timer.tick().await; // Wait for the next interval

        debug!("Running scan for service: {}", service_name);

        // Perform the service scan
        match scan_service(
            &service_name,
            &service_details,
            &action,
            &metrics,
            counters.clone(),
            &ssl_cache,
        )
        .await
        {
            Ok(()) => (),
            Err(e) => {
                // Increment failure counter
                metrics
                    .epazote_failures_total
                    .with_label_values(&[&service_name])
                    .inc();

                metrics
                    .epazote_status
                    .with_label_values(&[&service_name])
                    .set(0);

                error!("Error scanning service '{}': {}", &service_name, e);
            }
        }
    }
}

/// Records certificate expiry for HTTPS services.
///
/// A failed certificate check must not abort the scan: returning early here
/// skips the HTTP request, so the configured `if_not` fallback never runs for
/// an unreachable HTTPS service - exactly when recovery is needed. Health is
/// decided by the HTTP expectations instead.
async fn check_certificate_expiry(
    url: &str,
    service_name: &str,
    service_details: &ServiceDetails,
    metrics: &ServiceMetrics,
    ssl_cache: &SslCheckCache,
) {
    if !url.starts_with("https://") {
        return;
    }

    if let Err(error) = check_ssl_certificate(
        url,
        service_name,
        metrics,
        ssl_cache,
        service_details.timeout,
    )
    .await
    {
        warn!("SSL certificate check failed for '{service_name}': {error}");
    }
}

/// `scan_service` performs the actual scan of the service
async fn scan_service(
    service_name: &str,
    service_details: &ServiceDetails,
    action: &ServiceAction,
    metrics: &ServiceMetrics,
    counters: Arc<Mutex<HashMap<String, FallbackState>>>,
    ssl_cache: &SslCheckCache,
) -> Result<()> {
    let start_time = Instant::now();

    match action {
        ServiceAction::Url(client) => {
            let request_builder = build_http_request(client, service_details)?;

            let request = request_builder.build()?;

            let url = request.url().to_string();

            check_certificate_expiry(&url, service_name, service_details, metrics, ssl_cache).await;

            debug!("HTTP request: {:?}", request);

            // Make the request
            let response = match client.execute(request).await {
                Ok(response) => response,
                Err(error) => {
                    // Mark the service down before the fallback runs, not
                    // after. Fallback commands are serialized process-wide, so
                    // this scan can sit queued behind another service's
                    // restart; reporting the failure only once the fallback
                    // returns would leave the gauge showing the last-known
                    // state for as long as that queue takes.
                    metrics
                        .epazote_status
                        .with_label_values(&[service_name])
                        .set(0);

                    if let Some(action) = &service_details.expect.if_not
                        && should_continue_fallback(service_name, &counters, action).await
                    {
                        let state = get_fallback_state(service_name, &counters)
                            .await
                            .unwrap_or_default();
                        let context = FallbackContext {
                            service_name,
                            service_type: FallbackServiceType::Http,
                            expected_status: service_details.expect.expected_status_i32(),
                            actual_status: None,
                            error: "request_error",
                            failure_count: state.consecutive_failures,
                            threshold: action.threshold.unwrap_or(1),
                            url: Some(&url),
                            test: None,
                        };

                        execute_fallbacks_tracking_stop(action, &context, service_name, &counters)
                            .await?;
                    }

                    return Err(error.into());
                }
            };

            // Record response time
            let response_time = start_time.elapsed().as_secs_f64();
            metrics
                .epazote_response_time
                .with_label_values(&[service_name])
                .observe(response_time);

            // Handle the response
            handle_http_response(service_name, service_details, response, metrics, counters)
                .await?;
        }

        ServiceAction::Command(command) => {
            debug!("Executing command: {}", command);

            let execution = execute_command(command, service_details.timeout).await;

            // Record how long the check took, mirroring the HTTP path so
            // command services appear in the same dashboards.
            let response_time = start_time.elapsed().as_secs_f64();
            metrics
                .epazote_response_time
                .with_label_values(&[service_name])
                .observe(response_time);

            let expected_status = expected_command_status(service_details)?;

            // A command that could not run (spawn failure, timeout) never
            // produced an exit status, so it must not be compared against the
            // expected one. Synthesising exit code 1 here reported a hung
            // command as healthy whenever `expect.status` was itself 1.
            let (is_match, actual_status, error) = match execution {
                Ok(code) => (code == expected_status, Some(code), "command_failed"),
                Err(e) => {
                    warn!("Failed to execute command for {service_name}: {e}");
                    (false, None, "command_error")
                }
            };

            // Command checks previously recorded no metrics at all, so a
            // failing service was invisible to Prometheus. Set the gauge here
            // the way the HTTP path does rather than relying on the scan
            // returning an error, which only happens when a fallback fails.
            metrics
                .epazote_status
                .with_label_values(&[service_name])
                .set(i64::from(is_match));

            if is_match {
                reset_fallback_state(service_name, &counters).await;
            } else if let Some(action) = &service_details.expect.if_not
                && should_continue_fallback(service_name, &counters, action).await
            {
                let state = get_fallback_state(service_name, &counters)
                    .await
                    .unwrap_or_default();
                let context = FallbackContext {
                    service_name,
                    service_type: FallbackServiceType::Command,
                    expected_status: Some(expected_status),
                    actual_status,
                    error,
                    failure_count: state.consecutive_failures,
                    threshold: action.threshold.unwrap_or(1),
                    url: None,
                    test: Some(command),
                };

                execute_fallbacks_tracking_stop(action, &context, service_name, &counters).await?;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::cli::config::{Action, Expect, HttpMethod};
    use mockito::Server;
    use reqwest::StatusCode;
    use std::{fs, net::TcpListener, os::unix::fs::PermissionsExt, path::PathBuf, sync::Arc};
    use tokio::process::Command;
    use tokio::runtime::Runtime;
    use tokio::time::Duration;

    /// An endpoint that accepts connections and never answers, so a fallback
    /// HTTP action stays in flight until its own timeout. Used to observe what
    /// the metrics say *while* a fallback is still running - without taking
    /// the process-wide fallback command lock, which would couple the test to
    /// every other test that runs a fallback command.
    fn hanging_endpoint() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("Failed to bind test listener");
        let addr = listener.local_addr().expect("Failed to get local addr");

        std::thread::spawn(move || {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept() {
                held.push(socket);
            }
        });

        format!("http://{addr}/alert")
    }

    /// A fallback that stays in flight long enough to observe the metrics
    /// mid-flight, without serializing against other tests.
    fn slow_http_fallback() -> Action {
        Action {
            cmd: None,
            http: Some(hanging_endpoint()),
            stop: None,
            threshold: Some(1),
            timeout: Some(Duration::from_millis(1500)),
        }
    }

    /// Helper Function: Create Mock `ServiceDetails`
    fn mock_service_details(
        test_cmd: Option<&str>,
        expect_status: u16,
        if_not: Option<&str>,
    ) -> ServiceDetails {
        ServiceDetails {
            every: Duration::from_secs(1),
            expect: Expect {
                status: Some(expect_status),
                header: None,
                body: None,
                body_not: None,
                json: None,
                if_not: if_not.map(|cmd| Action {
                    cmd: Some(cmd.to_string()),
                    ..Default::default()
                }),
            },
            follow_redirects: Some(true),
            headers: None,
            max_bytes: None,
            test: test_cmd.map(std::string::ToString::to_string),
            timeout: Duration::from_secs(5),
            url: None,
            method: HttpMethod::Get,
            body: None,
        }
    }

    /// Helper Function: Create Mock Action
    fn mock_action(test_cmd: &str) -> ServiceAction {
        ServiceAction::Command(test_cmd.to_string())
    }

    fn create_env_capture_script(env_vars: &[&str]) -> (tempfile::TempDir, String, PathBuf) {
        let tempdir = tempfile::Builder::new()
            .prefix("epazote-run-env-")
            .tempdir_in(".")
            .expect("Failed to create temp dir");
        let script_path = tempdir.path().join("capture.sh");
        let output_path = tempdir.path().join("output.txt");
        let body = env_vars
            .iter()
            .map(|key| format!("printenv {key}"))
            .collect::<Vec<_>>()
            .join("\n");

        fs::write(
            &script_path,
            format!("#!/bin/sh\n{{\n{body}\n}} > {}\n", output_path.display()),
        )
        .expect("Failed to write capture script");

        let mut permissions = fs::metadata(&script_path)
            .expect("Failed to stat script")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).expect("Failed to chmod script");

        (
            tempdir,
            script_path
                .to_str()
                .expect("Invalid script path")
                .to_string(),
            output_path,
        )
    }

    fn occupy_metrics_port() -> (u16, TcpListener, Option<TcpListener>) {
        let listener = TcpListener::bind("[::]:0")
            .or_else(|_| TcpListener::bind("0.0.0.0:0"))
            .expect("Failed to bind test listener");
        let port = listener
            .local_addr()
            .expect("Failed to get local addr")
            .port();
        let ipv4_listener = TcpListener::bind(("0.0.0.0", port)).ok();

        (port, listener, ipv4_listener)
    }

    #[tokio::test]
    async fn test_handle_returns_error_when_metrics_port_is_in_use() {
        let (port, _listener, _ipv4_listener) = occupy_metrics_port();
        let config_file = tempfile::NamedTempFile::new().expect("Failed to create config file");
        fs::write(
            config_file.path(),
            r#"
services:
  command_service:
    test: "true"
    every: 1s
    expect:
      status: 0
"#,
        )
        .expect("Failed to write config file");

        let result = handle(crate::cli::actions::Action::Run {
            config: config_file.path().to_path_buf(),
            bind: "[::]".to_string(),
            port,
        })
        .await;

        let error = result.expect_err("handle should fail");
        assert!(
            format!("{error:#}").contains("Metrics server"),
            "unexpected error: {error:#}"
        );
    }

    /// Test: Verify Shell Command Exit Codes
    async fn run_command(cmd: &str) -> i32 {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string());
        let output = Command::new(shell)
            .arg("-c")
            .arg(cmd)
            .output()
            .await
            .expect("Failed to execute command");

        output.status.code().unwrap_or(1) // Default to 1 if no exit code
    }

    #[test]
    // this test is only for the test run_command function, not the actual code
    fn test_command_exit_status() {
        let rt = Runtime::new().expect("Failed to create runtime");

        let exit_code_0 = rt.block_on(run_command("exit 0"));
        assert_eq!(exit_code_0, 0, "Command `exit 0` should return exit code 0");

        let exit_code_1 = rt.block_on(run_command("exit 1"));
        assert_eq!(exit_code_1, 1, "Command `exit 1` should return exit code 1");
    }

    /// Test: Successful HTTP Service with Expected Status
    #[tokio::test]
    async fn test_http_service_expect_status() {
        let mut server = Server::new_async().await;
        let _m = server
            .mock("GET", "/test")
            .with_status(200)
            .create_async()
            .await;

        let url = format!("{}/test", server.url());
        let client = Client::new();
        let response = client
            .get(&url)
            .send()
            .await
            .expect("Failed to send request");
        let status = response.status();

        assert_eq!(status, StatusCode::OK, "Expected status 200 OK");
    }

    /// Test: Scan Service Command - Success
    #[tokio::test]
    async fn test_scan_service_command_success() {
        let service_details = mock_service_details(Some("exit 0"), 0, None);
        let action = mock_action("exit 0");
        let metrics = Arc::new(ServiceMetrics::new().expect("Failed to create metrics"));
        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let ssl_cache = new_ssl_check_cache();

        let result = scan_service(
            "test-service",
            &service_details,
            &action,
            &metrics,
            counters,
            &ssl_cache,
        )
        .await;

        assert!(
            result.is_ok(),
            "Scan service should succeed for a successful command"
        );
    }

    #[tokio::test]
    async fn test_scan_service_command_if_not_cmd_sets_env_vars() {
        let (_tempdir, script_path, output_path) = create_env_capture_script(&[
            "EPAZOTE_SERVICE_NAME",
            "EPAZOTE_SERVICE_TYPE",
            "EPAZOTE_EXPECTED_STATUS",
            "EPAZOTE_ACTUAL_STATUS",
            "EPAZOTE_ERROR",
            "EPAZOTE_FAILURE_COUNT",
            "EPAZOTE_THRESHOLD",
            "EPAZOTE_TEST",
        ]);

        let mut service_details = mock_service_details(Some("exit 1"), 0, Some(&script_path));
        service_details
            .expect
            .if_not
            .as_mut()
            .expect("if_not should be present")
            .threshold = Some(2);

        let action = mock_action("exit 1");
        let metrics = Arc::new(ServiceMetrics::new().expect("Failed to create metrics"));
        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let ssl_cache = new_ssl_check_cache();

        for _ in 0..2 {
            let result = scan_service(
                "test-service",
                &service_details,
                &action,
                &metrics,
                Arc::clone(&counters),
                &ssl_cache,
            )
            .await;

            assert!(result.is_ok(), "Scan service should complete");
        }

        let output = fs::read_to_string(output_path).expect("Failed to read env capture");
        assert_eq!(
            output.lines().collect::<Vec<_>>(),
            vec![
                "test-service",
                "command",
                "0",
                "1",
                "command_failed",
                "2",
                "2",
                "exit 1",
            ]
        );
    }

    /// Test: Scan Service Command - Failure with Fallback
    #[tokio::test]
    async fn test_scan_service_command_failure_with_fallback() {
        let service_details = mock_service_details(Some("exit 1"), 0, Some("echo 'Fallback'"));
        let action = mock_action("exit 1");
        let metrics = Arc::new(ServiceMetrics::new().expect("Failed to create metrics"));
        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let ssl_cache = new_ssl_check_cache();

        let result = scan_service(
            "test-service",
            &service_details,
            &action,
            &metrics,
            Arc::clone(&counters),
            &ssl_cache,
        )
        .await;

        assert!(
            result.is_ok(),
            "Scan service should execute fallback for failed command"
        );

        let counters_locked = counters.lock().await;
        let count = counters_locked
            .get("test-service")
            .map_or(0, |state| state.fallback_executions);

        assert_eq!(count, 1, "Counter should have been incremented");
    }

    /// Test: Scan Service Command - Stops after 2 failures
    #[tokio::test]
    async fn test_scan_service_command_failure_with_stop_after_2_attempts() {
        let mut service_details = mock_service_details(Some("exit 1"), 0, Some("echo 'Fallback'"));
        let action = mock_action("exit 1");

        // Set stop condition to 2
        service_details
            .expect
            .if_not
            .as_mut()
            .expect("if_not should be present")
            .stop = Some(2);

        let metrics = Arc::new(ServiceMetrics::new().expect("Failed to create metrics"));
        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let ssl_cache = new_ssl_check_cache();

        // First attempt
        let result1 = scan_service(
            "test-service",
            &service_details,
            &action,
            &metrics,
            Arc::clone(&counters),
            &ssl_cache,
        )
        .await;

        assert!(result1.is_ok(), "First attempt should allow fallback");

        // Check counter after first attempt
        let count1 = {
            let counters_locked = counters.lock().await;
            counters_locked
                .get("test-service")
                .map_or(0, |state| state.fallback_executions)
        };
        assert_eq!(count1, 1, "Counter should be 1 after first attempt");

        // Second attempt
        let result2 = scan_service(
            "test-service",
            &service_details,
            &action,
            &metrics,
            Arc::clone(&counters),
            &ssl_cache,
        )
        .await;

        assert!(result2.is_ok(), "Second attempt should allow fallback");

        // Check counter after second attempt
        let count2 = {
            let counters_locked = counters.lock().await;
            counters_locked
                .get("test-service")
                .map_or(0, |state| state.fallback_executions)
        };
        assert_eq!(count2, 2, "Counter should be 2 after second attempt");

        // Third attempt (should NOT execute fallback)
        let result3 = scan_service(
            "test-service",
            &service_details,
            &action,
            &metrics,
            Arc::clone(&counters),
            &ssl_cache,
        )
        .await;

        assert!(
            result3.is_ok(),
            "Third attempt should skip fallback due to stop limit"
        );

        // Check counter after third attempt (should remain at 2)
        let count3 = {
            let counters_locked = counters.lock().await;
            counters_locked
                .get("test-service")
                .map_or(0, |state| state.fallback_executions)
        };
        assert_eq!(count3, 2, "Counter should remain at 2 after third attempt");
    }

    #[tokio::test]
    async fn test_scan_service_command_threshold_delays_fallback() {
        let mut service_details = mock_service_details(Some("exit 1"), 0, Some("echo 'Fallback'"));
        let action = mock_action("exit 1");

        service_details
            .expect
            .if_not
            .as_mut()
            .expect("if_not should be present")
            .threshold = Some(3);

        let metrics = Arc::new(ServiceMetrics::new().expect("Failed to create metrics"));
        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let ssl_cache = new_ssl_check_cache();

        for expected_executions in [0, 0, 1] {
            let result = scan_service(
                "test-service",
                &service_details,
                &action,
                &metrics,
                Arc::clone(&counters),
                &ssl_cache,
            )
            .await;

            assert!(result.is_ok(), "Scan service should complete");

            let counters_locked = counters.lock().await;
            let state = counters_locked
                .get("test-service")
                .expect("State not found");
            assert_eq!(state.fallback_executions, expected_executions);
            drop(counters_locked);
        }
    }

    #[tokio::test]
    async fn test_scan_service_command_success_resets_threshold_counter() {
        let mut service_details = mock_service_details(Some("exit 1"), 0, Some("echo 'Fallback'"));
        service_details
            .expect
            .if_not
            .as_mut()
            .expect("if_not should be present")
            .threshold = Some(2);

        let failing_action = mock_action("exit 1");
        let success_action = mock_action("exit 0");
        let metrics = Arc::new(ServiceMetrics::new().expect("Failed to create metrics"));
        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let ssl_cache = new_ssl_check_cache();

        let first_failure = scan_service(
            "test-service",
            &service_details,
            &failing_action,
            &metrics,
            Arc::clone(&counters),
            &ssl_cache,
        )
        .await;
        assert!(first_failure.is_ok());

        let success = scan_service(
            "test-service",
            &service_details,
            &success_action,
            &metrics,
            Arc::clone(&counters),
            &ssl_cache,
        )
        .await;
        assert!(success.is_ok());

        let second_failure = scan_service(
            "test-service",
            &service_details,
            &failing_action,
            &metrics,
            Arc::clone(&counters),
            &ssl_cache,
        )
        .await;
        assert!(second_failure.is_ok());

        let counters_locked = counters.lock().await;
        let state = counters_locked
            .get("test-service")
            .expect("State not found");
        assert_eq!(state.consecutive_failures, 1);
        assert_eq!(state.fallback_executions, 0);
    }

    /// Regression: a command that never ran must not be compared against the
    /// expected exit status. Synthesising exit code 1 reported a hung command
    /// as healthy whenever `expect.status` was itself 1 - no fallback, and a
    /// green status metric for a service that is not being checked at all.
    #[tokio::test]
    async fn test_scan_service_command_timeout_is_unhealthy_when_expecting_exit_1() {
        let (_tempdir, script_path, output_path) =
            create_env_capture_script(&["EPAZOTE_SERVICE_NAME", "EPAZOTE_ERROR"]);

        let mut service_details =
            mock_service_details(Some("sleep 30"), 1, Some(script_path.as_str()));
        service_details.timeout = Duration::from_millis(200);

        let metrics = Arc::new(ServiceMetrics::new().expect("Failed to create metrics"));
        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let ssl_cache = new_ssl_check_cache();

        scan_service(
            "expects-exit-1",
            &service_details,
            &mock_action("sleep 30"),
            &metrics,
            Arc::clone(&counters),
            &ssl_cache,
        )
        .await
        .expect("a failed check is not a scan error");

        assert_eq!(
            metrics
                .epazote_status
                .with_label_values(&["expects-exit-1"])
                .get(),
            0,
            "a command that could not run must never be reported healthy"
        );
        assert!(
            output_path.exists(),
            "a command that could not run must trigger the fallback"
        );
    }

    /// A genuine exit code 1 must still satisfy `expect.status: 1`, so the fix
    /// above does not make every command check fail.
    #[tokio::test]
    async fn test_scan_service_command_genuine_exit_1_matches_expected() {
        let metrics = Arc::new(ServiceMetrics::new().expect("Failed to create metrics"));
        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let ssl_cache = new_ssl_check_cache();

        let service_details = mock_service_details(Some("exit 1"), 1, None);

        scan_service(
            "genuine-exit-1",
            &service_details,
            &mock_action("exit 1"),
            &metrics,
            Arc::clone(&counters),
            &ssl_cache,
        )
        .await
        .expect("scan should succeed");

        assert_eq!(
            metrics
                .epazote_status
                .with_label_values(&["genuine-exit-1"])
                .get(),
            1,
            "an actual exit code 1 must still match expect.status: 1"
        );
    }

    /// Regression: command checks must report their health. They previously
    /// recorded no metrics at all, so a failing `test` service was invisible
    /// to Prometheus - `epazote_status == 0` could never fire for it.
    #[tokio::test]
    async fn test_scan_service_command_records_status_metric() {
        let metrics = Arc::new(ServiceMetrics::new().expect("Failed to create metrics"));
        let ssl_cache = new_ssl_check_cache();
        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));

        // A passing check reports 1.
        let healthy = mock_service_details(Some("true"), 0, None);
        scan_service(
            "healthy-cmd",
            &healthy,
            &mock_action("true"),
            &metrics,
            Arc::clone(&counters),
            &ssl_cache,
        )
        .await
        .expect("healthy scan should succeed");

        assert_eq!(
            metrics
                .epazote_status
                .with_label_values(&["healthy-cmd"])
                .get(),
            1,
            "a passing command check must report status 1"
        );

        // A failing check reports 0, without needing the scan to error.
        let failing = mock_service_details(Some("false"), 0, None);
        scan_service(
            "failing-cmd",
            &failing,
            &mock_action("false"),
            &metrics,
            Arc::clone(&counters),
            &ssl_cache,
        )
        .await
        .expect("a failed check is not a scan error");

        assert_eq!(
            metrics
                .epazote_status
                .with_label_values(&["failing-cmd"])
                .get(),
            0,
            "a failing command check must report status 0"
        );
    }

    /// Command checks must also record how long they took, so they appear in
    /// the same response-time panels as HTTP services.
    #[tokio::test]
    async fn test_scan_service_command_records_response_time() {
        let metrics = Arc::new(ServiceMetrics::new().expect("Failed to create metrics"));
        let ssl_cache = new_ssl_check_cache();
        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let service_details = mock_service_details(Some("true"), 0, None);
        scan_service(
            "timed-cmd",
            &service_details,
            &mock_action("true"),
            &metrics,
            Arc::clone(&counters),
            &ssl_cache,
        )
        .await
        .expect("scan should succeed");

        assert_eq!(
            metrics
                .epazote_response_time
                .with_label_values(&["timed-cmd"])
                .get_sample_count(),
            1,
            "a command check must record a response-time observation"
        );
    }

    /// A failing check must not inflate `epazote_failures_total`: that counter
    /// tracks scan errors, and the HTTP path does not increment it either.
    #[tokio::test]
    async fn test_scan_service_command_failure_does_not_count_as_scan_error() {
        let metrics = Arc::new(ServiceMetrics::new().expect("Failed to create metrics"));
        let ssl_cache = new_ssl_check_cache();
        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let service_details = mock_service_details(Some("false"), 0, None);
        scan_service(
            "quiet-failure",
            &service_details,
            &mock_action("false"),
            &metrics,
            Arc::clone(&counters),
            &ssl_cache,
        )
        .await
        .expect("a failed check is not a scan error");

        assert_eq!(
            metrics
                .epazote_failures_total
                .with_label_values(&["quiet-failure"])
                .get(),
            0,
            "a failed check must not be counted as a scan error"
        );
    }

    /// Regression: a failed SSL certificate check must not abort the scan
    /// before the HTTP request is made. Previously the check was `?`-propagated,
    /// so an unreachable HTTPS service returned early and its configured
    /// `if_not` fallback never ran - the one case recovery exists for.
    #[tokio::test]
    async fn test_scan_service_https_unreachable_still_runs_fallback() {
        let (_tempdir, script_path, output_path) =
            create_env_capture_script(&["EPAZOTE_SERVICE_NAME", "EPAZOTE_ERROR"]);

        // A closed port, so both the SSL check and the HTTP request fail.
        let listener = TcpListener::bind("127.0.0.1:0").expect("Failed to bind test listener");
        let url = format!(
            "https://{}/health",
            listener.local_addr().expect("Failed to get local addr")
        );
        drop(listener);

        let service_details = ServiceDetails {
            every: Duration::from_secs(1),
            expect: Expect {
                status: Some(200),
                header: None,
                body: None,
                body_not: None,
                json: None,
                if_not: Some(Action {
                    cmd: Some(script_path),
                    http: None,
                    stop: None,
                    threshold: Some(1),
                    timeout: None,
                }),
            },
            follow_redirects: Some(true),
            headers: None,
            max_bytes: None,
            test: None,
            timeout: Duration::from_millis(500),
            url: Some(url),
            method: HttpMethod::Get,
            body: None,
        };

        let action = ServiceAction::Url(Client::new());
        let metrics = Arc::new(ServiceMetrics::new().expect("Failed to create metrics"));
        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let ssl_cache = new_ssl_check_cache();

        let result = scan_service(
            "https-unreachable-service",
            &service_details,
            &action,
            &metrics,
            Arc::clone(&counters),
            &ssl_cache,
        )
        .await;

        assert!(
            result.is_err(),
            "unreachable service should report an error"
        );
        assert!(
            output_path.exists(),
            "fallback command must run for an unreachable HTTPS service"
        );
    }

    /// Regression: a check command that never returns must not stall the
    /// service task. Without a timeout the scan blocked forever, silently
    /// stopping every future scan for that service.
    #[tokio::test]
    async fn test_scan_service_command_timeout_does_not_hang() {
        let (_tempdir, script_path, output_path) =
            create_env_capture_script(&["EPAZOTE_SERVICE_NAME"]);

        let mut service_details =
            mock_service_details(Some("sleep 30"), 0, Some(script_path.as_str()));
        service_details.timeout = Duration::from_millis(200);

        let action = mock_action("sleep 30");
        let metrics = Arc::new(ServiceMetrics::new().expect("Failed to create metrics"));
        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let ssl_cache = new_ssl_check_cache();

        let started = std::time::Instant::now();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            scan_service(
                "hanging-command-service",
                &service_details,
                &action,
                &metrics,
                Arc::clone(&counters),
                &ssl_cache,
            ),
        )
        .await
        .expect("scan_service must not hang past the service timeout");

        result.expect("a timed-out check is a failed check, not a scan error");

        assert!(
            started.elapsed() < Duration::from_secs(5),
            "scan should return near the configured timeout, took {:?}",
            started.elapsed()
        );
        assert!(
            output_path.exists(),
            "a command that exceeds the timeout must count as a failed check and run the fallback"
        );
    }

    /// Regression: a failed probe must be visible in Prometheus before the
    /// fallback runs. Fallback commands are serialized process-wide, so a scan
    /// can sit queued behind another service's restart - reporting the failure
    /// only after the fallback returns would leave the gauge stale for minutes.
    #[tokio::test]
    async fn test_scan_service_marks_service_down_before_running_fallback() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("Failed to bind test listener");
        let url = format!(
            "http://{}/health",
            listener.local_addr().expect("Failed to get local addr")
        );
        drop(listener);

        let service_details = ServiceDetails {
            every: Duration::from_secs(1),
            expect: Expect {
                status: Some(200),
                header: None,
                body: None,
                body_not: None,
                json: None,
                if_not: Some(slow_http_fallback()),
            },
            follow_redirects: Some(true),
            headers: None,
            max_bytes: None,
            test: None,
            timeout: Duration::from_millis(100),
            url: Some(url),
            method: HttpMethod::Get,
            body: None,
        };

        let action = ServiceAction::Url(Client::new());
        let metrics = Arc::new(ServiceMetrics::new().expect("Failed to create metrics"));
        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let ssl_cache = new_ssl_check_cache();

        // The service was healthy on the previous scan, so a gauge left
        // untouched still reads UP.
        metrics
            .epazote_status
            .with_label_values(&["down-service"])
            .set(1);

        let scan = scan_service(
            "down-service",
            &service_details,
            &action,
            &metrics,
            Arc::clone(&counters),
            &ssl_cache,
        );

        let status_during_fallback = async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            metrics
                .epazote_status
                .with_label_values(&["down-service"])
                .get()
        };

        let (result, status_during_fallback) = tokio::join!(scan, status_during_fallback);

        assert!(
            result.is_err(),
            "Request error should still return an error"
        );
        assert_eq!(
            status_during_fallback, 0,
            "the service must report DOWN while its fallback is still running, not only after"
        );
    }

    /// The same invariant on the command-check path: `epazote_status` must be
    /// written before the fallback, which can block for as long as the
    /// fallback timeout allows.
    #[tokio::test]
    async fn test_scan_service_command_check_marks_service_down_before_running_fallback() {
        // A check that exits 1 against an expected 0, with a fallback that
        // stays in flight while the gauge is inspected.
        let mut service_details = mock_service_details(Some("exit 1"), 0, None);
        service_details.expect.if_not = Some(slow_http_fallback());

        let action = ServiceAction::Command("exit 1".to_string());
        let metrics = Arc::new(ServiceMetrics::new().expect("Failed to create metrics"));
        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let ssl_cache = new_ssl_check_cache();

        metrics
            .epazote_status
            .with_label_values(&["cmd-service"])
            .set(1);

        let scan = scan_service(
            "cmd-service",
            &service_details,
            &action,
            &metrics,
            Arc::clone(&counters),
            &ssl_cache,
        );

        let status_during_fallback = async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            metrics
                .epazote_status
                .with_label_values(&["cmd-service"])
                .get()
        };

        let (result, status_during_fallback) = tokio::join!(scan, status_during_fallback);

        assert!(
            result.is_err(),
            "the unanswered fallback endpoint is reported as a scan error"
        );
        assert_eq!(
            status_during_fallback, 0,
            "the service must report DOWN while its fallback is still running, not only after"
        );
    }

    /// `epazote_failures_total` is owned by the scan loop, which increments it
    /// when a scan returns an error. `scan_service` must not also count the
    /// same failure - marking the service down early must not turn into a
    /// double increment.
    #[tokio::test]
    async fn test_scan_service_leaves_the_failure_counter_to_the_run_loop() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("Failed to bind test listener");
        let url = format!(
            "http://{}/health",
            listener.local_addr().expect("Failed to get local addr")
        );
        drop(listener);

        let mut service_details = mock_service_details(None, 200, None);
        service_details.url = Some(url);
        service_details.timeout = Duration::from_millis(100);

        let action = ServiceAction::Url(Client::new());
        let metrics = Arc::new(ServiceMetrics::new().expect("Failed to create metrics"));
        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let ssl_cache = new_ssl_check_cache();

        let result = scan_service(
            "uncounted-service",
            &service_details,
            &action,
            &metrics,
            Arc::clone(&counters),
            &ssl_cache,
        )
        .await;

        assert!(result.is_err(), "an unreachable service must error");
        assert_eq!(
            metrics
                .epazote_failures_total
                .with_label_values(&["uncounted-service"])
                .get(),
            0,
            "scan_service must leave the failure count to the scan loop, or the failure is counted twice"
        );
    }

    #[tokio::test]
    async fn test_scan_service_http_request_error_sets_env_vars() {
        let (_tempdir, script_path, output_path) = create_env_capture_script(&[
            "EPAZOTE_SERVICE_NAME",
            "EPAZOTE_SERVICE_TYPE",
            "EPAZOTE_EXPECTED_STATUS",
            "EPAZOTE_ERROR",
            "EPAZOTE_FAILURE_COUNT",
            "EPAZOTE_THRESHOLD",
            "EPAZOTE_URL",
        ]);

        let listener = TcpListener::bind("127.0.0.1:0").expect("Failed to bind test listener");
        let url = format!(
            "http://{}/health",
            listener.local_addr().expect("Failed to get local addr")
        );
        drop(listener);

        let service_details = ServiceDetails {
            every: Duration::from_secs(1),
            expect: Expect {
                status: Some(200),
                header: None,
                body: None,
                body_not: None,
                json: None,
                if_not: Some(Action {
                    cmd: Some(script_path),
                    http: None,
                    stop: None,
                    threshold: Some(1),
                    timeout: None,
                }),
            },
            follow_redirects: Some(true),
            headers: None,
            max_bytes: None,
            test: None,
            timeout: Duration::from_millis(100),
            url: Some(url.clone()),
            method: HttpMethod::Get,
            body: None,
        };

        let action = ServiceAction::Url(Client::new());
        let metrics = Arc::new(ServiceMetrics::new().expect("Failed to create metrics"));
        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let ssl_cache = new_ssl_check_cache();

        let result = scan_service(
            "http-error-service",
            &service_details,
            &action,
            &metrics,
            Arc::clone(&counters),
            &ssl_cache,
        )
        .await;

        assert!(
            result.is_err(),
            "Request error should still return an error"
        );

        let output = fs::read_to_string(output_path).expect("Failed to read env capture");
        assert_eq!(
            output.lines().collect::<Vec<_>>(),
            vec![
                "http-error-service",
                "http",
                "200",
                "request_error",
                "1",
                "1",
                &url,
            ]
        );
    }

    #[tokio::test]
    async fn test_scan_service_command_success_resets_stop_counter() {
        let tempdir = tempfile::Builder::new()
            .prefix("epazote-command-stop-reset-")
            .tempdir_in(".")
            .expect("Failed to create temp dir");
        let script_path = tempdir.path().join("capture.sh");
        let output_path = tempdir.path().join("output.txt");

        fs::write(
            &script_path,
            format!(
                "#!/bin/sh\nprintenv EPAZOTE_FAILURE_COUNT >> {}\n",
                output_path.display()
            ),
        )
        .expect("Failed to write capture script");

        let mut permissions = fs::metadata(&script_path)
            .expect("Failed to stat script")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).expect("Failed to chmod script");

        let mut service_details = mock_service_details(
            Some("exit 1"),
            0,
            Some(script_path.to_str().expect("Invalid script path")),
        );
        let if_not = service_details
            .expect
            .if_not
            .as_mut()
            .expect("if_not should be present");
        if_not.threshold = Some(1);
        if_not.stop = Some(1);

        let failing_action = mock_action("exit 1");
        let success_action = mock_action("exit 0");
        let metrics = Arc::new(ServiceMetrics::new().expect("Failed to create metrics"));
        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let ssl_cache = new_ssl_check_cache();

        let first_failure = scan_service(
            "test-service",
            &service_details,
            &failing_action,
            &metrics,
            Arc::clone(&counters),
            &ssl_cache,
        )
        .await;
        assert!(first_failure.is_ok());

        let output = fs::read_to_string(&output_path).expect("Failed to read env capture");
        assert_eq!(output.lines().collect::<Vec<_>>(), vec!["1"]);

        let success = scan_service(
            "test-service",
            &service_details,
            &success_action,
            &metrics,
            Arc::clone(&counters),
            &ssl_cache,
        )
        .await;
        assert!(success.is_ok());

        let second_failure = scan_service(
            "test-service",
            &service_details,
            &failing_action,
            &metrics,
            Arc::clone(&counters),
            &ssl_cache,
        )
        .await;
        assert!(second_failure.is_ok());

        let counters_locked = counters.lock().await;
        let state = counters_locked
            .get("test-service")
            .expect("State not found");
        assert_eq!(state.fallback_executions, 1);
        assert_eq!(state.consecutive_failures, 1);
        drop(counters_locked);

        let output = fs::read_to_string(output_path).expect("Failed to read env capture");
        assert_eq!(output.lines().collect::<Vec<_>>(), vec!["1", "1"]);
    }

    /// Test: Scan Service Command - Ensure counter can reach 1000 when no stop condition is set
    #[tokio::test]
    async fn test_scan_service_command_runs_1000_times_without_stop() {
        let mut service_details = mock_service_details(Some("exit 1"), 0, Some("echo 'Fallback'"));
        let action = mock_action("exit 1");

        // Ensure no stop limit is set
        service_details
            .expect
            .if_not
            .as_mut()
            .expect("if_not should be present")
            .stop = None;

        let metrics = Arc::new(ServiceMetrics::new().expect("Failed to create metrics"));
        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let ssl_cache = new_ssl_check_cache();

        // Run scan_service 100 times
        for _ in 0..100 {
            let _ = scan_service(
                "test-service",
                &service_details,
                &action,
                &metrics,
                Arc::clone(&counters),
                &ssl_cache,
            )
            .await;
        }

        // Check that counter reached 1000
        let final_count = {
            let counters_locked = counters.lock().await;
            counters_locked
                .get("test-service")
                .map_or(0, |state| state.fallback_executions)
        };

        assert_eq!(
            final_count, 100,
            "Counter should reach 100 when no stop is set"
        );
    }

    /// Test: Scan Service Command - Failure with Fallback and Stop
    #[tokio::test]
    async fn test_scan_service_command_failure_with_fallback_and_stop() {
        let service_details = mock_service_details(Some("exit 1"), 0, Some("echo 'Fallback'"));
        let action = mock_action("exit 1");
        let metrics = Arc::new(ServiceMetrics::new().expect("Failed to create metrics"));
        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let ssl_cache = new_ssl_check_cache();

        let result = scan_service(
            "test-service",
            &service_details,
            &action,
            &metrics,
            counters,
            &ssl_cache,
        )
        .await;
        assert!(
            result.is_ok(),
            "Scan service should execute fallback for failed command"
        );
    }

    /// Test: Run Service - URL Success
    #[tokio::test]
    async fn test_run_service_http_success() {
        let mut server = Server::new_async().await;
        let _m = server
            .mock("GET", "/health")
            .with_status(200)
            .create_async()
            .await;

        let service_details = ServiceDetails {
            every: Duration::from_secs(1),
            expect: Expect {
                status: Some(200),
                header: None,
                body: None,
                body_not: None,
                json: None,
                if_not: None,
            },
            follow_redirects: Some(true),
            headers: None,
            max_bytes: None,
            test: None,
            timeout: Duration::from_secs(5),
            url: Some(format!("{}/health", server.url())),
            method: HttpMethod::Get,
            body: None,
        };

        let action = ServiceAction::Url(Client::new());
        let metrics = Arc::new(ServiceMetrics::new().expect("Failed to create metrics"));
        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let ssl_cache = new_ssl_check_cache();

        tokio::spawn(async move {
            run_service(
                "http-service".to_string(),
                service_details,
                action,
                metrics,
                Duration::from_millis(100),
                counters,
                ssl_cache,
            )
            .await;
        });

        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
