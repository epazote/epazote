use crate::cli::actions::metrics::ServiceMetrics;
use anyhow::{Context, Result};
use rustls::ClientConfig;
use rustls::pki_types::ServerName;
use std::{
    collections::HashMap,
    sync::Arc,
    sync::LazyLock,
    time::{SystemTime, UNIX_EPOCH},
};

static ROOT_CERT_STORE: LazyLock<rustls::RootCertStore> = LazyLock::new(|| {
    let mut roots = rustls::RootCertStore::empty();

    for cert in rustls_native_certs::load_native_certs().expect("could not load platform certs") {
        let _ = roots.add(cert);
    }
    roots
});
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_rustls::TlsConnector;
use tracing::debug;
use url::Url;
use x509_parser::parse_x509_certificate;

const SSL_RECHECK_INTERVAL_SECS: u64 = 60 * 60 * 12;

#[derive(Clone, Copy, Debug)]
pub struct SslCheckState {
    checked_at_epoch_secs: u64,
    remaining_secs_at_check: u64,
}

impl SslCheckState {
    fn remaining_secs_now(self, now_epoch_secs: u64) -> u64 {
        let elapsed = now_epoch_secs.saturating_sub(self.checked_at_epoch_secs);
        self.remaining_secs_at_check.saturating_sub(elapsed)
    }

    fn should_refresh(self, now_epoch_secs: u64) -> bool {
        let elapsed = now_epoch_secs.saturating_sub(self.checked_at_epoch_secs);
        elapsed >= SSL_RECHECK_INTERVAL_SECS || self.remaining_secs_now(now_epoch_secs) == 0
    }
}

pub type SslCheckCache = Arc<Mutex<HashMap<String, SslCheckState>>>;

#[must_use]
pub fn new_ssl_check_cache() -> SslCheckCache {
    Arc::new(Mutex::new(HashMap::new()))
}

fn current_epoch_secs() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

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
async fn get_cert_expiration_time(host: String, port: u16) -> Result<u64> {
    // Configure TLS client
    let config = ClientConfig::builder()
        .with_root_certificates(ROOT_CERT_STORE.clone())
        .with_no_client_auth();

    let connector = TlsConnector::from(Arc::new(config));

    // Establish TCP connection
    let addr = format!("{host}:{port}");
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
    #[allow(clippy::cast_sign_loss)]
    let not_after = parsed_cert.validity().not_after.timestamp() as u64;

    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();

    let remaining = not_after.saturating_sub(now);

    debug!(
        "Certificate for: {},  expies in: {}s, not after: {}",
        addr,
        remaining,
        parsed_cert.validity().not_after
    );

    Ok(remaining)
}

/// Checks the SSL certificate expiration for a given URL
///
/// # Errors
///
/// Returns an error if the URL is invalid, the host cannot be reached, or the certificate is invalid.
pub async fn check_ssl_certificate(
    url: &str,
    service_name: &str,
    metrics: &ServiceMetrics,
    cache: &SslCheckCache,
) -> Result<()> {
    let now_epoch_secs = current_epoch_secs()?;

    if let Some(cached_state) = {
        let cache = cache.lock().await;
        cache.get(service_name).copied()
    } && !cached_state.should_refresh(now_epoch_secs)
    {
        metrics
            .epazote_ssl_cert_expiry_seconds
            .with_label_values(&[service_name])
            .set(cached_state.remaining_secs_now(now_epoch_secs).try_into()?);

        return Ok(());
    }

    let (host, port) = extract_host_port(url)?;
    let remaining = get_cert_expiration_time(host, port).await?;

    {
        let mut cache = cache.lock().await;
        cache.insert(
            service_name.to_string(),
            SslCheckState {
                checked_at_epoch_secs: now_epoch_secs,
                remaining_secs_at_check: remaining,
            },
        );
    }

    // Update metrics
    metrics
        .epazote_ssl_cert_expiry_seconds
        .with_label_values(&[service_name])
        .set(remaining.try_into()?);

    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use anyhow::Result;
    use ctor::ctor;
    use rustls::crypto::CryptoProvider;

    // Initialize crypto provider once before all tests
    #[ctor]
    fn init_crypto() {
        CryptoProvider::install_default(rustls::crypto::ring::default_provider())
            .expect("Failed to initialize crypto provider");
    }

    #[test]
    fn test_extract_host_port() -> Result<()> {
        let url = "https://example.com:443";
        let (host, port) = extract_host_port(url)?;
        assert_eq!(host, "example.com");
        assert_eq!(port, 443);
        Ok(())
    }

    #[tokio::test]
    async fn test_get_cert_expiration_time() -> Result<()> {
        let (host, port) = extract_host_port("https://www.google.com")?;
        let remaining = get_cert_expiration_time(host, port).await?;
        assert!(remaining > 0, "Certificate should have future expiration");
        Ok(())
    }

    #[tokio::test]
    async fn test_expired_certificate() -> Result<()> {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("GET", "/")
            .with_status(200)
            .create_async()
            .await;

        let (host, port) = extract_host_port(&server.url())?;
        let remaining = get_cert_expiration_time(host, port).await;
        assert!(remaining.is_err());
        Ok(())
    }

    #[test]
    fn test_ssl_check_state_uses_cached_value_until_refresh() {
        let state = SslCheckState {
            checked_at_epoch_secs: 100,
            remaining_secs_at_check: 1_000,
        };

        assert_eq!(state.remaining_secs_now(250), 850);
        assert!(!state.should_refresh(250));
        assert!(state.should_refresh(100 + SSL_RECHECK_INTERVAL_SECS));
    }
}
