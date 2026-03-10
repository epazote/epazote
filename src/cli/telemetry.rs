use anyhow::Result;
use opentelemetry::{KeyValue, global, trace::TracerProvider as _};
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::{
    Resource,
    trace::{SdkTracerProvider, Tracer},
};
use std::{env, time::Duration};
use tracing::Level;
use tracing_subscriber::{EnvFilter, Registry, fmt, layer::SubscriberExt};

fn init_tracer() -> Result<Tracer> {
    let tracer_provider = SdkTracerProvider::builder()
        .with_batch_exporter(
            opentelemetry_otlp::SpanExporter::builder()
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

    global::set_tracer_provider(tracer_provider.clone());

    Ok(tracer_provider.tracer(env!("CARGO_PKG_NAME")))
}

fn otlp_enabled() -> bool {
    if matches!(
        env::var("OTEL_SDK_DISABLED"),
        Ok(value) if value.eq_ignore_ascii_case("true")
    ) {
        return false;
    }

    env::var_os("OTEL_EXPORTER_OTLP_ENDPOINT").is_some()
        || env::var_os("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT").is_some()
}

/// Start the telemetry layer
/// # Errors
/// Will return an error if the telemetry layer fails to start
pub fn init(verbosity_level: Option<Level>) -> Result<()> {
    let verbosity_level = verbosity_level.unwrap_or(Level::ERROR);

    let fmt_layer = fmt::layer()
        .with_file(false)
        .with_line_number(false)
        .with_thread_ids(false)
        .with_thread_names(false)
        .with_target(false)
        .json();

    // RUST_LOG=
    let filter = EnvFilter::builder()
        .with_default_directive(verbosity_level.into())
        .from_env_lossy()
        .add_directive("hyper=error".parse()?)
        .add_directive("tokio=error".parse()?)
        .add_directive("reqwest=error".parse()?);

    let subscriber = Registry::default().with(fmt_layer).with(filter);

    if otlp_enabled() {
        let tracer = init_tracer()?;
        let otel_tracer_layer = tracing_opentelemetry::layer().with_tracer(tracer);

        return Ok(tracing::subscriber::set_global_default(
            subscriber.with(otel_tracer_layer),
        )?);
    }

    Ok(tracing::subscriber::set_global_default(subscriber)?)
}

#[cfg(test)]
mod tests {
    use super::otlp_enabled;

    #[test]
    fn test_otlp_disabled_by_default() {
        unsafe {
            std::env::remove_var("OTEL_SDK_DISABLED");
            std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT");
            std::env::remove_var("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT");
        }

        assert!(!otlp_enabled());
    }

    #[test]
    fn test_otlp_enabled_with_endpoint() {
        unsafe {
            std::env::remove_var("OTEL_SDK_DISABLED");
            std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", "http://127.0.0.1:4317");
        }

        assert!(otlp_enabled());

        unsafe {
            std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT");
        }
    }

    #[test]
    fn test_otlp_disabled_explicitly() {
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
