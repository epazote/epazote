use crate::cli::commands::built_info;
use anyhow::{Result, anyhow};
use axum::{Router, extract::State, http::StatusCode, response::IntoResponse, routing::get};
use prometheus::{HistogramVec, IntCounterVec, IntGaugeVec, Registry, histogram_opts, opts};
use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::net::TcpListener;
use tracing::{debug, error};

/// Every action that ran reported success, and no grouped command was held
/// back.
pub const FALLBACK_SUCCESS: &str = "success";

/// At least one action reported failure or could not be started. A command
/// that never got its group lock did not itself fail, but a failed action
/// running alongside it takes precedence.
pub const FALLBACK_FAILURE: &str = "failure";

/// A grouped command never got its lock, and no action that did run failed.
///
/// An HTTP action may still have run successfully alongside the held-back
/// command. The label follows what happened to the command; the `stop` refund
/// is a separate decision and applies only when no HTTP action ran. Only
/// grouped commands can be skipped this way; an ungrouped command never queues.
pub const FALLBACK_SKIPPED: &str = "skipped";

const FALLBACK_OUTCOMES: [&str; 3] = [FALLBACK_SUCCESS, FALLBACK_FAILURE, FALLBACK_SKIPPED];

// Metrics struct to hold our Prometheus metrics
#[derive(Debug)]
pub struct ServiceMetrics {
    registry: Arc<Registry>,
    pub epazote_status: IntGaugeVec,           // Current state
    pub epazote_failures_total: IntCounterVec, // Cumulative scan errors
    pub epazote_response_time: HistogramVec,
    pub epazote_ssl_cert_expiry_seconds: IntGaugeVec,
    pub epazote_consecutive_failures: IntGaugeVec,
    pub epazote_fallback_executions_total: IntCounterVec,
    pub epazote_fallback_exhausted: IntGaugeVec,
    pub epazote_fallback_configured: IntGaugeVec,
    pub epazote_last_check_timestamp_seconds: IntGaugeVec,
    pub epazote_build_info: IntGaugeVec,
}

