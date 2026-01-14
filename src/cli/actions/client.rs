use crate::cli::config::ServiceDetails;
use anyhow::Result;
use reqwest::{
    Client, ClientBuilder,
    header::{HeaderMap, HeaderName, HeaderValue},
};

pub static APP_USER_AGENT: &str =
    concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"), ")");

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

    let mut builder = Client::builder().timeout(timeout).user_agent(&user_agent);

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
}
