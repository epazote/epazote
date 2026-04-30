use crate::cli::{
    actions::{
        Action, FallbackContext, FallbackServiceType, FallbackState,
        client::build_client,
        execute_command, execute_fallback_command, execute_fallback_http, get_fallback_state,
        metrics::{ServiceMetrics, metrics_server},
        request::{build_http_request, handle_http_response},
        reset_fallback_state, should_continue_fallback,
        ssl::{SslCheckCache, check_ssl_certificate, new_ssl_check_cache},
    },
    config::{Config, ServiceDetails},
};
use anyhow::{Result, anyhow};
use reqwest::Client;
use rustls::crypto::CryptoProvider;
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::{
    sync::Mutex,
    time::{Instant, MissedTickBehavior, interval},
};
use tracing::{debug, error, info, instrument};

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
    CryptoProvider::install_default(rustls::crypto::ring::default_provider())
        .map_err(|e| anyhow!("Failed to install default crypto provider: {e:?}"))?;

    let Action::Run { config, port } = action;

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
    let metrics_server_handle = tokio::spawn(async move {
        if let Err(e) = metrics_server(service_metrics, port).await {
            error!("Metrics server error: {}", e);
        }
    });

    info!("Epazote 🌿 is running");

    // Wait for all tasks to complete
    tokio::select! {
        (result, _, _) = futures::future::select_all(service_handles) => {
            match result {
                Ok(()) => error!("A service monitoring task completed unexpectedly"),
                Err(e) => error!("A service monitoring task panicked: {}", e),
            }
        },
        _ = metrics_server_handle => {
            error!("Metrics server stopped unexpectedly");
        }
    }

    Ok(())
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

            if url.starts_with("https://") {
                check_ssl_certificate(&url, service_name, metrics, ssl_cache).await?;
            }

            debug!("HTTP request: {:?}", request);

            // Make the request
            let response = match client.execute(request).await {
                Ok(response) => response,
                Err(error) => {
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

                        if let Some(cmd) = &action.cmd {
                            let exit_code = execute_fallback_command(cmd, &context).await?;
                            info!(
                                "Executed fallback command for {} with exit code {}",
                                service_name, exit_code
                            );
                        }

                        if let Some(http) = &action.http {
                            let status = execute_fallback_http(http).await?;
                            info!(
                                "Executed fallback HTTP request for {} with status code {}",
                                service_name, status
                            );
                        }
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

            let exit_status = execute_command(command).await.unwrap_or(1);
            let expected_status = expected_command_status(service_details)?;

            if exit_status == expected_status {
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
                    actual_status: Some(exit_status),
                    error: "command_failed",
                    failure_count: state.consecutive_failures,
                    threshold: action.threshold.unwrap_or(1),
                    url: None,
                    test: Some(command),
                };

                if let Some(cmd) = &action.cmd {
                    let exit_code = execute_fallback_command(cmd, &context).await?;
                    info!(
                        "Executed fallback command for {} with exit code {}",
                        service_name, exit_code
                    );
                }

                if let Some(http) = &action.http {
                    let status = execute_fallback_http(http).await?;
                    info!(
                        "Executed fallback HTTP request for {} with status code {}",
                        service_name, status
                    );
                }
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
    async fn test_scan_service_command_stop_does_not_reset_after_success() {
        let mut service_details = mock_service_details(Some("exit 1"), 0, Some("echo 'Fallback'"));
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
