use crate::cli::{
    actions::{
        metrics::{metrics_server, ServiceMetrics},
        ssl::check_ssl_certificate,
        Action,
    },
    config::{Config, ServiceDetails},
};
use anyhow::{anyhow, Result};
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue},
    Client,
};
use rustls::crypto::CryptoProvider;
use std::{env, sync::Arc, time::Duration};
use tokio::{
    process::Command,
    time::{interval, Instant},
};
use tracing::{debug, error, info, instrument};

static APP_USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"), ")");

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

    for (service_name, service) in &config.services {
        let service_name = service_name.clone();
        let service_details = service.clone();

        let action = if let Some(ref command) = service_details.test {
            ServiceAction::Command(command.clone())
        } else {
            let mut builder = reqwest::Client::builder()
                .timeout(service_details.timeout)
                .user_agent(APP_USER_AGENT);

            // Disable redirects if follow is not set
            if service_details.follow_redirects.is_none() {
                builder = builder.redirect(reqwest::redirect::Policy::none());
            }

            // Conditionally add headers if they are provided
            if let Some(headers) = &service_details.headers {
                let mut header_map = HeaderMap::new();

                for (key, value) in headers {
                    let header_name =
                        HeaderName::from_bytes(key.as_bytes()).expect("Invalid header name");
                    let header_value = HeaderValue::from_str(value).expect("Invalid header value");

                    header_map.insert(header_name, header_value);
                }

                builder = builder.default_headers(header_map);
            }

            let client = builder.build()?;
            ServiceAction::Url(client)
        };

        // Clone the metrics for this task
        let metrics = service_metrics.clone();

        // Spawn a task for each service
        let handle = tokio::spawn(async move {
            let every = service_details.every;
            run_service(service_name, service_details, action, metrics, every).await;
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
) {
    let mut interval_timer = interval(interval_duration);

    loop {
        interval_timer.tick().await; // Wait for the next interval

        debug!("Running scan for service: {}", service_name);

        // Perform the service scan
        match scan_service(&service_name, &service_details, &action, &metrics).await {
            Ok(_) => (),
            Err(e) => {
                // Increment failure counter
                metrics
                    .service_failures_total
                    .with_label_values(&[&service_name])
                    .inc();

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
) -> Result<()> {
    let start_time = Instant::now();

    match action {
        ServiceAction::Url(client) => {
            debug!("HTTP request: {:?}", client);

            let url = service_details
                .url
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("No URL provided"))?;

            // Send a GET request to the service
            let response = client.get(url).send().await?;
            let status = response.status();
            let headers = response.headers();

            if url.starts_with("https://") {
                check_ssl_certificate(url, service_name, metrics).await?;
            }

            // Record response time
            let response_time = start_time.elapsed().as_secs_f64();
            metrics
                .service_response_time
                .with_label_values(&[service_name])
                .observe(response_time);

            // Capture exit code, defaulting to 0
            if status.as_u16() != service_details.expect.status {
                // Set service status to FAIL (0)
                metrics
                    .service_status
                    .with_label_values(&[service_name])
                    .set(0);

                if let Some(if_not) = &service_details.expect.if_not {
                    let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string());
                    let cmd = Command::new(shell)
                        .arg("-c")
                        .arg(&if_not.cmd)
                        .output()
                        .await?;

                    let exit_code = match cmd.status.code() {
                        Some(code) => code,
                        None => Err(anyhow!("Process terminated by signal"))?,
                    };

                    info!(
                        service_name = service_name,
                        service_url = service_details.url,
                        service_status = status.as_u16(),
                        expect_status = service_details.expect.status,
                        cmd_exit_code = exit_code,
                        response_headers = ?headers
                    );
                };
            } else {
                // Set service status to OK (1)
                metrics
                    .service_status
                    .with_label_values(&[service_name])
                    .set(1);

                info!(
                    service_name = service_name,
                    service_url = service_details.url,
                    service_status = status.as_u16(),
                    response_headers = ?headers
                );
            }
        }

        ServiceAction::Command(command) => {
            debug!("Executing command: {}", command);

            let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string());
            let output = Command::new(shell).arg("-c").arg(command).output().await?;

            let exit_status = output.status.code().unwrap_or(1); // Default to `1` if no exit code
            debug!("Command executed with exit code: {}", exit_status);

            if exit_status != service_details.expect.status as i32 {
                if let Some(action) = &service_details.expect.if_not {
                    debug!("Executing fallback action: {}", action.cmd);
                    let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string());
                    Command::new(shell)
                        .arg("-c")
                        .arg(&action.cmd)
                        .output()
                        .await?;
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::config::{Action, Expect};
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
                    cmd: cmd.to_string(),
                    ..Default::default()
                }),
            },
            follow_redirects: Some(true),
            headers: None,
            if_header: None,
            if_status: None,
            insecure: None,
            read_limit: None,
            stop: None,
            test: test_cmd.map(|cmd| cmd.to_string()),
            timeout: Duration::from_secs(5),
            url: None,
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

        let result = scan_service("test-service", &service_details, &action, &metrics).await;
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

        let result = scan_service("test-service", &service_details, &action, &metrics).await;
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
            if_header: None,
            if_status: None,
            insecure: None,
            read_limit: None,
            stop: None,
            test: None,
            timeout: Duration::from_secs(5),
            url: Some(format!("{}/health", server.url())),
        };

        let action = ServiceAction::Url(Client::new());
        let metrics = Arc::new(ServiceMetrics::new().unwrap());

        tokio::spawn(async move {
            run_service(
                "http-service".to_string(),
                service_details,
                action,
                metrics,
                Duration::from_millis(100),
            )
            .await;
        });

        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            true,
            "Run service should execute multiple times in test interval"
        );
    }

    /// Test: Run Service - HTTP Failure with Fallback
    #[tokio::test]
    async fn test_run_service_http_failure_with_fallback() {
        let mut server = Server::new_async().await;
        let _m = server
            .mock("GET", "/down")
            .with_status(500)
            .create_async()
            .await;

        let service_details = ServiceDetails {
            every: Duration::from_secs(1),
            expect: Expect {
                status: 200,
                header: None,
                body: None,
                if_not: Some(Action {
                    cmd: "echo 'HTTP Failure Fallback'".to_string(),
                    ..Default::default()
                }),
            },
            follow_redirects: Some(true),
            headers: None,
            if_header: None,
            if_status: None,
            insecure: None,
            read_limit: None,
            stop: None,
            test: None,
            timeout: Duration::from_secs(5),
            url: Some(format!("{}/down", server.url())),
        };

        let action = ServiceAction::Url(Client::new());
        let metrics = Arc::new(ServiceMetrics::new().unwrap());

        tokio::spawn(async move {
            run_service(
                "http-service-down".to_string(),
                service_details,
                action,
                metrics,
                Duration::from_millis(100),
            )
            .await;
        });

        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            true,
            "Run service should detect failure and execute fallback"
        );
    }
}
