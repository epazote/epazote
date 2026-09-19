use anyhow::Result;
#[cfg(feature = "telemetry")]
use opentelemetry::KeyValue;
#[cfg(feature = "telemetry")]
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
#[cfg(feature = "telemetry")]
use opentelemetry_otlp::WithExportConfig;
#[cfg(feature = "telemetry")]
use opentelemetry_sdk::{Resource, logs::SdkLoggerProvider};
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(feature = "telemetry")]
use std::{env, sync::OnceLock, time::Duration};
use tracing::Level;
use tracing_subscriber::{EnvFilter, Registry, fmt, layer::SubscriberExt};

static PRETTY_LOGS_ENABLED: AtomicBool = AtomicBool::new(false);
#[cfg(feature = "telemetry")]
static LOGGER_PROVIDER: OnceLock<SdkLoggerProvider> = OnceLock::new();

#[cfg(feature = "telemetry")]
fn init_logger() -> Result<SdkLoggerProvider> {
    let logger_provider = SdkLoggerProvider::builder()
        .with_batch_exporter(
            opentelemetry_otlp::LogExporter::builder()
                .with_tonic()
                .with_timeout(Duration::from_secs(3))
                .build()?,
        )
        .with_resource(
            Resource::builder_empty()
                .with_attributes(vec![
                    KeyValue::new("service.name", env!("CARGO_PKG_NAME")),
                    KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
                ])
                .build(),
        )
        .build();

    let _ = LOGGER_PROVIDER.set(logger_provider.clone());

    Ok(logger_provider)
}

#[cfg(feature = "telemetry")]
fn otlp_enabled() -> bool {
    if matches!(
        env::var("OTEL_SDK_DISABLED"),
        Ok(value) if value.eq_ignore_ascii_case("true")
    ) {
        return false;
    }

    env::var_os("OTEL_EXPORTER_OTLP_ENDPOINT").is_some()
        || env::var_os("OTEL_EXPORTER_OTLP_LOGS_ENDPOINT").is_some()
}

pub(crate) fn pretty_logs_enabled() -> bool {
    PRETTY_LOGS_ENABLED.load(Ordering::Relaxed)
}

/// Builds the log filter.
///
/// Split from [`init`] so the directives can be tested: [`init`] installs a
/// global subscriber, which can only be done once per process.
fn build_filter(verbosity_level: Level) -> Result<EnvFilter> {
    // RUST_LOG=
    Ok(EnvFilter::builder()
        .with_default_directive(verbosity_level.into())
        .from_env_lossy()
        .add_directive("hyper=error".parse()?)
        .add_directive("tokio=error".parse()?)
        .add_directive("reqwest=error".parse()?)
        // The default level is ERROR and packaged installs leave
        // EPAZOTE_VERBOSE empty, so a warning would go unseen in exactly the
        // deployment that needs it. Config emits one thing - a start-up
        // diagnostic that the configuration is valid but probably does not
        // mean what was intended - which is worth the same visibility as the
        // errors that refuse to start at all.
        .add_directive("epazote::cli::config=warn".parse()?))
}

/// Start local logging and, when compiled and configured, OTLP log export.
///
/// OTLP export requires the `telemetry` Cargo feature and either
/// `OTEL_EXPORTER_OTLP_ENDPOINT` or `OTEL_EXPORTER_OTLP_LOGS_ENDPOINT`.
/// Without the feature, these environment variables have no effect.
///
/// # Errors
///
/// Will return an error if local logging or the optional telemetry layer fails to start.
pub fn init(verbosity_level: Option<Level>, json_logs: bool) -> Result<()> {
    let verbosity_level = verbosity_level.unwrap_or(Level::ERROR);
    PRETTY_LOGS_ENABLED.store(!json_logs, Ordering::Relaxed);

    let filter = build_filter(verbosity_level)?;

    if json_logs {
        let fmt_layer = fmt::layer()
            .with_file(false)
            .with_line_number(false)
            .with_thread_ids(false)
            .with_thread_names(false)
            .with_target(false)
            .json();

        let subscriber = Registry::default().with(fmt_layer).with(filter);

        #[cfg(feature = "telemetry")]
        if otlp_enabled() {
            let logger_provider = init_logger()?;
            let otel_log_layer = OpenTelemetryTracingBridge::new(&logger_provider);

            return Ok(tracing::subscriber::set_global_default(
                subscriber.with(otel_log_layer),
            )?);
        }

        return Ok(tracing::subscriber::set_global_default(subscriber)?);
    }

    let fmt_layer = fmt::layer()
        .pretty()
        .with_file(false)
        .with_line_number(false)
        .with_thread_ids(false)
        .with_thread_names(false)
        .with_target(false);

    let subscriber = Registry::default().with(fmt_layer).with(filter);

    #[cfg(feature = "telemetry")]
    if otlp_enabled() {
        let logger_provider = init_logger()?;
        let otel_log_layer = OpenTelemetryTracingBridge::new(&logger_provider);

        return Ok(tracing::subscriber::set_global_default(
            subscriber.with(otel_log_layer),
        )?);
    }

    Ok(tracing::subscriber::set_global_default(subscriber)?)
}

