use crate::cli::{
    actions::metrics::ServiceMetrics,
    config::{BodyType, ServiceDetails},
};
use anyhow::{anyhow, Result};
use regex::Regex;
use reqwest::{Client, Method, RequestBuilder};
use tokio::process::Command;
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
        .service_status
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
        if let Some(if_not) = &service_details.expect.if_not {
            let exit_code = execute_fallback_command(&if_not.cmd).await?;
            info!(
                "Executed fallback command for {} with exit code {}",
                service_name, exit_code
            );
        }
    }

    Ok(is_match)
}

/// Generates a regex pattern from the input string
/// If input starts with r", extract the raw regex part (strip r" and ending quote if present)
fn generate_regex_pattern(input: &str) -> Result<Regex> {
    let pattern = if input.starts_with("r\"") {
        let raw_regex = input.trim_start_matches("r\"").trim_end_matches('"');
        raw_regex.to_string()
    } else {
        format!(r".*{}.*", regex::escape(input)) // Escape input to prevent regex injection
    };

    debug!("Generated regex pattern: {}", pattern);

    // Compile and return the regex
    Regex::new(&pattern).map_err(|e| e.into())
}

/// Executes the fallback command
async fn execute_fallback_command(cmd: &str) -> Result<i32> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string());
    let output = Command::new(shell).arg("-c").arg(cmd).output().await?;

    let exit_code = match output.status.code() {
        Some(code) => code,
        None => Err(anyhow!("Process terminated by signal"))?,
    };

    Ok(exit_code)
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
        let pattern = generate_regex_pattern("test").unwrap();
        assert_eq!(pattern.as_str(), r".*test.*");

        let pattern = generate_regex_pattern("r\"test\"").unwrap();
        assert_eq!(pattern.as_str(), "test");

        let pattern = generate_regex_pattern("r\"test").unwrap();
        assert_eq!(pattern.as_str(), "test");

        let pattern = generate_regex_pattern("r\"test\"").unwrap();
        assert_eq!(pattern.as_str(), "test");

        let pattern = generate_regex_pattern("hello world").unwrap();
        assert_eq!(pattern.as_str(), r".*hello world.*");

        let pattern = generate_regex_pattern(r#"r"(?i).*hello.*""#).unwrap();
        assert_eq!(pattern.as_str(), "(?i).*hello.*");
    }

    #[tokio::test]
    async fn test_execute_fallback_command() {
        let exit_code = execute_fallback_command("exit 0").await.unwrap();
        assert_eq!(exit_code, 0);

        let exit_code = execute_fallback_command("exit 1").await.unwrap();
        assert_eq!(exit_code, 1);
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
            stop: None,
            test: None,
            timeout: Duration::from_secs(5),
            url: Some(format!("{}/health", server.url())),
            method: HttpMethod::Get,
            body: None,
        };

        let metrics = Arc::new(ServiceMetrics::new().unwrap());

        let (builder, _client_config) = build_client(&service).unwrap();
        let client = builder.build().unwrap();
        let request = build_http_request(&client, &service).unwrap();
        let response = client.execute(request.build().unwrap()).await.unwrap();

        let rs = handle_http_response("test", &service, response, &metrics).await;

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
            .with_header("content-type", "text/plain")
            .with_header("x-api-key", "1234")
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

        let rs = handle_http_response("test", service, response, &ServiceMetrics::new().unwrap())
            .await
            .unwrap();

        assert!(rs);
    }
}
