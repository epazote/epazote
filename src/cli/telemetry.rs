use anyhow::Result;
use opentelemetry::{global, trace::TracerProvider as _, KeyValue};
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::{
    trace::{Tracer, TracerProvider},
    Resource,
};
use std::time::Duration;
use tracing::Level;
use tracing_subscriber::{fmt, layer::SubscriberExt, EnvFilter, Registry};

fn init_tracer() -> Result<Tracer> {
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_timeout(Duration::from_secs(3))
        .build()?;

    let tracer_provider = TracerProvider::builder()
        .with_batch_exporter(exporter, opentelemetry_sdk::runtime::Tokio)
        .with_resource(Resource::new(vec![
            KeyValue::new("service.name", env!("CARGO_PKG_NAME")),
            KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
        ]))
        .build();

    global::set_tracer_provider(tracer_provider.clone());

    Ok(tracer_provider.tracer(env!("CARGO_PKG_NAME")))
}

/// Start the telemetry layer
/// # Errors
/// Will return an error if the telemetry layer fails to start
pub fn init(verbosity_level: Option<Level>) -> Result<()> {
    let verbosity_level = verbosity_level.unwrap_or(Level::ERROR);

    let tracer = init_tracer()?;

    let otel_tracer_layer = tracing_opentelemetry::layer().with_tracer(tracer);

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

    let subscriber = Registry::default()
        .with(fmt_layer)
        .with(otel_tracer_layer)
        .with(filter);

    Ok(tracing::subscriber::set_global_default(subscriber)?)
}
