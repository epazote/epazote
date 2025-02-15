use crate::cli::config::ServiceDetails;
use anyhow::Result;
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue},
    Client, ClientBuilder,
};

static APP_USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"), ")");

#[derive(Debug)]
pub struct ClientConfig {
    pub timeout: std::time::Duration,
    pub user_agent: String,
    pub follow_redirects: bool,
    pub headers: HeaderMap,
}

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
            let header_name = HeaderName::from_bytes(key.as_bytes()).expect("Invalid header name");
            let header_value = HeaderValue::from_str(value).expect("Invalid header value");

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
mod tests {
    use super::*;
    use crate::cli::config::Config;
    use mockito::Server;
    use std::io::Write;

    // Helper to create config from YAML
    fn create_config(yaml: &str) -> Config {
        let mut tmp_file = tempfile::NamedTempFile::new().unwrap();
        tmp_file.write_all(yaml.as_bytes()).unwrap();
        tmp_file.flush().unwrap();
        Config::new(tmp_file.path().to_path_buf()).unwrap()
    }

    #[tokio::test]
    async fn test_build_client_multiple_services() {
        let yaml = r#"
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
    "#;

        let mut server = Server::new_async().await;

        let expected_headers = vec![
            (
                "test",
                vec![
                    ("X-Custom-Header", "TestValue"),
                    ("User-Agent", APP_USER_AGENT),
                ],
            ),
            ("test2", vec![("User-Agent", "TestAgent")]),
        ];

        for (service_name, headers) in &expected_headers {
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
            let service = config.services.get(*service_name).unwrap();

            let (builder, _client_config) = build_client(service).unwrap();

            let client = builder.build().unwrap();
            let url = format!("{}/{}", server.url(), service_name);
            let response = client.get(url).send().await.unwrap();

            assert_eq!(response.status(), 200);
        }
    }
}