/// Flush pending OTLP logs and stop the logger provider.
///
/// This is a no-op when the binary was built without the `telemetry` feature
/// or no OTLP endpoint was configured at startup.
///
/// # Errors
///
/// Returns an error if the logger provider cannot shut down cleanly.
#[cfg(feature = "telemetry")]
pub fn shutdown() -> Result<()> {
    if let Some(provider) = LOGGER_PROVIDER.get() {
        provider.shutdown()?;
    }

    Ok(())
}

/// Return immediately when OTLP telemetry was not compiled in.
///
/// # Errors
///
/// This implementation cannot fail.
#[cfg(not(feature = "telemetry"))]
pub const fn shutdown() -> Result<()> {
    Ok(())
}

#[cfg(test)]
#[allow(clippy::panic)]
mod tests {
    use super::build_filter;
    #[cfg(feature = "telemetry")]
    use super::otlp_enabled;
    use std::sync::{Arc, Mutex};
    use tracing::{Level, subscriber::with_default};
    use tracing_subscriber::{Layer, layer::Context, layer::SubscriberExt, registry};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Records the target of every event the filter lets through.
    #[derive(Clone, Default)]
    struct CapturedTargets(Arc<Mutex<Vec<String>>>);

    impl<S: tracing::Subscriber> Layer<S> for CapturedTargets {
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
            match self.0.lock() {
                Ok(mut targets) => targets.push(event.metadata().target().to_string()),
                Err(error) => panic!("failed to record event: {error}"),
            }
        }
    }

    /// Regression: the start-up warning about ungrouped fallback commands that
    /// look like they collide must be visible without `-v`.
    ///
    /// It exists to catch a config whose recovery commands will not behave as
    /// intended, and packaged installs leave `EPAZOTE_VERBOSE` empty. Filtered
    /// out at the default level it would reach nobody in the deployment that
    /// matters, so this pins the directive that keeps it visible - along with
    /// the fact that it does not drag every other
    /// warning up with it.
    #[test]
    fn test_config_warnings_are_visible_at_default_verbosity() {
        let _lock = match ENV_LOCK.lock() {
            Ok(lock) => lock,
            Err(error) => panic!("failed to lock env: {error}"),
        };
        unsafe {
            std::env::remove_var("RUST_LOG");
        }

        let filter = match build_filter(Level::ERROR) {
            Ok(filter) => filter,
            Err(error) => panic!("failed to build filter: {error}"),
        };

        let captured = CapturedTargets::default();
        let subscriber = registry().with(filter).with(captured.clone());

        with_default(subscriber, || {
            tracing::warn!(target: "epazote::cli::config", "conflict");
            tracing::warn!(target: "epazote::cli::actions", "some other warning");
            tracing::error!(target: "epazote::cli::actions", "a real error");
        });

        let targets = match captured.0.lock() {
            Ok(targets) => targets.clone(),
            Err(error) => panic!("failed to read captured events: {error}"),
        };

        assert_eq!(
            targets,
            vec![
                "epazote::cli::config".to_string(),
                "epazote::cli::actions".to_string(),
            ],
            "the config warning and the error must pass, the unrelated warning must not"
        );
    }

    #[cfg(feature = "telemetry")]
    #[test]
    fn test_otlp_disabled_by_default() {
        let _lock = match ENV_LOCK.lock() {
            Ok(lock) => lock,
            Err(error) => panic!("failed to lock env: {error}"),
        };
        unsafe {
            std::env::remove_var("OTEL_SDK_DISABLED");
            std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT");
            std::env::remove_var("OTEL_EXPORTER_OTLP_LOGS_ENDPOINT");
        }

        assert!(!otlp_enabled());
    }

    #[cfg(feature = "telemetry")]
    #[test]
    fn test_otlp_enabled_with_endpoint() {
        let _lock = match ENV_LOCK.lock() {
            Ok(lock) => lock,
            Err(error) => panic!("failed to lock env: {error}"),
        };
        unsafe {
            std::env::remove_var("OTEL_SDK_DISABLED");
            std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", "http://127.0.0.1:4317");
        }

        assert!(otlp_enabled());

        unsafe {
            std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT");
        }
    }

    #[cfg(feature = "telemetry")]
    #[test]
    fn test_otlp_enabled_with_logs_endpoint() {
        let _lock = match ENV_LOCK.lock() {
            Ok(lock) => lock,
            Err(error) => panic!("failed to lock env: {error}"),
        };
        unsafe {
            std::env::remove_var("OTEL_SDK_DISABLED");
            std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT");
            std::env::set_var("OTEL_EXPORTER_OTLP_LOGS_ENDPOINT", "http://127.0.0.1:4317");
        }

        assert!(otlp_enabled());

        unsafe {
            std::env::remove_var("OTEL_EXPORTER_OTLP_LOGS_ENDPOINT");
        }
    }

    #[cfg(feature = "telemetry")]
    #[test]
    fn test_otlp_disabled_explicitly() {
        let _lock = match ENV_LOCK.lock() {
            Ok(lock) => lock,
            Err(error) => panic!("failed to lock env: {error}"),
        };
        unsafe {
            std::env::set_var("OTEL_SDK_DISABLED", "true");
            std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", "http://127.0.0.1:4317");
        }

        assert!(!otlp_enabled());

        unsafe {
            std::env::remove_var("OTEL_SDK_DISABLED");
            std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT");
        }
    }
}
