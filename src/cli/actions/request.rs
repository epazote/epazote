use crate::cli::{
    actions::{
        execute_fallback_command, execute_fallback_http, metrics::ServiceMetrics,
        should_continue_fallback,
    },
    config::{BodyType, ServiceDetails},
};
use anyhow::{anyhow, Result};
use regex::Regex;
use reqwest::{Client, Method, RequestBuilder};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::Mutex;
use tracing::{debug, error, info};

pub fn build_http_request(
    client: &Client,
    service_details: &ServiceDetails,
) -> Result<RequestBuilder> {
    let url = service_details
        .url
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("No URL provided"))?;

    let method = Method::from_bytes(service_details.method.to_string().as_bytes())?;

    let mut request = client.request(method, url);

    if let Some(body) = &service_details.body {
        debug!("Building HTTP request with body: {:?}", body);

        match body {
            BodyType::Json(json) => {
                request = request.json(json);
            }
            BodyType::Form(form_data) => {
                request = request.form(form_data);
            }
            BodyType::Text(text) => {
                request = request.body(text.clone()); // Handles XML, plain text, etc.
            }
        }
    }

    Ok(request)
}

/// Handles the HTTP response
pub async fn handle_http_response(
    service_name: &str,
    service_details: &ServiceDetails,
    response: reqwest::Response,
    metrics: &ServiceMetrics,
    counters: Arc<Mutex<HashMap<String, usize>>>,
) -> Result<bool> {
    let status = response.status();
    let headers = response.headers().clone();

    let status_matches = status.as_u16() == service_details.expect.status;

    // Check if the response body matches expected criteria
    let body_matches = if let Some(expected_body) = &service_details.expect.body {
        let regex = generate_regex_pattern(expected_body).map_err(|e| {
            error!(
                "Invalid regex pattern in Expect body: {}, Error: {}",
                expected_body, e
            );
            e
        })?;

        response.text().await.map_or_else(
            |e| {
                error!("Failed to read response body: {}", e);
                false
            },
            |body_text| regex.is_match(&body_text),
        )
    } else {
        true
    };

    let is_match = status_matches && body_matches;

    // Update metrics
    // Set service status to OK (1) if both status and body match
    metrics
        .epazote_status
        .with_label_values(&[service_name])
        .set(if is_match { 1 } else { 0 });

    info!(
        service_name = service_name,
        service_url = service_details.url,
        service_status = status.as_u16(),
        expected_status = service_details.expect.status,
        response_headers = ?headers,
        matches = is_match
    );

    if !is_match {
        if let Some(action) = &service_details.expect.if_not {
            if should_continue_fallback(service_name, &counters, action).await {
                if let Some(cmd) = &action.cmd {
                    let exit_code = execute_fallback_command(cmd).await?;
                    info!(
                        "Executed fallback command for {} with exit code {}",
                        service_name, exit_code
                    );
                }

                if let Some(http) = &action.http {
                    let status = execute_fallback_http(http).await?;
                    info!(
                        "Executed fallback HTTP request for {} with status code {}",
                        service_name, status
                    );
                }
            }
        }
    }

    Ok(is_match)
}

