use crate::cli::config::ServiceDetails;
use anyhow::Result;
use reqwest::{
    Client, ClientBuilder,
    header::{HeaderMap, HeaderName, HeaderValue},
};

pub static APP_USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

#[derive(Debug)]
pub struct ClientConfig {
    pub timeout: std::time::Duration,
    pub user_agent: String,
    pub follow_redirects: bool,
    pub headers: HeaderMap,
}

/// Builds a `reqwest::Client` based on the provided `ServiceDetails`.
///
/// # Errors
///
/// Returns an error if:
/// - The `service_details` contains invalid headers.
/// - The `reqwest::Client` fails to build.
pub fn build_client(service_details: &ServiceDetails) -> Result<(ClientBuilder, ClientConfig)> {
    let timeout = service_details.timeout;
    let user_agent = APP_USER_AGENT.to_string();
    let follow_redirects = service_details.follow_redirects.unwrap_or(false);

    // A health probe must open a fresh connection on every scan instead of
    // reusing a pooled keep-alive socket. The monitored service can restart or
    // close its idle connections between scans; reusing a stale pooled socket
    // surfaces spurious "error sending request" transport failures (which can
    // trip the fallback) even though the service answers fine on a new
    // connection. Disabling the idle pool makes each scan behave like a fresh
    // `curl`, so the check reflects the service's current reachability.
    let mut builder = Client::builder()
        .timeout(timeout)
        .user_agent(&user_agent)
        .pool_max_idle_per_host(0);

    // Disable redirects if follow is not set
    if !follow_redirects {
        builder = builder.redirect(reqwest::redirect::Policy::none());
    }

    let mut headers = HeaderMap::new();

    if let Some(service_headers) = &service_details.headers {
        for (key, value) in service_headers {
            let header_name = HeaderName::from_bytes(key.as_bytes())
                .map_err(|_| anyhow::anyhow!("Invalid header name: {key}"))?;
            let header_value = HeaderValue::from_str(value)
                .map_err(|_| anyhow::anyhow!("Invalid header value for key: {key}"))?;

            headers.insert(header_name, header_value);
        }
    }

    builder = builder.default_headers(headers.clone());

    let config = ClientConfig {
        timeout,
        user_agent,
        follow_redirects,
        headers,
    };

    Ok((builder, config))
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::cli::config::Config;
    use mockito::Server;
    use std::io::Write;

    // Helper to create config from YAML
    fn create_config(yaml: &str) -> Config {
        let mut tmp_file = tempfile::NamedTempFile::new().expect("Failed to create temp file");
        tmp_file
            .write_all(yaml.as_bytes())
            .expect("Failed to write to temp file");
        tmp_file.flush().expect("Failed to flush temp file");
        Config::new(tmp_file.path().to_path_buf()).expect("Failed to load config")
    }

    #[test]
    fn test_app_user_agent_format() {
        assert_eq!(
            APP_USER_AGENT,
            concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"))
        );
    }

    #[tokio::test]
    async fn test_build_client_multiple_services() {
        let yaml = r"
---
services:
  test:
    url: https://mock
    every: 30s
    headers:
      X-Custom-Header: TestValue
    expect:
      status: 200

  test2:
    url: https://mock
    follow_redirects: true
    every: 30s
    headers:
      User-Agent: TestAgent
    expect:
      status: 200
    ";

        let mut server = Server::new_async().await;

        let expected_services = vec![
            (
                "test",
                vec![
                    ("X-Custom-Header", "TestValue"),
                    ("User-Agent", APP_USER_AGENT),
                ],
                false,
            ), // `false` for no redirects
            ("test2", vec![("User-Agent", "TestAgent")], true), // `true` for redirects
        ];

        for (service_name, headers, expected_redirect) in &expected_services {
            let mut mock = server
                .mock("GET", format!("/{service_name}").as_str())
                .with_status(200)
                .create_async()
                .await;

            // Dynamically apply `match_header`
            for (header_name, expected_value) in headers {
                mock = mock.match_header(*header_name, *expected_value);
            }

            let _m = mock.create_async().await;

            let config = create_config(yaml);
            let service = config
                .services
                .get(*service_name)
                .expect("Service not found");

            let (builder, client_config) =
                build_client(service).expect("Failed to build client builder");

            // Check timeout
            assert_eq!(client_config.timeout, std::time::Duration::from_secs(5));

            // Check user agent
            assert_eq!(client_config.user_agent, APP_USER_AGENT);

            // Check redirect policy
            assert_eq!(
                client_config.follow_redirects, *expected_redirect,
                "Follow redirects mismatch for service {service_name}"
            );

            let client = builder.build().expect("Failed to build client");
            let url = format!("{}/{service_name}", server.url());
            let response = client
                .get(url)
                .send()
                .await
                .expect("Failed to send request");

            assert_eq!(response.status(), 200);
        }
    }

    /// Regression guard: a health probe must open a fresh TCP connection on
    /// every scan and must not reuse a pooled keep-alive connection. A
    /// monitored service can restart or close idle sockets between scans, so a
    /// reused connection surfaces spurious "error sending request" failures.
    ///
    /// The test runs a raw HTTP/1.1 server that keeps connections alive (so a
    /// pooling client *would* reuse them) and counts accepted connections. Two
    /// sequential requests through a `build_client` client must therefore open
    /// two separate connections. If the idle pool were re-enabled the second
    /// request would reuse the first socket and the count would drop to 1.
    #[tokio::test]
    async fn test_client_does_not_reuse_keepalive_connections() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::{TcpListener, TcpStream};

        // Answer minimal keep-alive HTTP/1.1 responses, looping so a reused
        // connection is served on the same accepted socket (no new accept).
        async fn serve(mut stream: TcpStream) {
            let mut buf = [0u8; 1024];
            loop {
                // Break when the client closes its side, i.e. it did not pool
                // the connection for reuse.
                if matches!(stream.read(&mut buf).await, Ok(0) | Err(_)) {
                    break;
                }
                let response =
                    b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: keep-alive\r\n\r\n";
                if stream.write_all(response).await.is_err() {
                    break;
                }
            }
        }

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("failed to bind test listener");
        let addr = listener.local_addr().expect("failed to read local addr");

        let connections = Arc::new(AtomicUsize::new(0));
        let accept_connections = connections.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                accept_connections.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(serve(stream));
            }
        });

        let yaml = format!(
            "---\nservices:\n  probe:\n    url: http://{addr}/\n    every: 30s\n    expect:\n      status: 200\n"
        );
        let config = create_config(&yaml);
        let service = config.services.get("probe").expect("probe service missing");

        let (builder, _client_config) = build_client(service).expect("failed to build client");
        let client = builder.build().expect("failed to build client");

        // Two sequential scans: each must establish its own connection.
        for _ in 0..2 {
            let response = client
                .get(format!("http://{addr}/"))
                .send()
                .await
                .expect("request failed");
            assert_eq!(response.status(), 200);
            // Fully consume the body so a pooling client *could* keep the
            // connection alive for reuse; our client must instead close it.
            let body = response.bytes().await.expect("failed to read body");
            assert!(body.is_empty());
            // Give the client a moment to return/close the connection.
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        assert_eq!(
            connections.load(Ordering::SeqCst),
            2,
            "each scan must open a fresh connection; a reused keep-alive pool would yield 1"
        );
    }
}
