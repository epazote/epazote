use crate::cli::{
    actions::Action,
    config::{Config, ServiceDetails},
    globals::GlobalArgs,
};
use anyhow::{anyhow, Result};
use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::get, Router};
use prometheus::{histogram_opts, opts, HistogramVec, IntCounterVec, IntGaugeVec, Registry};
use reqwest::Client;
use std::{env, sync::Arc};
use tokio::{
    net::TcpListener,
    time::{interval, Instant},
};
use tracing::{debug, error, info, instrument};

// Metrics struct to hold our Prometheus metrics
pub struct ServiceMetrics {
    registry: Arc<Registry>,
    service_status: IntGaugeVec,           // Current state
    service_failures_total: IntCounterVec, // Cumulative failures
    service_response_time: HistogramVec,
}

impl ServiceMetrics {
    fn new() -> Result<Self> {
        let registry = Arc::new(Registry::new());

        let service_status = IntGaugeVec::new(
            opts!("service_status", "Service status (0 = OK, 1 = FAIL)"),
            &["service_name"],
        )?;

        let service_failures_total = IntCounterVec::new(
            opts!("service_failures_total", "Total number of service failures"),
            &["service_name"],
        )?;

        let service_response_time = HistogramVec::new(
            histogram_opts!(
                "service_response_time_seconds",
                "Service response time in seconds"
            ),
            &["service_name"],
        )?;

        // Register metrics with the registry
        registry.register(Box::new(service_status.clone()))?;
        registry.register(Box::new(service_failures_total.clone()))?;
        registry.register(Box::new(service_response_time.clone()))?;

        Ok(Self {
            registry,
            service_status,
            service_failures_total,
            service_response_time,
        })
    }
}

static APP_USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"), ")");

/// Handle the create action
#[instrument(skip(action))]
pub async fn handle(action: Action, _globals: GlobalArgs) -> Result<()> {
    let Action::Run { config } = action;

    let config_path = config;

    let config = Config::new(config_path)?;

    // Create service metrics
    let service_metrics = Arc::new(ServiceMetrics::new()?);

    let mut service_handles = Vec::new();

    for (service_name, service) in &config.services {
        let service_name = service_name.clone();
        let service_details = service.clone();

        let client = reqwest::Client::builder()
            .user_agent(APP_USER_AGENT)
            .build()?;

        // Clone the metrics for this task
        let metrics = service_metrics.clone();

        // Spawn a task for each service
        let handle = tokio::spawn(async move {
            run_service(service_name, service_details, client, metrics).await;
        });

        service_handles.push(handle);
    }

    // Spawn metrics server
    let metrics_server_handle = tokio::spawn(async move {
        if let Err(e) = metrics_server(service_metrics).await {
            error!("Metrics server error: {}", e);
        }
    });

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

pub async fn metrics_server(metrics: Arc<ServiceMetrics>) -> Result<()> {
    let port = 8080;

    let app = Router::new()
        .route("/metrics", get(metrics_handler))
        .with_state(metrics);

    let listener = match TcpListener::bind(format!("::0:{port}")).await {
        Ok(listener) => listener,
        Err(_) => TcpListener::bind(format!("0.0.0.0:{port}")).await?,
    };

    info!("Metrics server listening on port {}", port);

    axum::serve(listener, app.into_make_service())
        .await
        .map_err(|e| anyhow!("Server error: {}", e))
}

pub async fn metrics_handler(State(metrics): State<Arc<ServiceMetrics>>) -> impl IntoResponse {
    info!("Handling metrics request");

    let encoder = prometheus::TextEncoder::new();
    let metric_families = metrics.registry.gather();

    if metric_families.is_empty() {
        error!("No metrics collected in the registry.");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "No metrics collected in the registry",
        )
            .into_response();
    }

    let mut metrics_str = String::new();

    match encoder.encode_utf8(&metric_families, &mut metrics_str) {
        Ok(_) => {
            info!("Metrics encoded successfully.");
            (StatusCode::OK, metrics_str).into_response()
        }
        Err(e) => {
            error!("Failed to encode metrics: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to encode metrics",
            )
                .into_response()
        }
    }
}

/// Runs the task for a single service
async fn run_service(
    service_name: String,
    service_details: ServiceDetails,
    client: Client,
    metrics: Arc<ServiceMetrics>,
) {
    let mut interval_timer = interval(service_details.every);

    loop {
        interval_timer.tick().await; // Wait for the next interval

        debug!("Running scan for service: {}", service_name);

        // Perform the service scan
        match scan_service(&service_name, &service_details, &client, &metrics).await {
            Ok(_) => (),
            Err(e) => error!("Error scanning service '{}': {}", service_name, e),
        }
    }
}

/// Simulates scanning a service (e.g., sending an HTTP request)
async fn scan_service(
    service_name: &str,
    service_details: &ServiceDetails,
    client: &Client,
    metrics: &ServiceMetrics,
) -> Result<()> {
    let start_time = Instant::now();

    // Send a GET request to the service
    let response = client.get(&service_details.url).send().await?;
    let status = response.status();
    let headers = response.headers();

    // Record response time
    let response_time = start_time.elapsed().as_secs_f64();
    metrics
        .service_response_time
        .with_label_values(&[service_name])
        .observe(response_time);

    // Capture exit code, defaulting to 0
    if status.as_u16() != service_details.expect.status {
        // Set service status to FAIL (1)
        metrics
            .service_status
            .with_label_values(&[service_name])
            .set(1);

        // Increment failure counter
        metrics
            .service_failures_total
            .with_label_values(&[service_name])
            .inc();

        if let Some(if_not) = &service_details.expect.if_not {
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string());
            let cmd = tokio::process::Command::new(shell)
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
        // Set service status to OK (0)
        metrics
            .service_status
            .with_label_values(&[service_name])
            .set(0);

        info!(
            service_name = service_name,
            service_url = service_details.url,
            service_status = status.as_u16(),
            response_headers = ?headers
        );
    }

    Ok(())
}