impl ServiceMetrics {
    /// Create a new `ServiceMetrics` instance.
    ///
    /// # Errors
    ///
    /// Returns an error if the metrics cannot be created or registered.
    pub fn new() -> Result<Self> {
        let registry = Arc::new(Registry::new());

        let epazote_status = IntGaugeVec::new(
            opts!("epazote_status", "Service status (1 = OK, 0 = FAIL)"),
            &["service_name"],
        )?;

        let epazote_failures_total = IntCounterVec::new(
            opts!(
                "epazote_failures_total",
                "Total number of scan errors: checks that could not be completed at all. A check that ran and failed its expectations sets epazote_status to 0 instead."
            ),
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

        let epazote_consecutive_failures = IntGaugeVec::new(
            opts!(
                "epazote_consecutive_failures",
                "Consecutive failed checks for the service, reset to 0 after a successful check"
            ),
            &["service_name"],
        )?;

        let epazote_fallback_executions_total = IntCounterVec::new(
            opts!(
                "epazote_fallback_executions_total",
                "Total number of if_not fallback attempts, by outcome"
            ),
            &["service_name", "outcome"],
        )?;

        let epazote_fallback_exhausted = IntGaugeVec::new(
            opts!(
                "epazote_fallback_exhausted",
                "1 when the service used up its 'stop' budget and will no longer run its configured fallback actions"
            ),
            &["service_name"],
        )?;

        let epazote_fallback_configured = IntGaugeVec::new(
            opts!(
                "epazote_fallback_configured",
                "1 when the service declares an if_not action that can actually run"
            ),
            &["service_name"],
        )?;

        let epazote_last_check_timestamp_seconds = IntGaugeVec::new(
            opts!(
                "epazote_last_check_timestamp_seconds",
                "Unix timestamp of the last completed check for the service"
            ),
            &["service_name"],
        )?;

        let epazote_build_info = IntGaugeVec::new(
            opts!(
                "epazote_build_info",
                "Build information for the running epazote binary (always 1)"
            ),
            &["version", "revision"],
        )?;

        // Register metrics with the registry
        registry.register(Box::new(epazote_status.clone()))?;
        registry.register(Box::new(epazote_failures_total.clone()))?;
        registry.register(Box::new(epazote_response_time.clone()))?;
        registry.register(Box::new(epazote_ssl_cert_expiry_seconds.clone()))?;
        registry.register(Box::new(epazote_consecutive_failures.clone()))?;
        registry.register(Box::new(epazote_fallback_executions_total.clone()))?;
        registry.register(Box::new(epazote_fallback_exhausted.clone()))?;
        registry.register(Box::new(epazote_fallback_configured.clone()))?;
        registry.register(Box::new(epazote_last_check_timestamp_seconds.clone()))?;
        registry.register(Box::new(epazote_build_info.clone()))?;

        let metrics = Self {
            registry,
            epazote_status,
            epazote_failures_total,
            epazote_response_time,
            epazote_ssl_cert_expiry_seconds,
            epazote_consecutive_failures,
            epazote_fallback_executions_total,
            epazote_fallback_exhausted,
            epazote_fallback_configured,
            epazote_last_check_timestamp_seconds,
            epazote_build_info,
        };

        metrics.set_build_info();

        Ok(metrics)
    }

    /// Publish the version and git revision of the running binary.
    ///
    /// Without this there is no way to tell from the metric store which hosts
    /// are running which build, so a partially-rolled-out fix looks identical
    /// to a fully-rolled-out one. The value is always 1; the information is
    /// carried by the labels, which is the usual convention for `*_build_info`.
    fn set_build_info(&self) {
        self.epazote_build_info
            .with_label_values(&[
                env!("CARGO_PKG_VERSION"),
                built_info::GIT_COMMIT_HASH.unwrap_or("unknown"),
            ])
            .set(1);
    }

    /// Pre-create the per-service metric children so they are exported from
    /// process start instead of only materialising on the first failure.
    ///
    /// `IntCounterVec` children are created lazily by `with_label_values`, so a
    /// service that has never failed exports no `epazote_failures_total` series
    /// at all - not even a zero. That makes "healthy, never failed" and
    /// "epazote is down / the service was renamed or removed from the config"
    /// indistinguishable: both render as an empty panel, and `increase()` over
    /// the missing series returns no data rather than `0`. Touching the counter
    /// here publishes it at `0` so the two cases can be told apart.
    ///
    /// The cost is bounded: one extra series per configured service.
    ///
    /// The fallback counters and gauges are seeded for the same reason: a
    /// service that has never needed its fallback must still report `0`
    /// attempts and `exhausted=0`, otherwise "never ran" reads as "not
    /// reporting". `epazote_last_check_timestamp_seconds` is deliberately left
    /// out - seeding it would claim a check happened at the Unix epoch and
    /// immediately trip any staleness alert, so its absence until the first
    /// real check is the honest representation.
    ///
    /// `epazote_status` is deliberately not pre-created: seeding it would have
    /// to pick either `0` (a false DOWN alert before the first check runs) or
    /// `1` (a false UP). It is set by the first real check instead. The
    /// response-time histogram is likewise left alone - it is by far the
    /// largest contributor to this exporter's series count.
    ///
    /// `fallback_configured` is what stops `epazote_fallback_exhausted = 0`
    /// from being read as reassurance. Seeded for every service, that `0` says
    /// "has not given up" even when there is nothing to give up on, so a
    /// service with no `if_not` is indistinguishable from one whose fallback is
    /// still available. The pair separates the two.
    pub fn init_service(&self, service_name: &str, fallback_configured: bool) {
        let _ = self
            .epazote_failures_total
            .get_metric_with_label_values(&[service_name]);

        for outcome in FALLBACK_OUTCOMES {
            let _ = self
                .epazote_fallback_executions_total
                .get_metric_with_label_values(&[service_name, outcome]);
        }

        self.epazote_consecutive_failures
            .with_label_values(&[service_name])
            .set(0);

        self.epazote_fallback_exhausted
            .with_label_values(&[service_name])
            .set(0);

        self.epazote_fallback_configured
            .with_label_values(&[service_name])
            .set(i64::from(fallback_configured));
    }

    /// Record that a check completed, whatever its result.
    ///
    /// This is what makes a stuck scheduler visible: `epazote_status` keeps
    /// reporting its last value forever if the service task stops ticking, so
    /// a frozen check is indistinguishable from a stable one without a
    /// timestamp to age out.
    pub fn record_check(&self, service_name: &str) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| {
                i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
            });

