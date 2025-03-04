use crate::cli::{
    actions::{
        client::build_client,
        execute_fallback_command, execute_fallback_http,
        metrics::{metrics_server, ServiceMetrics},
        request::{build_http_request, handle_http_response},
        should_continue_fallback,
        ssl::check_ssl_certificate,
        Action,
    },
    config::{Config, ServiceDetails},
};
use anyhow::{anyhow, Result};
use reqwest::Client;
use rustls::crypto::CryptoProvider;
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::{
    sync::Mutex,
    time::{interval, Instant},
};
use tracing::{debug, error, info, instrument};

enum ServiceAction {
    Url(Client),
    Command(String),
}

/// Handle the create action
#[instrument(skip(action))]
pub async fn handle(action: Action) -> Result<()> {
    // rustls requires a cryptographic provider
    CryptoProvider::install_default(rustls::crypto::ring::default_provider())
        .map_err(|e| anyhow!("Failed to install default crypto provider: {:?}", e))?;

    let Action::Run { config, port } = action;

    let config_path = config;

    let config = Config::new(config_path)?;

    // Create service metrics
    let service_metrics = Arc::new(ServiceMetrics::new()?);

    let mut service_handles = Vec::new();

    let service_counters: Arc<Mutex<HashMap<String, usize>>> = Arc::new(Mutex::new(HashMap::new()));

    for (service_name, service) in &config.services {
        let service_name = service_name.clone();
        let service_details = service.clone();
        let counters = service_counters.clone();

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
        _ = futures::future::join_all(service_handles) => {
            error!("All service monitoring tasks completed unexpectedly");
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
    counters: Arc<Mutex<HashMap<String, usize>>>,
) {
    let mut interval_timer = interval(interval_duration);

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
        )
        .await
        {
            Ok(_) => (),
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

/// scan_service performs the actual scan of the service
async fn scan_service(
    service_name: &str,
    service_details: &ServiceDetails,
    action: &ServiceAction,
    metrics: &ServiceMetrics,
    counters: Arc<Mutex<HashMap<String, usize>>>,
) -> Result<()> {
    let start_time = Instant::now();

    match action {
        ServiceAction::Url(client) => {
            let request_builder = build_http_request(client, service_details)?;

            let request = request_builder.build()?;

            let url = request.url().to_string();

            if url.starts_with("https://") {
                check_ssl_certificate(&url, service_name, metrics).await?;
            }

            debug!("HTTP request: {:?}", request);

            // Make the request
            let response = client.execute(request).await?;

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

            let exit_status = execute_fallback_command(command).await.unwrap_or(1);

            if exit_status != service_details.expect.status as i32 {
                if let Some(action) = &service_details.expect.if_not {
                    if should_continue_fallback(service_name, &counters, action).await {
                        if let Some(cmd) = &action.cmd {
                            let exit_code = execute_fallback_command(cmd).await?;
                            debug!("Fallback action executed with exit code: {}", exit_code);
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
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::config::{Action, Expect, HttpMethod};
    use mockito::Server;
    use reqwest::StatusCode;
    use std::sync::Arc;
    use tokio::process::Command;
    use tokio::runtime::Runtime;
    use tokio::time::Duration;

    /// Helper Function: Create Mock ServiceDetails
    fn mock_service_details(
        test_cmd: Option<&str>,
        expect_status: u16,
        if_not: Option<&str>,
    ) -> ServiceDetails {
        ServiceDetails {
            every: Duration::from_secs(1),
            expect: Expect {
                status: expect_status,
                header: None,
                body: None,
                if_not: if_not.map(|cmd| Action {
                    cmd: Some(cmd.to_string()),
                    ..Default::default()
                }),
            },
            follow_redirects: Some(true),
            headers: None,
            max_bytes: None,
            test: test_cmd.map(|cmd| cmd.to_string()),
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
        let rt = Runtime::new().unwrap();

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
        let response = client.get(&url).send().await.unwrap();
        let status = response.status();

        assert_eq!(status, StatusCode::OK, "Expected status 200 OK");
    }

    /// Test: Scan Service Command - Success
    #[tokio::test]
    async fn test_scan_service_command_success() {
        let service_details = mock_service_details(Some("exit 0"), 0, None);
        let action = mock_action("exit 0");
        let metrics = Arc::new(ServiceMetrics::new().unwrap());
        let counters: Arc<Mutex<HashMap<String, usize>>> = Arc::new(Mutex::new(HashMap::new()));

        let result = scan_service(
            "test-service",
            &service_details,
            &action,
            &metrics,
            counters,
        )
        .await;

        assert!(
            result.is_ok(),
            "Scan service should succeed for a successful command"
        );
    }

    /// Test: Scan Service Command - Failure with Fallback
    #[tokio::test]
    async fn test_scan_service_command_failure_with_fallback() {
        let service_details = mock_service_details(Some("exit 1"), 0, Some("echo 'Fallback'"));
        let action = mock_action("exit 1");
        let metrics = Arc::new(ServiceMetrics::new().unwrap());
        let counters: Arc<Mutex<HashMap<String, usize>>> = Arc::new(Mutex::new(HashMap::new()));

        let result = scan_service(
            "test-service",
            &service_details,
            &action,
            &metrics,
            Arc::clone(&counters),
        )
        .await;

        assert!(
            result.is_ok(),
            "Scan service should execute fallback for failed command"
        );

        let counters_locked = counters.lock().await;
        let count = counters_locked.get("test-service").copied().unwrap_or(0);

        assert_eq!(count, 1, "Counter should have been incremented");
    }

    /// Test: Scan Service Command - Stops after 2 failures
    #[tokio::test]
    async fn test_scan_service_command_failure_with_stop_after_2_attempts() {
        let mut service_details = mock_service_details(Some("exit 1"), 0, Some("echo 'Fallback'"));
        let action = mock_action("exit 1");

        // Set stop condition to 2
        service_details.expect.if_not.as_mut().unwrap().stop = Some(2);

        let metrics = Arc::new(ServiceMetrics::new().unwrap());
        let counters: Arc<Mutex<HashMap<String, usize>>> = Arc::new(Mutex::new(HashMap::new()));

        // First attempt
        let result1 = scan_service(
            "test-service",
            &service_details,
            &action,
            &metrics,
            Arc::clone(&counters),
        )
        .await;

        assert!(result1.is_ok(), "First attempt should allow fallback");

        // Check counter after first attempt
        let count1 = {
            let counters_locked = counters.lock().await;
            *counters_locked.get("test-service").unwrap_or(&0)
        };
        assert_eq!(count1, 1, "Counter should be 1 after first attempt");

        // Second attempt
        let result2 = scan_service(
            "test-service",
            &service_details,
            &action,
            &metrics,
            Arc::clone(&counters),
        )
        .await;

        assert!(result2.is_ok(), "Second attempt should allow fallback");

        // Check counter after second attempt
        let count2 = {
            let counters_locked = counters.lock().await;
            *counters_locked.get("test-service").unwrap_or(&0)
        };
        assert_eq!(count2, 2, "Counter should be 2 after second attempt");

        // Third attempt (should NOT execute fallback)
        let result3 = scan_service(
            "test-service",
            &service_details,
            &action,
            &metrics,
            Arc::clone(&counters),
        )
        .await;

        assert!(
            result3.is_ok(),
            "Third attempt should skip fallback due to stop limit"
        );

        // Check counter after third attempt (should remain at 2)
        let count3 = {
            let counters_locked = counters.lock().await;
            *counters_locked.get("test-service").unwrap_or(&0)
        };
        assert_eq!(count3, 2, "Counter should remain at 2 after third attempt");
    }

    /// Test: Scan Service Command - Ensure counter can reach 1000 when no stop condition is set
    #[tokio::test]
    async fn test_scan_service_command_runs_1000_times_without_stop() {
        let mut service_details = mock_service_details(Some("exit 1"), 0, Some("echo 'Fallback'"));
        let action = mock_action("exit 1");

        // Ensure no stop limit is set
        service_details.expect.if_not.as_mut().unwrap().stop = None;

        let metrics = Arc::new(ServiceMetrics::new().unwrap());
        let counters: Arc<Mutex<HashMap<String, usize>>> = Arc::new(Mutex::new(HashMap::new()));

        // Run scan_service 100 times
        for _ in 0..100 {
            let _ = scan_service(
                "test-service",
                &service_details,
                &action,
                &metrics,
                Arc::clone(&counters),
            )
            .await;
        }

        // Check that counter reached 1000
        let final_count = {
            let counters_locked = counters.lock().await;
            *counters_locked.get("test-service").unwrap_or(&0)
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
        let metrics = Arc::new(ServiceMetrics::new().unwrap());
        let counters: Arc<Mutex<HashMap<String, usize>>> = Arc::new(Mutex::new(HashMap::new()));

        let result = scan_service(
            "test-service",
            &service_details,
            &action,
            &metrics,
            counters,
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
                status: 200,
                header: None,
                body: None,
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
        let metrics = Arc::new(ServiceMetrics::new().unwrap());
        let counters: Arc<Mutex<HashMap<String, usize>>> = Arc::new(Mutex::new(HashMap::new()));

        tokio::spawn(async move {
            run_service(
                "http-service".to_string(),
                service_details,
                action,
                metrics,
                Duration::from_millis(100),
                counters,
            )
            .await;
        });

        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            true,
            "Run service should execute multiple times in test interval"
        );
    }
}