// Generates a regex pattern from the input string.
/// - If input starts with `r"`, extract and use it as a raw regex (strip `r"` and trailing `"` if present).
/// - Trims input before processing to remove extra whitespace.
fn generate_regex_pattern(input: &str) -> Result<Regex> {
    let trimmed_input = input.trim();

    if trimmed_input.is_empty() {
        return Err(anyhow!("Input regex pattern cannot be empty"));
    }

    let pattern = trimmed_input.strip_prefix("r\"").map_or_else(
        // Escape the input to prevent regex injection
        || format!(r".*{}.*", regex::escape(trimmed_input)),
        // If prefix exists, strip suffix and use raw regex
        |raw| raw.strip_suffix('"').unwrap_or(raw).to_string(),
    );

    debug!(
        "Generated regex for: {}, pattern: {}",
        trimmed_input, pattern
    );

    Regex::new(&pattern).map_err(|e| {
        debug!("Regex compilation failed: {}", e);
        e.into()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{
        actions::client::build_client,
        config::{Config, Expect, HttpMethod, ServiceDetails},
    };
    use mockito::Server;
    use reqwest::StatusCode;
    use serde_json::json;
    use std::{io::Write, sync::Arc};
    use tokio::time::Duration;

    // Helper to create config from YAML
    fn create_config(yaml: &str) -> Config {
        let mut tmp_file = tempfile::NamedTempFile::new().unwrap();
        tmp_file.write_all(yaml.as_bytes()).unwrap();
        tmp_file.flush().unwrap();
        Config::new(tmp_file.path().to_path_buf()).unwrap()
    }

    #[test]
    fn test_generate_regex_pattern() {
        // Normal input should be escaped and wrapped in ".* .*"
        let pattern = generate_regex_pattern("test").unwrap();
        assert_eq!(pattern.as_str(), r".*test.*");

        // Raw regex should be extracted without modification
        let pattern = generate_regex_pattern(r#"r"test""#).unwrap();
        assert_eq!(pattern.as_str(), "test");

        // Raw regex without closing quote should still work
        let pattern = generate_regex_pattern(r#"r"test"#).unwrap();
        assert_eq!(pattern.as_str(), "test");

        // Raw regex with extra quotes should be handled
        let pattern = generate_regex_pattern(r#"r"(?i).*hello.*""#).unwrap();
        assert_eq!(pattern.as_str(), "(?i).*hello.*");

        // Standard input should be escaped
        let pattern = generate_regex_pattern("hello world").unwrap();
        assert_eq!(pattern.as_str(), r".*hello world.*"); // Space should be escaped
                                                          //
                                                          // Ensure regex matching works
        assert!(pattern.is_match("this is a hello world test"));
        assert!(!pattern.is_match("this is a goodbye test"));

        // the . should be escaped
        let pattern = generate_regex_pattern("hello.world").unwrap();
        assert_eq!(pattern.as_str(), r".*hello\.world.*"); // Space should be escaped
                                                           //
        let pattern = generate_regex_pattern("a+b*").unwrap();
        assert_eq!(pattern.as_str(), r".*a\+b\*.*"); // Space should be escaped
    }

    #[tokio::test]
    async fn test_handle_http_response() {
        let mut server = Server::new_async().await;
        let _m = server
            .mock("GET", "/health")
            .with_status(200)
            .create_async()
            .await;

        let service = ServiceDetails {
            every: Duration::from_secs(1),
            expect: Expect {
                status: 200,
                header: None,
                body: None,
                if_not: None,
            },
            follow_redirects: Some(true),
            headers: None,
            if_header: None,
            if_status: None,
            insecure: None,
            read_limit: None,
            test: None,
            timeout: Duration::from_secs(5),
            url: Some(format!("{}/health", server.url())),
            method: HttpMethod::Get,
            body: None,
        };

        let metrics = Arc::new(ServiceMetrics::new().unwrap());
        let counters: Arc<Mutex<HashMap<String, usize>>> = Arc::new(Mutex::new(HashMap::new()));

        let (builder, _client_config) = build_client(&service).unwrap();
        let client = builder.build().unwrap();
        let request = build_http_request(&client, &service).unwrap();
        let response = client.execute(request.build().unwrap()).await.unwrap();

        let rs = handle_http_response("test", &service, response, &metrics, counters).await;

        assert!(rs.is_ok());
    }

    #[tokio::test]
    async fn test_build_http_request_json() {
        // Start mock server
        let mut server = Server::new_async().await;
        let mock_url = server.url();

        let yaml = format!(
            r#"
---
services:
  test:
    url: {}/test
    method: POST
    body:
      json:
        key: value
        oi: hola
    every: 30s
    headers:
      X-Custom-Header: TestValue
    expect:
      status: 200
    "#,
            mock_url
        );

        let config = create_config(&yaml);
        let service = config.services.get("test").unwrap();

        // Define expected JSON body
        let expected_json = json!({
            "key": "value",
            "oi": "hola"
        });

        let _ = env_logger::try_init();
        let _mock = server
            .mock("POST", "/test")
            .match_header("X-Custom-Header", "TestValue")
            .match_header("Content-Type", "application/json")
            .match_header(
                "User-Agent",
                mockito::Matcher::Regex("epazote.*".to_string()),
            )
            .match_body(mockito::Matcher::Json(expected_json.clone()))
            .with_status(200)
            .create_async()
            .await;

        let (builder, _client_config) = build_client(service).unwrap();
        let client = builder.build().unwrap();
        let request = build_http_request(&client, service).unwrap();

        if let Some(body) = &config.services.get("test").unwrap().body {
            let json_body = serde_json::to_string(body).unwrap();
            assert_eq!(json_body, expected_json.to_string());
        }

        let response = client.execute(request.build().unwrap()).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_build_http_request_form() {
        // Start mock server
        let mut server = Server::new_async().await;
        let mock_url = server.url();

        let yaml = format!(
            r#"
---
services:
  test:
    url: {}/test
    method: POST
    body:
      form:
        key: value
        oi: hola
    every: 30s
    headers:
      X-Custom-Header: TestValue
    expect:
      status: 200
    "#,
            mock_url
        );

        let config = create_config(&yaml);
        let service = config.services.get("test").unwrap();

        // Define expected form body
        let expected_form = vec![
            ("key".to_string(), "value".to_string()),
            ("oi".to_string(), "hola".to_string()),
        ];

        let _ = env_logger::try_init();
        let _mock = server
            .mock("POST", "/test")
            .match_header("X-Custom-Header", "TestValue")
            .match_header("Content-Type", "application/x-www-form-urlencoded")
            .match_header(
                "User-Agent",
                mockito::Matcher::Regex("epazote.*".to_string()),
            )
            .match_body(mockito::Matcher::UrlEncoded(
                "key".to_string(),
                "value".to_string(),
            ))
            .match_body(mockito::Matcher::UrlEncoded(
                "oi".to_string(),
                "hola".to_string(),
            ))
            .with_status(200)
            .create_async()
            .await;

        let (builder, _client_config) = build_client(service).unwrap();
        let client = builder.build().unwrap();
        let request = build_http_request(&client, service).unwrap();

        // Check that the body is correctly interpreted as a form
        if let Some(BodyType::Form(body)) = &config.services.get("test").unwrap().body {
            for (key, value) in expected_form.iter() {
                assert_eq!(body.get(key), Some(value));
            }
        } else {
            panic!("Expected BodyType::Form but found something else");
        }

        let response = client.execute(request.build().unwrap()).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_build_http_request_text() {
        // Start mock server
        let mut server = Server::new_async().await;
        let mock_url = server.url();

        let yaml = format!(
            r#"
---
services:
  test:
    url: {}/test
    method: POST
    body: "Hello, world!"
    every: 30s
    headers:
      content-type: text/plain
      X-Custom-Header: TestValue
    expect:
      status: 200
    "#,
            mock_url
        );

        let config = create_config(&yaml);
        let service = config.services.get("test").unwrap();

        // Expected plain text body
        let expected_text = String::from("Hello, world!");

        let _ = env_logger::try_init();
        let _mock = server
            .mock("POST", "/test")
            .match_header("X-Custom-Header", "TestValue")
            .match_header("Content-Type", "text/plain")
            .match_header(
                "User-Agent",
                mockito::Matcher::Regex("epazote.*".to_string()),
            )
            .match_body(mockito::Matcher::Exact(expected_text.clone()))
            .with_status(200)
            .create_async()
            .await;

        let (builder, _client_config) = build_client(service).unwrap();
        let client = builder.build().unwrap();
        let request = build_http_request(&client, service).unwrap();

        // Check that the body is correctly interpreted as Text
        if let Some(BodyType::Text(body)) = &config.services.get("test").unwrap().body {
            assert_eq!(body, &expected_text);
        } else {
            panic!("Expected BodyType::Text but found something else");
        }

        let response = client.execute(request.build().unwrap()).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_handle_http_response_expect_body() {
        // Start mock server
        let mut server = Server::new_async().await;
        let mock_url = server.url();

        let yaml = format!(
            r#"
---
services:
  test:
    url: {}/test
    every: 30s
    expect:
      status: 200
      body: sopas
    "#,
            mock_url
        );

        let config = create_config(&yaml);
        let service = config.services.get("test").unwrap();

        let _ = env_logger::try_init();
        let _mock = server
            .mock("GET", "/test")
            .with_body("world-sopas-hello")
            .match_header(
                "User-Agent",
                mockito::Matcher::Regex("epazote.*".to_string()),
            )
            .with_status(200)
            .create_async()
            .await;

        let (builder, _client_config) = build_client(service).unwrap();
        let client = builder.build().unwrap();
        let request = build_http_request(&client, service).unwrap();

        let response = client.execute(request.build().unwrap()).await.unwrap();

        let counters: Arc<Mutex<HashMap<String, usize>>> = Arc::new(Mutex::new(HashMap::new()));

        let rs = handle_http_response(
            "test",
            service,
            response,
            &ServiceMetrics::new().unwrap(),
            counters,
        )
        .await
        .unwrap();

        assert!(rs);
    }

    #[tokio::test]
    async fn test_handle_http_response_expect_body_regex_stop() {
        // Start mock server
        let mut server = Server::new_async().await;
        let mock_url = server.url();

        let yaml = format!(
            r#"
---
services:
  test-stop:
    url: {}/test
    every: 30s
    expect:
      status: 200
      body: r"\b(?:sopas|cit-02)\b" # match sopas or cit-02
      if_not:
        stop: 2
    "#,
            mock_url
        );

        let config = create_config(&yaml);
        let service = config.services.get("test-stop").unwrap();

        let _ = env_logger::try_init();
        let _mock = server
            .mock("GET", "/test")
            .with_body("---")
            .match_header(
                "User-Agent",
                mockito::Matcher::Regex("epazote.*".to_string()),
            )
            .with_status(200)
            .create_async()
            .await;

        let (builder, _client_config) = build_client(service).unwrap();
        let client = builder.build().unwrap();
        let request = build_http_request(&client, service).unwrap();
        let response = client.execute(request.build().unwrap()).await.unwrap();

        let counters: Arc<Mutex<HashMap<String, usize>>> = Arc::new(Mutex::new(HashMap::new()));

        let rs1 = handle_http_response(
            "test-stop",
            service,
            response,
            &ServiceMetrics::new().unwrap(),
            Arc::clone(&counters),
        )
        .await
        .unwrap();

        assert!(!rs1);

        // Check counter after first attempt
        let count1 = {
            let counters_locked = counters.lock().await;
            *counters_locked.get("test-stop").unwrap_or(&0)
        };
        assert_eq!(count1, 1, "Counter should be 1 after first attempt");

        let request = build_http_request(&client, service).unwrap();
        let response = client.execute(request.build().unwrap()).await.unwrap();

        let rs2 = handle_http_response(
            "test-stop",
            service,
            response,
            &ServiceMetrics::new().unwrap(),
            Arc::clone(&counters),
        )
        .await
        .unwrap();

        assert!(!rs2);

        // Check counter after first attempt
        let count2 = {
            let counters_locked = counters.lock().await;
            *counters_locked.get("test-stop").unwrap_or(&0)
        };
        assert_eq!(count2, 2, "Counter should be 1 after first attempt");
    }

    #[tokio::test]
    async fn test_handle_http_response_expect_if_not_http() {
        // Start mock server
        let mut server = Server::new_async().await;
        let mock_url = server.url();

        let yaml = format!(
            r#"
---
services:
  test-stop:
    url: {}/test
    every: 30s
    expect:
      status: 200
      body: http
      if_not:
        stop: 2
        http: {}/notify?milei=libra
    "#,
            mock_url, mock_url
        );

        let config = create_config(&yaml);
        let service = config.services.get("test-stop").unwrap();

        let _ = env_logger::try_init();
        let _mock = server
            .mock("GET", "/test")
            .with_body("---milei---")
            .match_header(
                "User-Agent",
                mockito::Matcher::Regex("epazote.*".to_string()),
            )
            .with_status(200)
            .create_async()
            .await;

        let (builder, _client_config) = build_client(service).unwrap();
        let client = builder.build().unwrap();
        let request = build_http_request(&client, service).unwrap();
        let response = client.execute(request.build().unwrap()).await.unwrap();

        let counters: Arc<Mutex<HashMap<String, usize>>> = Arc::new(Mutex::new(HashMap::new()));

        let rs1 = handle_http_response(
            "test-stop",
            service,
            response,
            &ServiceMetrics::new().unwrap(),
            Arc::clone(&counters),
        )
        .await
        .unwrap();

        assert!(!rs1);

        // Check counter after first attempt
        let count1 = {
            let counters_locked = counters.lock().await;
            *counters_locked.get("test-stop").unwrap_or(&0)
        };
        assert_eq!(count1, 1, "Counter should be 1 after first attempt");

        let request = build_http_request(&client, service).unwrap();
        let response = client.execute(request.build().unwrap()).await.unwrap();

        let rs2 = handle_http_response(
            "test-stop",
            service,
            response,
            &ServiceMetrics::new().unwrap(),
            Arc::clone(&counters),
        )
        .await
        .unwrap();

        assert!(!rs2);

        // Check counter after first attempt
        let count2 = {
            let counters_locked = counters.lock().await;
            *counters_locked.get("test-stop").unwrap_or(&0)
        };
        assert_eq!(count2, 2, "Counter should be 1 after first attempt");
    }
}
