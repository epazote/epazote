use crate::cli::actions::metrics::ServiceMetrics;
use anyhow::{Context, Result};
use rustls::pki_types::ServerName;
use rustls::ClientConfig;
use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tracing::{debug, instrument};
use url::Url;
use x509_parser::parse_x509_certificate;

/// Extracts host and port from a URL
fn extract_host_port(url: &str) -> Result<(String, u16)> {
    let parsed_url = Url::parse(url)?;
    let host = parsed_url
        .host_str()
        .context("Invalid URL: No host found")?
        .to_string();
    let port = parsed_url
        .port_or_known_default()
        .context("Unable to determine port")?;
    Ok((host, port))
}

/// Retrieves the SSL certificate expiration time in seconds
#[instrument]
async fn get_cert_expiration_time(host: String, port: u16) -> Result<u64> {
    let mut roots = rustls::RootCertStore::empty();

    for cert in rustls_native_certs::load_native_certs().expect("could not load platform certs") {
        roots.add(cert)?;
    }

    // Configure TLS client
    let config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();

    let connector = TlsConnector::from(Arc::new(config));

    // Establish TCP connection
    let addr = format!("{}:{}", host, port);
    let stream = TcpStream::connect(&addr)
        .await
        .context("Failed to establish TCP connection")?;

    // Perform TLS handshake
    let server_name =
        ServerName::try_from(host).map_err(|_| anyhow::anyhow!("Invalid DNS name"))?;

    let tls_stream = connector
        .connect(server_name, stream)
        .await
        .context("TLS handshake failed")?;

    // Extract leaf certificate
    let cert = tls_stream
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|certs| certs.first())
        .context("No certificate found")?;

    // Parse certificate validity
    let (_, parsed_cert) =
        parse_x509_certificate(cert.as_ref()).context("Failed to parse X.509 certificate")?;

    // Calculate remaining seconds
    let not_after = parsed_cert.validity().not_after.timestamp() as u64;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();

    Ok(not_after.saturating_sub(now))
}

/// Checks the SSL certificate expiration for a given URL
pub async fn check_ssl_certificate(
    url: &str,
    service_name: &str,
    metrics: &ServiceMetrics,
) -> Result<()> {
    let (host, port) = extract_host_port(url)?;
    let remaining = get_cert_expiration_time(host, port).await?;

    // Update metrics
    metrics
        .service_ssl_cert_expiry_seconds
        .with_label_values(&[service_name])
        .set(remaining.try_into()?);

    debug!(
        "SSL Certificate for {} expires in {} seconds",
        url, remaining
    );

    Ok(())
}