        self.epazote_last_check_timestamp_seconds
            .with_label_values(&[service_name])
            .set(now);
    }

    /// Publish the check timestamp and the service failure streak as soon as the
    /// check result is known.
    ///
    /// Recovery may wait for a group and then run for another full fallback
    /// timeout. Holding this update until recovery finishes leaves a known-down
    /// service reporting its previous streak for that entire period.
    ///
    /// This is the *only* writer of `epazote_consecutive_failures`. The gauge is
    /// what makes a service alertable *before* it crosses its `threshold`,
    /// rather than only after the fallback has already fired, so every path that
    /// completes a check calls this - including the ones that then return an
    /// error.
    pub fn record_check_state(&self, service_name: &str, consecutive_failures: usize) {
        self.record_check(service_name);
        self.epazote_consecutive_failures
            .with_label_values(&[service_name])
            .set(i64::try_from(consecutive_failures).unwrap_or(i64::MAX));
    }

    /// Publish whether the service has given up on recovering itself.
    ///
    /// This is the state that was previously invisible: once a service has used
    /// up its `stop` budget epazote stops attempting recovery, but it keeps
    /// reporting `epazote_status 0` and logs only a single error. From the
    /// metric store, "still failing and being retried" and "failing and
    /// permanently given up on" looked identical.
    ///
    /// Deliberately narrower than the streak it used to publish alongside:
    /// `consecutive_failures` is written by [`Self::record_check_state`] the
    /// moment the check result is known, and this runs only once the scan - and
    /// any recovery it triggered - has returned. Writing the streak from both
    /// would be two sources for one gauge, agreeing today and free to drift
    /// tomorrow.
    pub fn sync_fallback_exhausted(&self, service_name: &str, exhausted: bool) {
        self.epazote_fallback_exhausted
            .with_label_values(&[service_name])
            .set(i64::from(exhausted));
    }

    /// Count a fallback attempt under one of `FALLBACK_OUTCOMES`.
    ///
    /// Recovery actions are the whole point of the watchdog and were entirely
    /// absent from the metrics: whether a fallback ever fired, and whether it
    /// worked, could only be found by reading the command's log file on the
    /// host.
    pub fn record_fallback(&self, service_name: &str, outcome: &str) {
        self.epazote_fallback_executions_total
            .with_label_values(&[service_name, outcome])
            .inc();
    }
}

/// Bind the metrics listener for the given address and port.
///
/// When binding to the default dual-stack address `[::]` fails (for example on
/// systems where IPv6 is disabled), it falls back to the all-IPv4 address
/// `0.0.0.0`. An explicitly requested address is never silently changed: if it
/// cannot bind, the error is returned instead of falling back.
async fn bind_listener(bind: &str, port: u16) -> Result<TcpListener> {
    match TcpListener::bind(format!("{bind}:{port}")).await {
        Ok(listener) => Ok(listener),
        Err(_) if bind == "[::]" => Ok(TcpListener::bind(format!("0.0.0.0:{port}")).await?),
        Err(e) => Err(e.into()),
    }
}

