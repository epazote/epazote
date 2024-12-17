use anyhow::{anyhow, Result};
use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::get, Router};
use prometheus::{histogram_opts, opts, HistogramVec, IntCounterVec, IntGaugeVec, Registry};
use std::sync::Arc;
use tokio::net::TcpListener;
use tracing::{debug, error};

// Metrics struct to hold our Prometheus metrics
pub struct ServiceMetrics {
    registry: Arc<Registry>,
    pub service_status: IntGaugeVec,           // Current state
    pub service_failures_total: IntCounterVec, // Cumulative failures
    pub service_response_time: HistogramVec,
}

impl ServiceMetrics {
    pub fn new() -> Result<Self> {
        let registry = Arc::new(Registry::new());

        let service_status = IntGaugeVec::new(
            opts!("service_status", "Service status (1 = OK, 0 = FAIL)"),
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
