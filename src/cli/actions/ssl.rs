use crate::cli::actions::metrics::ServiceMetrics;
use anyhow::{Context, Result, anyhow};
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, RootCertStore};
use rustls_native_certs::CertificateResult;
use std::{
    collections::HashMap,
    sync::Arc,
    sync::LazyLock,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

static ROOT_CERT_STORE: LazyLock<Result<RootCertStore, String>> =
    LazyLock::new(load_native_root_cert_store);

fn root_cert_store_from_certs(certs: Vec<CertificateDer<'static>>) -> RootCertStore {
    let mut roots = RootCertStore::empty();

    for cert in certs {
        let _ = roots.add(cert);
    }

    roots
}

fn root_cert_store_from_native_parts(
    certs: Vec<CertificateDer<'static>>,
    errors: &[String],
) -> Result<RootCertStore> {
    if !errors.is_empty() {
        return Err(anyhow!(
            "could not load platform certs: {}",
            errors.join("; ")
        ));
    }

    Ok(root_cert_store_from_certs(certs))
}

fn root_cert_store_from_native_result(result: CertificateResult) -> Result<RootCertStore> {
    let errors = result
        .errors
        .into_iter()
        .map(|error| error.to_string())
        .collect::<Vec<_>>();

    root_cert_store_from_native_parts(result.certs, &errors)
}

fn load_native_root_cert_store() -> Result<RootCertStore, String> {
    root_cert_store_from_native_result(rustls_native_certs::load_native_certs())
        .map_err(|error| error.to_string())
}

fn native_root_cert_store() -> Result<RootCertStore> {
    ROOT_CERT_STORE
        .as_ref()
        .map(Clone::clone)
        .map_err(|error| anyhow!(error.clone()))
}

use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::time;
use tokio_rustls::TlsConnector;
use tracing::debug;
use url::Url;
use x509_parser::parse_x509_certificate;

const SSL_RECHECK_INTERVAL_SECS: u64 = 60 * 60 * 12;

// How long a failed certificate check is remembered before it is retried. A
// down service would otherwise repeat the whole connect and handshake on every
// scan, ahead of the HTTP request that already reports it as unhealthy.
const SSL_FAILURE_RETRY_SECS: u64 = 60;

#[derive(Clone, Copy, Debug)]
pub struct SslCheckState {
    checked_at_epoch_secs: u64,
    // `None` records a failed check, which only throttles the retry.
    remaining_secs_at_check: Option<u64>,
}

impl SslCheckState {
    const fn succeeded(checked_at_epoch_secs: u64, remaining_secs: u64) -> Self {
        Self {
            checked_at_epoch_secs,
            remaining_secs_at_check: Some(remaining_secs),
        }
    }

    const fn failed(checked_at_epoch_secs: u64) -> Self {
        Self {
            checked_at_epoch_secs,
            remaining_secs_at_check: None,
        }
    }

    fn remaining_secs_now(self, now_epoch_secs: u64) -> Option<u64> {
        let elapsed = now_epoch_secs.saturating_sub(self.checked_at_epoch_secs);
        self.remaining_secs_at_check
            .map(|remaining| remaining.saturating_sub(elapsed))
    }

    fn should_refresh(self, now_epoch_secs: u64) -> bool {
        let elapsed = now_epoch_secs.saturating_sub(self.checked_at_epoch_secs);

        match self.remaining_secs_now(now_epoch_secs) {
            // A failed check is retried far sooner than a valid certificate is
            // re-read, so a recovering service is picked up quickly.
            None => elapsed >= SSL_FAILURE_RETRY_SECS,
            Some(remaining) => elapsed >= SSL_RECHECK_INTERVAL_SECS || remaining == 0,
        }
    }
}

pub type SslCheckCache = Arc<Mutex<HashMap<String, SslCheckState>>>;

#[must_use]
pub fn new_ssl_check_cache() -> SslCheckCache {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Timestamp for recording a finished check. Falls back to the check's start
/// time if the clock cannot be read, which is never worse than the old
/// behaviour of always stamping the start.
fn completed_at_epoch_secs(started_at_epoch_secs: u64) -> u64 {
    current_epoch_secs().unwrap_or(started_at_epoch_secs)
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

async fn get_cert_expiration_time_with_roots(
    host: String,
    port: u16,
    root_cert_store: RootCertStore,
) -> Result<u64> {
    // Configure TLS client
    let config = ClientConfig::builder()
        .with_root_certificates(root_cert_store)
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
    timeout: Duration,
) -> Result<()> {
    // The store is built lazily: passing it by value cloned all system root
    // certificates on every scan, including the vast majority that hit the
    // cache below and never open a TLS connection.
    check_ssl_certificate_with_roots(
        url,
        service_name,
        metrics,
        cache,
        native_root_cert_store,
        timeout,
    )
    .await
}

async fn check_ssl_certificate_with_roots<F>(
    url: &str,
    service_name: &str,
    metrics: &ServiceMetrics,
    cache: &SslCheckCache,
    root_cert_store: F,
    timeout: Duration,
) -> Result<()>
where
    F: FnOnce() -> Result<RootCertStore>,
{
    let now_epoch_secs = current_epoch_secs()?;

    if let Some(cached_state) = {
        let cache = cache.lock().await;
        cache.get(service_name).copied()
    } && !cached_state.should_refresh(now_epoch_secs)
    {
        // A remembered failure only throttles the retry: there is no expiry
        // to report, and the HTTP check already decides service health.
        if let Some(remaining) = cached_state.remaining_secs_now(now_epoch_secs) {
            metrics
                .epazote_ssl_cert_expiry_seconds
                .with_label_values(&[service_name])
                .set(remaining.try_into()?);
        }

        return Ok(());
    }

    let (host, port) = extract_host_port(url)?;
    // Neither the TCP connect nor the TLS handshake is bounded on its own, so
    // a blackholed address or a peer that accepts TCP and never completes the
    // handshake would stall this service's scans indefinitely.
    let outcome = match time::timeout(
        timeout,
        get_cert_expiration_time_with_roots(host, port, root_cert_store()?),
    )
    .await
    {
        Ok(remaining) => remaining,
        Err(_) => Err(anyhow!(
            "certificate check exceeded the service 'timeout' of {timeout:?}"
        )),
    };

    let remaining = match outcome {
        Ok(remaining) => remaining,
        Err(error) => {
            // Remember the failure so a down service is not re-probed on every
            // scan ahead of the HTTP request. Stamped when the outcome is
            // recorded rather than when the check began: a failure that burned
            // the whole timeout would otherwise be cached already expired, so
            // the backoff vanished exactly for the slow failures it exists for.
            cache.lock().await.insert(
                service_name.to_string(),
                SslCheckState::failed(completed_at_epoch_secs(now_epoch_secs)),
            );

            return Err(error);
        }
    };

    {
        let mut cache = cache.lock().await;
        // `remaining` was read at the end of the handshake, so it is relative
        // to completion, not to when the check started.
        cache.insert(
            service_name.to_string(),
            SslCheckState::succeeded(completed_at_epoch_secs(now_epoch_secs), remaining),
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A failed check is remembered briefly, so a down service is not
    /// re-probed ahead of the HTTP request on every single scan.
    #[tokio::test]
    async fn test_failed_check_is_cached_until_retry_window_elapses() {
        let metrics = ServiceMetrics::new().expect("Failed to create metrics");
        let cache = new_ssl_check_cache();
        let builds = AtomicUsize::new(0);

        // Port 1 is closed, so the check fails immediately.
        for _ in 0..3 {
            let _ = check_ssl_certificate_with_roots(
                "https://127.0.0.1:1/health",
                "down_service",
                &metrics,
                &cache,
                || {
                    builds.fetch_add(1, Ordering::SeqCst);
                    Ok(RootCertStore::empty())
                },
                Duration::from_secs(2),
            )
            .await;
        }

        assert_eq!(
            builds.load(Ordering::SeqCst),
            1,
            "a failed check must be remembered instead of repeated every scan"
        );
    }

    /// Regression: the failure was stamped with the check's *start* time, so a
    /// check that burned its whole timeout was cached already expired and the
    /// backoff did nothing for exactly the slow failures it exists for.
    #[tokio::test]
    async fn test_failed_check_is_stamped_at_completion_not_at_start() {
        // Accepts TCP, never completes the handshake, so the check runs for
        // the full timeout below.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("Failed to bind listener");
        let addr = listener.local_addr().expect("Failed to get local addr");

        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                held.push(stream);
            }
        });

        let metrics = ServiceMetrics::new().expect("Failed to create metrics");
        let cache = new_ssl_check_cache();
        let started_at = current_epoch_secs().expect("Failed to read clock");

        let _ = check_ssl_certificate_with_roots(
            &format!("https://{addr}/health"),
            "slow_failure",
            &metrics,
            &cache,
            || Ok(RootCertStore::empty()),
            Duration::from_secs(2),
        )
        .await;

        let cached = cache
            .lock()
            .await
            .get("slow_failure")
            .copied()
            .expect("the failure should be cached");

        assert!(
            cached.checked_at_epoch_secs > started_at,
            "a failure must be stamped when it is recorded ({}), not when the \
             check began ({started_at})",
            cached.checked_at_epoch_secs
        );
    }

    /// The remembered failure must expire, so a recovering service is picked
    /// up again rather than being ignored forever.
    #[test]
    fn test_failed_check_is_retried_after_the_backoff() {
        let failed = SslCheckState::failed(1_000);

        assert!(
            !failed.should_refresh(1_000 + SSL_FAILURE_RETRY_SECS - 1),
            "a fresh failure should still be throttled"
        );
        assert!(
            failed.should_refresh(1_000 + SSL_FAILURE_RETRY_SECS),
            "a failure older than the backoff must be retried"
        );
        assert!(
            failed.remaining_secs_now(1_000).is_none(),
            "a failed check has no expiry to report"
        );
    }

    /// Regression: neither the TCP connect nor the TLS handshake was bounded,
    /// so a peer that accepts the connection and never completes the handshake
    /// stalled every later scan for that service.
    #[tokio::test]
    async fn test_check_ssl_certificate_times_out_on_silent_peer() {
        // Accepts TCP, then never speaks TLS.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("Failed to bind listener");
        let addr = listener.local_addr().expect("Failed to get local addr");

        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                held.push(stream);
            }
        });

        let metrics = ServiceMetrics::new().expect("Failed to create metrics");
        let cache = new_ssl_check_cache();

        let started = std::time::Instant::now();
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            check_ssl_certificate_with_roots(
                &format!("https://{addr}/health"),
                "silent_peer",
                &metrics,
                &cache,
                || Ok(RootCertStore::empty()),
                Duration::from_millis(300),
            ),
        )
        .await
        .expect("the certificate check must not hang past its timeout");

        let error = result.expect_err("a silent peer should fail the check");
        assert!(
            format!("{error:#}").contains("timeout"),
            "unexpected error: {error:#}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "check should return near the configured timeout"
        );
    }

    /// Regression: a cached certificate check must not build the root
    /// certificate store. It was previously passed by value, so every HTTPS
    /// scan cloned all system roots even when no TLS connection was opened.
    #[tokio::test]
    async fn test_check_ssl_certificate_skips_root_store_on_cache_hit() {
        let metrics = ServiceMetrics::new().expect("Failed to create metrics");
        let cache = new_ssl_check_cache();
        let now = current_epoch_secs().expect("Failed to read clock");

        cache.lock().await.insert(
            "cached_service".to_string(),
            SslCheckState::succeeded(now, 86_400),
        );

        let builds = AtomicUsize::new(0);
        let result = check_ssl_certificate_with_roots(
            "https://cached.example/health",
            "cached_service",
            &metrics,
            &cache,
            || {
                builds.fetch_add(1, Ordering::SeqCst);
                Ok(RootCertStore::empty())
            },
            Duration::from_secs(5),
        )
        .await;

        assert!(result.is_ok(), "cached check should succeed: {result:?}");
        assert_eq!(
            builds.load(Ordering::SeqCst),
            0,
            "a cache hit must not build the root certificate store"
        );
    }

    /// A stale cache entry must still fall through to a real check, which does
    /// need the root store.
    #[tokio::test]
    async fn test_check_ssl_certificate_builds_root_store_on_cache_miss() {
        let metrics = ServiceMetrics::new().expect("Failed to create metrics");
        let cache = new_ssl_check_cache();

        let builds = AtomicUsize::new(0);
        let _ = check_ssl_certificate_with_roots(
            "https://127.0.0.1:1/health",
            "uncached_service",
            &metrics,
            &cache,
            || {
                builds.fetch_add(1, Ordering::SeqCst);
                Ok(RootCertStore::empty())
            },
            Duration::from_secs(5),
        )
        .await;

        assert_eq!(
            builds.load(Ordering::SeqCst),
            1,
            "a cache miss must build the root certificate store exactly once"
        );
    }

    use anyhow::Result;
    use ctor::ctor;
    use rcgen::{CertifiedKey, generate_simple_self_signed};
    use rustls::{
        ServerConfig,
        pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer},
    };
    use tokio::{io::AsyncWriteExt, net::TcpListener};
    use tokio_rustls::TlsAcceptor;

    // Initialize crypto provider once before all tests
    #[ctor(unsafe)]
    fn init_crypto() {
        rustls::crypto::CryptoProvider::install_default(rustls::crypto::ring::default_provider())
            .expect("Failed to initialize crypto provider");
    }

    struct LocalTlsServer {
        url: String,
        roots: RootCertStore,
    }

    async fn start_local_tls_server() -> Result<LocalTlsServer> {
        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(vec!["localhost".to_string()])?;
        let cert_der = cert.der().clone();
        let key_der = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der.clone()], key_der)?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let acceptor = TlsAcceptor::from(Arc::new(config));

        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await
                && let Ok(mut stream) = acceptor.accept(stream).await
            {
                let _ = stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                    .await;
            }
        });

        let mut roots = RootCertStore::empty();
        roots.add(cert_der)?;

        Ok(LocalTlsServer {
            url: format!("https://localhost:{port}"),
            roots,
        })
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
        let server = start_local_tls_server().await?;
        let (host, port) = extract_host_port(&server.url)?;
        let remaining = get_cert_expiration_time_with_roots(host, port, server.roots).await?;
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
        let remaining =
            get_cert_expiration_time_with_roots(host, port, RootCertStore::empty()).await;
        assert!(remaining.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn test_check_ssl_certificate_sets_metric_with_local_tls() -> Result<()> {
        let server = start_local_tls_server().await?;
        let metrics = ServiceMetrics::new()?;
        let cache = new_ssl_check_cache();

        check_ssl_certificate_with_roots(
            &server.url,
            "local_tls",
            &metrics,
            &cache,
            || Ok(server.roots),
            Duration::from_secs(5),
        )
        .await?;

        let gauge = metrics
            .epazote_ssl_cert_expiry_seconds
            .get_metric_with_label_values(&["local_tls"])?;
        assert!(gauge.get() > 0);
        Ok(())
    }

    #[test]
    fn test_root_cert_store_from_native_result_returns_errors() {
        assert!(
            root_cert_store_from_native_parts(Vec::new(), &["missing cert store".to_string()])
                .is_err()
        );
    }

    #[test]
    fn test_ssl_check_state_uses_cached_value_until_refresh() {
        let state = SslCheckState::succeeded(100, 1_000);

        assert_eq!(state.remaining_secs_now(250), Some(850));
        assert!(!state.should_refresh(250));
        assert!(state.should_refresh(100 + SSL_RECHECK_INTERVAL_SECS));
    }
}