/// Starts the metrics server.
///
/// # Errors
///
/// Returns an error if the server cannot bind to the address/port or encounters a runtime error.
pub async fn metrics_server(metrics: Arc<ServiceMetrics>, bind: String, port: u16) -> Result<()> {
    let app = Router::new()
        .route("/metrics", get(metrics_handler))
        .with_state(metrics);

    let listener = bind_listener(&bind, port).await?;

    match listener.local_addr() {
        Ok(addr) => debug!("Metrics server listening on {addr}"),
        Err(e) => debug!("Metrics server listening (failed to resolve local address: {e})"),
    }

    axum::serve(listener, app.into_make_service())
        .await
        .map_err(|e| anyhow!("Server error: {e}"))
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
        Ok(()) => {
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
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::cli::{
        actions::FallbackState,
        actions::client::build_client,
        actions::request::{build_http_request, handle_http_response},
        config::Config,
    };
    use mockito::Server;
    use std::{collections::HashMap, io::Write, sync::Arc};
    use tokio::sync::Mutex;

    // Helper to create config from YAML
    fn create_config(yaml: &str) -> Config {
        let mut tmp_file = tempfile::NamedTempFile::new().expect("Failed to create temp file");
        tmp_file
            .write_all(yaml.as_bytes())
            .expect("Failed to write to temp file");
        tmp_file.flush().expect("Failed to flush temp file");
        Config::new(tmp_file.path().to_path_buf()).expect("Failed to load config")
    }

    #[tokio::test]
    async fn test_metrics() {
        // Start mock server
        let mut server = Server::new_async().await;
        let mock_url = server.url();

        let yaml = format!(
            r"
---
services:
  test:
    url: {mock_url}/test
    every: 30s
    expect:
      status: 200
    "
        );

        let config = create_config(&yaml);
        let service = config.services.get("test").expect("Service not found");

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

        let (builder, _client_config) =
            build_client(service).expect("Failed to build client builder");
        let client = builder.build().expect("Failed to build client");
        let request = build_http_request(&client, service).expect("Failed to build request");
        let response = client
            .execute(request.build().expect("Failed to build request"))
            .await
            .expect("Failed to execute request");
        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let metrics =
            Arc::new(ServiceMetrics::new().expect("Failed to initialize service metrics"));

        // Fetch initial values
        let initial_status = metrics
            .epazote_status
            .get_metric_with_label_values(&["test"])
            .map_or(0, |m| m.get());

        let initial_failures = metrics
            .epazote_failures_total
            .get_metric_with_label_values(&["test"])
            .map_or(0, |m| m.get());

        let rs = handle_http_response("test", service, response, &metrics, counters.clone()).await;

        assert!(rs.is_ok());

        // Fetch updated values
        let updated_status = metrics
            .epazote_status
            .get_metric_with_label_values(&["test"])
            .map_or(0, |m| m.get());

        let updated_failures = metrics
            .epazote_failures_total
            .get_metric_with_label_values(&["test"])
            .map_or(0, |m| m.get());

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

        let request = build_http_request(&client, service).expect("Failed to build request");
        let response = client
            .execute(request.build().expect("Failed to build request"))
            .await
            .expect("Failed to execute request");

        let rs = handle_http_response("test", service, response, &metrics, counters)
            .await
            .expect("Failed to handle HTTP response");
        // assert rs is false
        assert!(!rs);

        // Fetch updated values
        let updated_status = metrics
            .epazote_status
            .get_metric_with_label_values(&["test"])
            .map_or(0, |m| m.get());

        assert_eq!(
            updated_status, 0,
            "Service status should be 0 after a failed request"
        );
    }

    /// A service that has never failed must still export
    /// `epazote_failures_total` at 0. Without pre-creating the counter child,
    /// the series is absent entirely and dashboards show "No data" instead of
    /// a legitimate zero.
    #[test]
    fn test_failures_counter_is_exported_before_any_failure() {
        let metrics = ServiceMetrics::new().expect("Failed to create metrics");

        metrics.init_service("never-failed", true);

        let encoder = prometheus::TextEncoder::new();
        let mut out = String::new();
        encoder
            .encode_utf8(&metrics.registry.gather(), &mut out)
            .expect("Failed to encode metrics");

        assert!(
            out.contains(r#"epazote_failures_total{service_name="never-failed"} 0"#),
            "failures counter must be exported at 0 before any failure, got:\n{out}"
        );
    }

    fn encode(metrics: &ServiceMetrics) -> String {
        let encoder = prometheus::TextEncoder::new();
        let mut out = String::new();
        encoder
            .encode_utf8(&metrics.registry.gather(), &mut out)
            .expect("Failed to encode metrics");
        out
    }

    #[test]
    fn test_fallback_metrics_are_exported_before_any_failure() {
        let metrics = ServiceMetrics::new().expect("Failed to create metrics");

        metrics.init_service("never-failed", true);

        let out = encode(&metrics);

        for expected in [
            r#"epazote_consecutive_failures{service_name="never-failed"} 0"#,
            r#"epazote_fallback_exhausted{service_name="never-failed"} 0"#,
            r#"epazote_fallback_executions_total{outcome="success",service_name="never-failed"} 0"#,
            r#"epazote_fallback_executions_total{outcome="failure",service_name="never-failed"} 0"#,
            r#"epazote_fallback_executions_total{outcome="skipped",service_name="never-failed"} 0"#,
        ] {
            assert!(
                out.contains(expected),
                "missing `{expected}` before any failure, got:\n{out}"
            );
        }
    }

    /// `epazote_fallback_exhausted = 0` on its own reads as "the fallback has
    /// not given up yet". For a service with no `if_not` that is false and
    /// reassuring: there is no fallback at all. Seeded for every service, the
    /// gauge drew both cases as the same green band. Only the pair tells them
    /// apart.
    #[test]
    fn test_configured_gauge_separates_armed_fallback_from_none() {
        let metrics = ServiceMetrics::new().expect("Failed to create metrics");

        metrics.init_service("has-if-not", true);
        metrics.init_service("no-if-not", false);

        let out = encode(&metrics);

        for expected in [
            r#"epazote_fallback_configured{service_name="has-if-not"} 1"#,
            r#"epazote_fallback_configured{service_name="no-if-not"} 0"#,
        ] {
            assert!(out.contains(expected), "missing `{expected}`, got:\n{out}");
        }

        // The reason the second gauge is needed: on this one alone a service
        // with no fallback is indistinguishable from one with an available one.
        for service in ["has-if-not", "no-if-not"] {
            let exhausted = format!(r#"epazote_fallback_exhausted{{service_name="{service}"}} 0"#);
            assert!(
                out.contains(&exhausted),
                "missing `{exhausted}`, got:\n{out}"
            );
        }
    }

    #[test]
    fn test_last_check_timestamp_is_absent_until_first_check() {
        let metrics = ServiceMetrics::new().expect("Failed to create metrics");

        metrics.init_service("never-checked", true);

        assert!(
            !encode(&metrics).contains("epazote_last_check_timestamp_seconds{"),
            "seeding the timestamp would claim a check happened at the Unix epoch"
        );

        metrics.record_check_state("never-checked", 3);

        let out = encode(&metrics);
        assert!(
            out.contains(r#"epazote_last_check_timestamp_seconds{service_name="never-checked"}"#),
            "the timestamp must appear once a check has completed"
        );
        assert!(
            out.contains(r#"epazote_consecutive_failures{service_name="never-checked"} 3"#),
            "the known failure streak must be published with the check result"
        );
    }

    #[test]
    fn test_exhausted_gauge_marks_a_service_that_stopped_running_fallbacks() {
        let metrics = ServiceMetrics::new().expect("Failed to create metrics");

        metrics.init_service("gave-up", true);
        // The two gauges have separate writers on purpose: the streak is
        // published the moment the check result is known, the exhausted flag
        // only once the scan and any recovery it triggered have returned.
        metrics.record_check_state("gave-up", 7);
        metrics.sync_fallback_exhausted("gave-up", true);

        let out = encode(&metrics);

        assert!(
            out.contains(r#"epazote_consecutive_failures{service_name="gave-up"} 7"#),
            "consecutive failures must be published, got:\n{out}"
        );
        assert!(
            out.contains(r#"epazote_fallback_exhausted{service_name="gave-up"} 1"#),
            "a service past its stop budget must be marked exhausted, got:\n{out}"
        );
    }

    /// Regression: the exhausted gauge must not quietly re-publish the streak.
    ///
    /// It used to write both, which meant `epazote_consecutive_failures` had two
    /// sources - this one, reading state back after the scan, and
    /// `record_check_state`, writing it the moment the result was known. They
    /// agreed, but nothing held them to it.
    #[test]
    fn test_the_exhausted_gauge_does_not_write_the_failure_streak() {
        let metrics = ServiceMetrics::new().expect("Failed to create metrics");

        metrics.init_service("single-writer", true);
        metrics.record_check_state("single-writer", 4);
        metrics.sync_fallback_exhausted("single-writer", true);

        let out = encode(&metrics);

        assert!(
            out.contains(r#"epazote_consecutive_failures{service_name="single-writer"} 4"#),
            "the streak must still read what the check published, got:\n{out}"
        );
    }

    #[test]
    fn test_record_fallback_counts_each_outcome_separately() {
        let metrics = ServiceMetrics::new().expect("Failed to create metrics");

        metrics.init_service("svc", true);
        metrics.record_fallback("svc", FALLBACK_SUCCESS);
        metrics.record_fallback("svc", FALLBACK_FAILURE);
        metrics.record_fallback("svc", FALLBACK_FAILURE);

        let out = encode(&metrics);

        assert!(
            out.contains(
                r#"epazote_fallback_executions_total{outcome="success",service_name="svc"} 1"#
            ),
            "got:\n{out}"
        );
        assert!(
            out.contains(
                r#"epazote_fallback_executions_total{outcome="failure",service_name="svc"} 2"#
            ),
            "a fallback that ran and failed must be distinguishable from one that worked, got:\n{out}"
        );
    }

    #[test]
    fn test_build_info_is_exported() {
        let metrics = ServiceMetrics::new().expect("Failed to create metrics");

        let out = encode(&metrics);

        assert!(
            out.contains(&format!(r#"version="{}""#, env!("CARGO_PKG_VERSION"))),
            "build info must carry the running version so a partial rollout is visible, got:\n{out}"
        );
        assert!(
            out.contains("epazote_build_info{"),
            "build info must be exported without waiting for a service, got:\n{out}"
        );
    }

    #[tokio::test]
    async fn test_bind_listener_explicit_loopback() {
        // An explicit loopback address binds to exactly that interface.
        let listener = bind_listener("127.0.0.1", 0)
            .await
            .expect("should bind to loopback");
        let addr = listener.local_addr().expect("should have a local addr");
        assert!(addr.ip().is_loopback(), "expected loopback, got {addr}");
        assert!(addr.is_ipv4(), "expected IPv4, got {addr}");
    }

    #[tokio::test]
    async fn test_bind_listener_default_dual_stack() {
        // The default [::] address binds successfully (with the historical
        // IPv4 fallback when IPv6 is unavailable).
        let listener = bind_listener("[::]", 0)
            .await
            .expect("default bind should succeed");
        assert!(listener.local_addr().is_ok());
    }

    #[tokio::test]
    async fn test_bind_listener_explicit_address_does_not_fall_back() {
        // An explicitly requested but unbindable address must error rather than
        // silently falling back to 0.0.0.0 (only the default [::] falls back).
        let result = bind_listener("203.0.113.1", 0).await;
        assert!(
            result.is_err(),
            "explicit non-local address should fail without fallback, got {result:?}"
        );
    }
}
