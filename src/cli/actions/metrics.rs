use anyhow::{Result, anyhow};
use axum::{Router, extract::State, http::StatusCode, response::IntoResponse, routing::get};
use prometheus::{HistogramVec, IntCounterVec, IntGaugeVec, Registry, histogram_opts, opts};
use std::sync::Arc;
use tokio::net::TcpListener;
use tracing::{debug, error};

// Metrics struct to hold our Prometheus metrics
#[derive(Debug)]
pub struct ServiceMetrics {
    registry: Arc<Registry>,
    pub epazote_status: IntGaugeVec,           // Current state
    pub epazote_failures_total: IntCounterVec, // Cumulative failures
    pub epazote_response_time: HistogramVec,
    pub epazote_ssl_cert_expiry_seconds: IntGaugeVec,
}

impl ServiceMetrics {
    pub fn new() -> Result<Self> {
        let registry = Arc::new(Registry::new());

        let epazote_status = IntGaugeVec::new(
            opts!("epazote_status", "Service status (1 = OK, 0 = FAIL)"),
            &["service_name"],
        )?;

        let epazote_failures_total = IntCounterVec::new(
            opts!("epazote_failures_total", "Total number of service failures"),
            &["service_name"],
        )?;

        let epazote_response_time = HistogramVec::new(
            histogram_opts!(
                "epazote_response_time_seconds",
                "Service response time in seconds"
            ),
            &["service_name"],
        )?;

        let epazote_ssl_cert_expiry_seconds = IntGaugeVec::new(
            opts!(
                "epazote_ssl_cert_expiry_seconds",
                "Number of seconds until SSL certificate expiration"
            ),
            &["service_name"],
        )?;

        // Register metrics with the registry
        registry.register(Box::new(epazote_status.clone()))?;
        registry.register(Box::new(epazote_failures_total.clone()))?;
        registry.register(Box::new(epazote_response_time.clone()))?;
        registry.register(Box::new(epazote_ssl_cert_expiry_seconds.clone()))?;

        Ok(Self {
            registry,
            epazote_status,
            epazote_failures_total,
            epazote_response_time,
            epazote_ssl_cert_expiry_seconds,
        })
    }
}

pub async fn metrics_server(metrics: Arc<ServiceMetrics>, port: u16) -> Result<()> {
    let app = Router::new()
        .route("/metrics", get(metrics_handler))
        .with_state(metrics);

    let listener = match TcpListener::bind(format!("::0:{port}")).await {
        Ok(listener) => listener,
        Err(_) => TcpListener::bind(format!("0.0.0.0:{port}")).await?,
    };

    debug!("Metrics server listening on port {}", port);

    axum::serve(listener, app.into_make_service())
        .await
        .map_err(|e| anyhow!("Server error: {}", e))
}

pub async fn metrics_handler(State(metrics): State<Arc<ServiceMetrics>>) -> impl IntoResponse {
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
            debug!("Metrics encoded successfully.");
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{
        actions::client::build_client,
        actions::request::{build_http_request, handle_http_response},
        config::Config,
    };
    use mockito::Server;
    use std::{collections::HashMap, io::Write, sync::Arc};
    use tokio::sync::Mutex;

    // Helper to create config from YAML
    fn create_config(yaml: &str) -> Config {
        let mut tmp_file = tempfile::NamedTempFile::new().unwrap();
        tmp_file.write_all(yaml.as_bytes()).unwrap();
        tmp_file.flush().unwrap();
        Config::new(tmp_file.path().to_path_buf()).unwrap()
    }

    #[tokio::test]
    async fn test_metrics() {
        // Start mock server
        let mut server = Server::new_async().await;
        let mock_url = server.url();

        let yaml = format!(
            r#"
---
services:
  test:
    url: {}/test
    every: 30s
    expect:
      status: 200
    "#,
            mock_url
        );

        let config = create_config(&yaml);
        let service = config.services.get("test").unwrap();

        let _ = env_logger::try_init();
        let mock = server
            .mock("GET", "/test")
            .match_header(
                "User-Agent",
                mockito::Matcher::Regex("epazote.*".to_string()),
            )
            .with_status(200)
            .create_async()
            .await;

        let (builder, _client_config) = build_client(service).unwrap();
        let client = builder.build().unwrap();
        let request = build_http_request(&client, service).unwrap();
        let response = client.execute(request.build().unwrap()).await.unwrap();
        let counters: Arc<Mutex<HashMap<String, usize>>> = Arc::new(Mutex::new(HashMap::new()));
        let metrics = Arc::new(ServiceMetrics::new().unwrap());

        // Fetch initial values
        let initial_status = metrics
            .epazote_status
            .get_metric_with_label_values(&["test"])
            .map(|m| m.get())
            .unwrap_or(0);

        let initial_failures = metrics
            .epazote_failures_total
            .get_metric_with_label_values(&["test"])
            .map(|m| m.get())
            .unwrap_or(0);

        let rs = handle_http_response("test", &service, response, &metrics, counters.clone()).await;

        assert!(rs.is_ok());

        // Fetch updated values
        let updated_status = metrics
            .epazote_status
            .get_metric_with_label_values(&["test"])
            .map(|m| m.get())
            .unwrap_or(0);

        let updated_failures = metrics
            .epazote_failures_total
            .get_metric_with_label_values(&["test"])
            .map(|m| m.get())
            .unwrap_or(0);

        assert_ne!(
            initial_status, updated_status,
            "Service status should change after a successful request"
        );

        assert_eq!(
            updated_status, 1,
            "Service status should be 1 after a successful request"
        );
        assert_eq!(
            updated_failures, initial_failures,
            "Failures should not increase after a successful request"
        );

        mock.remove();

        let _mock = server
            .mock("GET", "/test")
            .with_status(500)
            .create_async()
            .await;

        let request = build_http_request(&client, service).unwrap();
        let response = client.execute(request.build().unwrap()).await.unwrap();

        let rs = handle_http_response("test", &service, response, &metrics, counters)
            .await
            .unwrap();
        // assert rs is false
        assert!(!rs);

        // Fetch updated values
        let updated_status = metrics
            .epazote_status
            .get_metric_with_label_values(&["test"])
            .map(|m| m.get())
            .unwrap_or(0);

        assert_eq!(
            updated_status, 0,
            "Service status should be 0 after a failed request"
        );
    }
}
