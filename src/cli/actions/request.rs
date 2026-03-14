use crate::cli::{
    actions::{
        FallbackContext, FallbackServiceType, FallbackState, execute_fallback_command,
        execute_fallback_http, get_fallback_state, metrics::ServiceMetrics, reset_fallback_state,
        should_continue_fallback,
    },
    config::{BodyType, ServiceDetails},
    telemetry,
};
use anyhow::{Result, anyhow};
use futures_util::StreamExt;
use regex::Regex;
use reqwest::{
    Client, Method, RequestBuilder,
    header::{HeaderMap, HeaderValue},
};
use serde_json::Value;
use std::{collections::HashMap, fmt::Write as _, sync::Arc};
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use std::hash::BuildHasher;

fn format_headers(headers: &HeaderMap<HeaderValue>) -> String {
    if headers.is_empty() {
        return "(none)".to_string();
    }

    let mut output = String::new();

    for (name, value) in headers {
        let value = value.to_str().unwrap_or("<non-utf8>");
        let _ = write!(output, "\n  {}: {}", name.as_str(), value);
    }

    output
}

fn format_headers_block(headers: &HeaderMap<HeaderValue>) -> String {
    if headers.is_empty() {
        "\n  (none)".to_string()
    } else {
        format_headers(headers)
    }
}

fn format_http_response_success_log(
    service_name: &str,
    service_url: Option<&String>,
    service_status: u16,
    expected_status: u16,
    matches: bool,
) -> String {
    let service_url = service_url.map_or("(none)", String::as_str);

    format!(
        "service_name: \"{service_name}\", service_url: \"{service_url}\", service_status: {service_status}, expected_status: {expected_status}, matches: {matches}"
    )
}

fn format_http_response_failure_log(
    service_name: &str,
    service_url: Option<&String>,
    service_status: u16,
    expected_status: u16,
    headers: &HeaderMap<HeaderValue>,
    matches: bool,
) -> String {
    let service_url = service_url.map_or("(none)", String::as_str);

    format!(
        "service_name: \"{service_name}\", service_url: \"{service_url}\", service_status: {service_status}, expected_status: {expected_status}\nresponse_headers:{}\nmatches: {matches}",
        format_headers_block(headers)
    )
}

/// Builds a `reqwest::RequestBuilder` from the service details.
///
/// # Errors
///
/// Returns an error if the URL is missing, the method is invalid, or the request cannot be built.
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
///
/// # Errors
///
/// Returns an error if the fallback command or HTTP request fails.
#[allow(clippy::too_many_lines)]
pub async fn handle_http_response<S: BuildHasher>(
    service_name: &str,
    service_details: &ServiceDetails,
    response: reqwest::Response,
    metrics: &ServiceMetrics,
    counters: Arc<Mutex<HashMap<String, FallbackState, S>>>,
) -> Result<bool> {
    let status = response.status();
    let headers = response.headers().clone();
    let actual_status = i32::from(status.as_u16());

    // Check if the response status matches expected status
    let status_matches = status.as_u16() == service_details.expect.status;

    // Check if the response body matches expected criteria
    let body_mismatch_reason = if let Some(expected_body) = &service_details.expect.body {
        if match_response_body(response, expected_body, service_details.max_bytes).await? {
            None
        } else {
            Some("body_mismatch")
        }
    } else if let Some(expected_json) = &service_details.expect.json {
        if match_response_json(response, expected_json, service_details.max_bytes).await? {
            None
        } else {
            Some("json_mismatch")
        }
    } else {
        None
    };
    let body_matches = body_mismatch_reason.is_none();

    let is_match = status_matches && body_matches;

    if is_match {
        reset_fallback_state(service_name, &counters).await;
    }

    // Update metrics
    // Set service status to OK (1) if both status and body match
    metrics
        .epazote_status
        .with_label_values(&[service_name])
        .set(i64::from(is_match));

    if telemetry::pretty_logs_enabled() {
        let formatted = if is_match {
            format_http_response_success_log(
                service_name,
                service_details.url.as_ref(),
                status.as_u16(),
                service_details.expect.status,
                is_match,
            )
        } else {
            format_http_response_failure_log(
                service_name,
                service_details.url.as_ref(),
                status.as_u16(),
                service_details.expect.status,
                &headers,
                is_match,
            )
        };

        if is_match {
            info!("{formatted}");
        } else {
            warn!("{formatted}");
        }
    } else if is_match {
        info!(
            service_name = service_name,
            service_url = service_details.url,
            service_status = status.as_u16(),
            expected_status = service_details.expect.status,
            response_headers = %format_headers(&headers),
            matches = is_match
        );
    } else {
        warn!(
            service_name = service_name,
            service_url = service_details.url,
            service_status = status.as_u16(),
            expected_status = service_details.expect.status,
            response_headers = %format_headers(&headers),
            matches = is_match
        );
    }

    if !is_match
        && let Some(action) = &service_details.expect.if_not
        && should_continue_fallback(service_name, &counters, action).await
    {
        let state = get_fallback_state(service_name, &counters)
            .await
            .unwrap_or_default();
        let context = FallbackContext {
            service_name,
            service_type: FallbackServiceType::Http,
            expected_status: i32::from(service_details.expect.status),
            actual_status: Some(actual_status),
            error: if status_matches {
                body_mismatch_reason.unwrap_or("request_error")
            } else {
                "status_mismatch"
            },
            failure_count: state.consecutive_failures,
            threshold: action.threshold.unwrap_or(1),
            url: service_details.url.as_deref(),
            test: None,
        };

        if let Some(cmd) = &action.cmd {
            let exit_code = execute_fallback_command(cmd, &context).await?;
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

    Ok(is_match)
}

async fn match_response_body(
    response: reqwest::Response,
    expected_body: &str,
    max_bytes: Option<usize>,
) -> Result<bool> {
    let regex = generate_regex_pattern(expected_body).map_err(|e| {
        error!(
            "Invalid regex pattern in Expect body: {}, Error: {}",
            expected_body, e
        );
        e
    })?;

    let mut buffer = String::new();
    let mut stream = response.bytes_stream();
    let max_bytes = max_bytes.unwrap_or(usize::MAX);
    let mut total_bytes_read = 0;

    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(bytes) => {
                let chunk_size = bytes.len();
                let remaining_bytes = max_bytes.saturating_sub(total_bytes_read);

                debug!(
                    "Read chunk of size: {}, remaining bytes: {}, total bytes read: {}",
                    chunk_size, remaining_bytes, total_bytes_read
                );

                if remaining_bytes == 0 {
                    break;
                }

                let limited_chunk = if chunk_size > remaining_bytes {
                    bytes.get(..remaining_bytes).unwrap_or(&bytes)
                } else {
                    &bytes
                };

                if let Ok(text) = std::str::from_utf8(limited_chunk) {
                    buffer.push_str(text);
                }

                total_bytes_read += limited_chunk.len();

                if regex.is_match(&buffer) {
                    debug!(
                        "Match found in response body: {}, total bytes read: {}",
                        buffer, total_bytes_read
                    );
                    return Ok(true);
                }

                if total_bytes_read >= max_bytes {
                    debug!(
                        "Max bytes limit reached: {}, total bytes read: {}, body: {}",
                        max_bytes, total_bytes_read, buffer
                    );
                    break;
                }
            }
            Err(e) => {
                error!("Failed to read chunk: {}", e);
                return Ok(false);
            }
        }
    }

    Ok(false)
}

async fn match_response_json(
    response: reqwest::Response,
    expected_json: &Value,
    max_bytes: Option<usize>,
) -> Result<bool> {
    let body = collect_response_bytes(response, max_bytes).await?;

    match serde_json::from_slice::<Value>(&body) {
        Ok(actual_json) => Ok(json_contains(expected_json, &actual_json)),
        Err(e) => {
            error!("Failed to parse response body as JSON: {}", e);
            Ok(false)
        }
    }
}

async fn collect_response_bytes(
    response: reqwest::Response,
    max_bytes: Option<usize>,
) -> Result<Vec<u8>> {
    let mut stream = response.bytes_stream();
    let max_bytes = max_bytes.unwrap_or(usize::MAX);
    let mut total_bytes_read = 0;
    let mut buffer = Vec::new();

    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(bytes) => {
                let remaining_bytes = max_bytes.saturating_sub(total_bytes_read);

                if remaining_bytes == 0 {
                    break;
                }

                let limited_chunk = if bytes.len() > remaining_bytes {
                    bytes.get(..remaining_bytes).unwrap_or(&bytes)
                } else {
                    &bytes
                };

                buffer.extend_from_slice(limited_chunk);
                total_bytes_read += limited_chunk.len();

                if total_bytes_read >= max_bytes {
                    break;
                }
            }
            Err(e) => {
                error!("Failed to read chunk: {}", e);
                return Ok(Vec::new());
            }
        }
    }

    Ok(buffer)
}

fn json_contains(expected: &Value, actual: &Value) -> bool {
    match (expected, actual) {
        (Value::Object(expected_map), Value::Object(actual_map)) => {
            expected_map.iter().all(|(key, expected_value)| {
                actual_map
                    .get(key)
                    .is_some_and(|actual_value| json_contains(expected_value, actual_value))
            })
        }
        (Value::Array(expected_items), Value::Array(actual_items)) => {
            expected_items.iter().all(|expected_item| {
                actual_items
                    .iter()
                    .any(|actual_item| json_contains(expected_item, actual_item))
            })
        }
        _ => expected == actual,
    }
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
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::cli::{
        actions::{FallbackState, client::build_client},
        config::{Config, Expect, HttpMethod, ServiceDetails},
    };
    use mockito::Server;
    use reqwest::StatusCode;
    use serde_json::json;
    use std::{fs, io::Write, os::unix::fs::PermissionsExt, path::PathBuf, sync::Arc};
    use tokio::time::Duration;

    // Helper to create config from YAML
    fn create_config(yaml: &str) -> Config {
        let mut tmp_file = tempfile::NamedTempFile::new().expect("Failed to create temp file");
        tmp_file
            .write_all(yaml.as_bytes())
            .expect("Failed to write to temp file");
        tmp_file.flush().expect("Failed to flush temp file");
        Config::new(tmp_file.path().to_path_buf()).expect("Failed to load config")
    }

    // helper to generate a string of numbers
    fn generate_numbers(limit: usize, start: usize) -> String {
        use std::fmt::Write;
        let mut result = String::new();
        let mut num = start;
        while result.len() + 2 < limit {
            // Approximate space for "N "
            let _ = write!(result, "{num} ");
            num += 1;
        }
        result
    }

    fn create_env_capture_script(env_vars: &[&str]) -> (tempfile::TempDir, String, PathBuf) {
        let tempdir = tempfile::Builder::new()
            .prefix("epazote-http-env-")
            .tempdir_in(".")
            .expect("Failed to create temp dir");
        let script_path = tempdir.path().join("capture.sh");
        let output_path = tempdir.path().join("output.txt");
        let body = env_vars
            .iter()
            .map(|key| format!("printenv {key}"))
            .collect::<Vec<_>>()
            .join("\n");

        fs::write(
            &script_path,
            format!("#!/bin/sh\n{{\n{body}\n}} > {}\n", output_path.display()),
        )
        .expect("Failed to write capture script");

        let mut permissions = fs::metadata(&script_path)
            .expect("Failed to stat script")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).expect("Failed to chmod script");

        (
            tempdir,
            script_path
                .to_str()
                .expect("Invalid script path")
                .to_string(),
            output_path,
        )
    }

    #[test]
    fn test_generate_regex_pattern() {
        // Normal input should be escaped and wrapped in ".* .*"
        let pattern = generate_regex_pattern("test").expect("Failed to generate regex pattern");
        assert_eq!(pattern.as_str(), r".*test.*");

        // Raw regex should be extracted without modification
        let pattern =
            generate_regex_pattern(r#"r"test""#).expect("Failed to generate regex pattern");
        assert_eq!(pattern.as_str(), "test");

        // Raw regex without closing quote should still work
        let pattern =
            generate_regex_pattern(r#"r"test"#).expect("Failed to generate regex pattern");
        assert_eq!(pattern.as_str(), "test");

        // Raw regex with extra quotes should be handled
        let pattern = generate_regex_pattern(r#"r"(?i).*hello.*""#)
            .expect("Failed to generate regex pattern");
        assert_eq!(pattern.as_str(), "(?i).*hello.*");

        // Standard input should be escaped
        let pattern =
            generate_regex_pattern("hello world").expect("Failed to generate regex pattern");
        assert_eq!(pattern.as_str(), r".*hello world.*"); // Space should be escaped
        //
        // Ensure regex matching works
        assert!(pattern.is_match("this is a hello world test"));
        assert!(!pattern.is_match("this is a goodbye test"));

        // the . should be escaped
        let pattern =
            generate_regex_pattern("hello.world").expect("Failed to generate regex pattern");
        assert_eq!(pattern.as_str(), r".*hello\.world.*"); // Space should be escaped
        //
        let pattern = generate_regex_pattern("a+b*").expect("Failed to generate regex pattern");
        assert_eq!(pattern.as_str(), r".*a\+b\*.*"); // Space should be escaped
    }

    #[test]
    fn test_format_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", HeaderValue::from_static("text/html"));
        headers.insert(
            "location",
            HeaderValue::from_static("https://www.google.com/"),
        );

        let formatted = format_headers(&headers);

        assert!(formatted.contains("\n  content-type: text/html"));
        assert!(formatted.contains("\n  location: https://www.google.com/"));
    }

    #[test]
    fn test_format_http_response_log() {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", HeaderValue::from_static("text/html"));
        headers.insert(
            "location",
            HeaderValue::from_static("https://www.google.com/"),
        );

        let formatted = format_http_response_failure_log(
            "google",
            Some(&"https://google.com".to_string()),
            301,
            301,
            &headers,
            true,
        );

        assert!(formatted.contains(
            "service_name: \"google\", service_url: \"https://google.com\", service_status: 301, expected_status: 301\nresponse_headers:"
        ));
        assert!(formatted.contains("\n  content-type: text/html"));
        assert!(formatted.contains("\n  location: https://www.google.com/"));
        assert!(formatted.ends_with("\nmatches: true"));
    }

    #[test]
    fn test_format_http_response_success_log() {
        let formatted = format_http_response_success_log(
            "google",
            Some(&"https://google.com".to_string()),
            301,
            301,
            true,
        );

        assert_eq!(
            formatted,
            "service_name: \"google\", service_url: \"https://google.com\", service_status: 301, expected_status: 301, matches: true"
        );
    }

    #[test]
    fn test_json_contains_nested_objects_and_arrays() {
        let expected = json!({
            "status": "success",
            "data": {
                "activeTargets": [
                    {
                        "labels": {
                            "job": "DBMI-lab-nico"
                        },
                        "health": "up"
                    }
                ]
            }
        });

        let actual = json!({
            "status": "success",
            "data": {
                "activeTargets": [
                    {
                        "labels": {
                            "instance": "127.0.0.1:8429",
                            "job": "DBMI-lab-nico"
                        },
                        "health": "up",
                        "lastSamplesScraped": 932
                    },
                    {
                        "labels": {
                            "instance": "127.0.0.1:9080",
                            "job": "other"
                        },
                        "health": "down"
                    }
                ],
                "droppedTargets": []
            }
        });

        assert!(json_contains(&expected, &actual));
    }

    #[test]
    fn test_json_contains_returns_false_for_missing_nested_match() {
        let expected = json!({
            "data": {
                "activeTargets": [
                    {
                        "labels": {
                            "job": "DBMI-lab-nico"
                        },
                        "health": "down"
                    }
                ]
            }
        });

        let actual = json!({
            "data": {
                "activeTargets": [
                    {
                        "labels": {
                            "job": "DBMI-lab-nico"
                        },
                        "health": "up"
                    }
                ]
            }
        });

        assert!(!json_contains(&expected, &actual));
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
                json: None,
                if_not: None,
            },
            follow_redirects: Some(true),
            headers: None,
            max_bytes: None,
            test: None,
            timeout: Duration::from_secs(5),
            url: Some(format!("{}/health", server.url())),
            method: HttpMethod::Get,
            body: None,
        };

        let metrics = Arc::new(ServiceMetrics::new().expect("Failed to create metrics"));
        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let (builder, _client_config) =
            build_client(&service).expect("Failed to build client builder");
        let client = builder.build().expect("Failed to build client");
        let request = build_http_request(&client, &service).expect("Failed to build request");
        let response = client
            .execute(request.build().expect("Failed to build request"))
            .await
            .expect("Failed to execute request");

        let rs = handle_http_response("test", &service, response, &metrics, counters).await;

        assert!(rs.is_ok());
    }

    #[tokio::test]
    async fn test_build_http_request_json() {
        // Start mock server
        let mut server = Server::new_async().await;
        let mock_url = server.url();

        let yaml = format!(
            r"
---
services:
  test:
    url: {mock_url}/test
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
    "
        );

        let config = create_config(&yaml);
        let service = config.services.get("test").expect("Service not found");

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

        let (builder, _client_config) =
            build_client(service).expect("Failed to build client builder");
        let client = builder.build().expect("Failed to build client");
        let request = build_http_request(&client, service).expect("Failed to build request");

        if let Some(body) = &config.services.get("test").expect("Service not found").body {
            let json_body = serde_json::to_string(body).expect("Failed to serialize body");
            assert_eq!(json_body, expected_json.to_string());
        }

        let response = client
            .execute(request.build().expect("Failed to build request"))
            .await
            .expect("Failed to execute request");

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_build_http_request_form() {
        // Start mock server
        let mut server = Server::new_async().await;
        let mock_url = server.url();

        let yaml = format!(
            r"
---
services:
  test:
    url: {mock_url}/test
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
    "
        );

        let config = create_config(&yaml);
        let service = config.services.get("test").expect("Service not found");

        // Define expected form body
        let expected_form = [
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

        let (builder, _client_config) =
            build_client(service).expect("Failed to build client builder");
        let client = builder.build().expect("Failed to build client");
        let request = build_http_request(&client, service).expect("Failed to build request");

        // Check that the body is correctly interpreted as a form
        if let Some(BodyType::Form(body)) =
            &config.services.get("test").expect("Service not found").body
        {
            for (key, value) in &expected_form {
                assert_eq!(body.get(key), Some(value));
            }
        } else {
            panic!("Expected BodyType::Form but found something else");
        }

        let response = client
            .execute(request.build().expect("Failed to build request"))
            .await
            .expect("Failed to execute request");

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
    url: {mock_url}/test
    method: POST
    body: "Hello, world!"
    every: 30s
    headers:
      content-type: text/plain
      X-Custom-Header: TestValue
    expect:
      status: 200
    "#
        );

        let config = create_config(&yaml);
        let service = config.services.get("test").expect("Service not found");

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

        let (builder, _client_config) =
            build_client(service).expect("Failed to build client builder");
        let client = builder.build().expect("Failed to build client");
        let request = build_http_request(&client, service).expect("Failed to build request");

        // Check that the body is correctly interpreted as Text
        if let Some(BodyType::Text(body)) =
            &config.services.get("test").expect("Service not found").body
        {
            assert_eq!(body, &expected_text);
        } else {
            panic!("Expected BodyType::Text but found something else");
        }

        let response = client
            .execute(request.build().expect("Failed to build request"))
            .await
            .expect("Failed to execute request");

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_handle_http_response_expect_body() {
        // Start mock server
        let mut server = Server::new_async().await;
        let mock_url = server.url();

        let yaml = format!(
            r"
---
services:
  test:
    url: {mock_url}/test
    every: 30s
    expect:
      status: 200
      body: sopas
    "
        );

        let config = create_config(&yaml);
        let service = config.services.get("test").expect("Service not found");

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

        let (builder, _client_config) =
            build_client(service).expect("Failed to build client builder");
        let client = builder.build().expect("Failed to build client");
        let request = build_http_request(&client, service).expect("Failed to build request");

        let response = client
            .execute(request.build().expect("Failed to build request"))
            .await
            .expect("Failed to execute request");

        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let rs = handle_http_response(
            "test",
            service,
            response,
            &ServiceMetrics::new().expect("Failed to create metrics"),
            counters,
        )
        .await
        .expect("Failed to handle response");

        assert!(rs);
    }

    #[tokio::test]
    async fn test_handle_http_response_expect_json() {
        let mut server = Server::new_async().await;
        let mock_url = server.url();

        let yaml = format!(
            r"
---
services:
  test:
    url: {mock_url}/test
    every: 30s
    expect:
      status: 200
      json:
        status: success
        data:
          activeTargets:
            - labels:
                job: DBMI-lab-nico
              health: up
    "
        );

        let config = create_config(&yaml);
        let service = config.services.get("test").expect("Service not found");

        let _ = env_logger::try_init();
        let _mock = server
            .mock("GET", "/test")
            .with_body(
                r#"{"status":"success","data":{"activeTargets":[{"labels":{"instance":"127.0.0.1:8429","job":"DBMI-lab-nico"},"health":"up","lastSamplesScraped":932},{"labels":{"instance":"127.0.0.1:9080","job":"other"},"health":"down"}],"droppedTargets":[]}}"#,
            )
            .match_header(
                "User-Agent",
                mockito::Matcher::Regex("epazote.*".to_string()),
            )
            .with_status(200)
            .create_async()
            .await;

        let (builder, _client_config) =
            build_client(service).expect("Failed to build client builder");
        let client = builder.build().expect("Failed to build client");
        let request = build_http_request(&client, service).expect("Failed to build request");

        let response = client
            .execute(request.build().expect("Failed to build request"))
            .await
            .expect("Failed to execute request");

        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let rs = handle_http_response(
            "test",
            service,
            response,
            &ServiceMetrics::new().expect("Failed to create metrics"),
            counters,
        )
        .await
        .expect("Failed to handle response");

        assert!(rs);
    }

    #[tokio::test]
    async fn test_handle_http_response_expect_json_invalid_body() {
        let mut server = Server::new_async().await;
        let mock_url = server.url();

        let yaml = format!(
            r"
---
services:
  test:
    url: {mock_url}/test
    every: 30s
    expect:
      status: 200
      json:
        status: success
    "
        );

        let config = create_config(&yaml);
        let service = config.services.get("test").expect("Service not found");

        let _ = env_logger::try_init();
        let _mock = server
            .mock("GET", "/test")
            .with_body("not-json")
            .match_header(
                "User-Agent",
                mockito::Matcher::Regex("epazote.*".to_string()),
            )
            .with_status(200)
            .create_async()
            .await;

        let (builder, _client_config) =
            build_client(service).expect("Failed to build client builder");
        let client = builder.build().expect("Failed to build client");
        let request = build_http_request(&client, service).expect("Failed to build request");

        let response = client
            .execute(request.build().expect("Failed to build request"))
            .await
            .expect("Failed to execute request");

        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let rs = handle_http_response(
            "test",
            service,
            response,
            &ServiceMetrics::new().expect("Failed to create metrics"),
            counters,
        )
        .await
        .expect("Failed to handle response");

        assert!(!rs);
    }

    #[tokio::test]
    async fn test_handle_http_response_if_not_cmd_sets_http_env_vars() {
        let mut server = Server::new_async().await;
        let mock_url = server.url();
        let (_tempdir, script_path, output_path) = create_env_capture_script(&[
            "EPAZOTE_SERVICE_NAME",
            "EPAZOTE_SERVICE_TYPE",
            "EPAZOTE_EXPECTED_STATUS",
            "EPAZOTE_ACTUAL_STATUS",
            "EPAZOTE_ERROR",
            "EPAZOTE_FAILURE_COUNT",
            "EPAZOTE_THRESHOLD",
            "EPAZOTE_URL",
        ]);

        let yaml = format!(
            r"
---
services:
  test-env:
    url: {mock_url}/test
    every: 30s
    expect:
      status: 200
      if_not:
        threshold: 2
        cmd: {script_path}
    "
        );

        let config = create_config(&yaml);
        let service = config.services.get("test-env").expect("Service not found");

        let _mock = server
            .mock("GET", "/test")
            .with_status(503)
            .create_async()
            .await;

        let (builder, _client_config) =
            build_client(service).expect("Failed to build client builder");
        let client = builder.build().expect("Failed to build client");
        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));

        for _ in 0..2 {
            let request = build_http_request(&client, service).expect("Failed to build request");
            let response = client
                .execute(request.build().expect("Failed to build request"))
                .await
                .expect("Failed to execute request");

            let rs = handle_http_response(
                "test-env",
                service,
                response,
                &ServiceMetrics::new().expect("Failed to create metrics"),
                Arc::clone(&counters),
            )
            .await
            .expect("Failed to handle response");

            assert!(!rs);
        }

        let output = fs::read_to_string(output_path).expect("Failed to read env capture");
        assert_eq!(
            output.lines().collect::<Vec<_>>(),
            vec![
                "test-env",
                "http",
                "200",
                "503",
                "status_mismatch",
                "2",
                "2",
                &format!("{mock_url}/test"),
            ]
        );
    }

    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn test_handle_http_response_if_not_cmd_resets_failure_count_after_success() {
        let mut server = Server::new_async().await;
        let mock_url = server.url();
        let (_tempdir, script_path, output_path) =
            create_env_capture_script(&["EPAZOTE_FAILURE_COUNT", "EPAZOTE_ERROR"]);

        let yaml = format!(
            r"
---
services:
  test-reset:
    url: {mock_url}/test
    every: 30s
    expect:
      status: 200
      if_not:
        threshold: 2
        cmd: {script_path}
    "
        );

        let config = create_config(&yaml);
        let service = config
            .services
            .get("test-reset")
            .expect("Service not found");

        let failing_mock = server
            .mock("GET", "/test")
            .with_status(503)
            .create_async()
            .await;

        let (builder, _client_config) =
            build_client(service).expect("Failed to build client builder");
        let client = builder.build().expect("Failed to build client");
        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let request = build_http_request(&client, service).expect("Failed to build request");
        let response = client
            .execute(request.build().expect("Failed to build request"))
            .await
            .expect("Failed to execute request");
        assert!(
            !handle_http_response(
                "test-reset",
                service,
                response,
                &ServiceMetrics::new().expect("Failed to create metrics"),
                Arc::clone(&counters),
            )
            .await
            .expect("Failed to handle response")
        );

        failing_mock.remove();
        let _success_mock = server
            .mock("GET", "/test")
            .with_status(200)
            .create_async()
            .await;

        let request = build_http_request(&client, service).expect("Failed to build request");
        let response = client
            .execute(request.build().expect("Failed to build request"))
            .await
            .expect("Failed to execute request");
        assert!(
            handle_http_response(
                "test-reset",
                service,
                response,
                &ServiceMetrics::new().expect("Failed to create metrics"),
                Arc::clone(&counters),
            )
            .await
            .expect("Failed to handle response")
        );

        let failing_mock = server
            .mock("GET", "/test")
            .with_status(503)
            .create_async()
            .await;

        for _ in 0..2 {
            let request = build_http_request(&client, service).expect("Failed to build request");
            let response = client
                .execute(request.build().expect("Failed to build request"))
                .await
                .expect("Failed to execute request");

            assert!(
                !handle_http_response(
                    "test-reset",
                    service,
                    response,
                    &ServiceMetrics::new().expect("Failed to create metrics"),
                    Arc::clone(&counters),
                )
                .await
                .expect("Failed to handle response")
            );
        }

        let output = fs::read_to_string(output_path).expect("Failed to read env capture");
        assert_eq!(
            output.lines().collect::<Vec<_>>(),
            vec!["2", "status_mismatch"]
        );

        failing_mock.remove();
    }

    #[tokio::test]
    async fn test_handle_http_response_threshold_delays_fallback() {
        let mut server = Server::new_async().await;
        let mock_url = server.url();

        let yaml = format!(
            r"
---
services:
  test-threshold:
    url: {mock_url}/test
    every: 30s
    expect:
      status: 200
      body: ok
      if_not:
        threshold: 3
        cmd: echo threshold
    "
        );

        let config = create_config(&yaml);
        let service = config
            .services
            .get("test-threshold")
            .expect("Service not found");

        let _ = env_logger::try_init();
        let _mock = server
            .mock("GET", "/test")
            .with_body("nope")
            .match_header(
                "User-Agent",
                mockito::Matcher::Regex("epazote.*".to_string()),
            )
            .with_status(200)
            .create_async()
            .await;

        let (builder, _client_config) =
            build_client(service).expect("Failed to build client builder");
        let client = builder.build().expect("Failed to build client");
        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));

        for expected_executions in [0, 0, 1] {
            let request = build_http_request(&client, service).expect("Failed to build request");
            let response = client
                .execute(request.build().expect("Failed to build request"))
                .await
                .expect("Failed to execute request");

            let rs = handle_http_response(
                "test-threshold",
                service,
                response,
                &ServiceMetrics::new().expect("Failed to create metrics"),
                Arc::clone(&counters),
            )
            .await
            .expect("Failed to handle response");

            assert!(!rs);

            let counters_locked = counters.lock().await;
            let state = counters_locked
                .get("test-threshold")
                .expect("State not found");
            assert_eq!(state.fallback_executions, expected_executions);
            drop(counters_locked);
        }
    }

    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn test_handle_http_response_success_resets_threshold_counter() {
        let mut server = Server::new_async().await;
        let mock_url = server.url();

        let yaml = format!(
            r"
---
services:
  test-threshold:
    url: {mock_url}/test
    every: 30s
    expect:
      status: 200
      body: ok
      if_not:
        threshold: 2
        cmd: echo threshold
    "
        );

        let config = create_config(&yaml);
        let service = config
            .services
            .get("test-threshold")
            .expect("Service not found");

        let _ = env_logger::try_init();
        let failing_mock = server
            .mock("GET", "/test")
            .with_body("nope")
            .match_header(
                "User-Agent",
                mockito::Matcher::Regex("epazote.*".to_string()),
            )
            .with_status(200)
            .create_async()
            .await;

        let (builder, _client_config) =
            build_client(service).expect("Failed to build client builder");
        let client = builder.build().expect("Failed to build client");
        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let request = build_http_request(&client, service).expect("Failed to build request");
        let response = client
            .execute(request.build().expect("Failed to build request"))
            .await
            .expect("Failed to execute request");

        let first_failure = handle_http_response(
            "test-threshold",
            service,
            response,
            &ServiceMetrics::new().expect("Failed to create metrics"),
            Arc::clone(&counters),
        )
        .await
        .expect("Failed to handle response");
        assert!(!first_failure);

        failing_mock.remove();
        let _success_mock = server
            .mock("GET", "/test")
            .with_body("ok")
            .match_header(
                "User-Agent",
                mockito::Matcher::Regex("epazote.*".to_string()),
            )
            .with_status(200)
            .create_async()
            .await;

        let request = build_http_request(&client, service).expect("Failed to build request");
        let response = client
            .execute(request.build().expect("Failed to build request"))
            .await
            .expect("Failed to execute request");

        let success = handle_http_response(
            "test-threshold",
            service,
            response,
            &ServiceMetrics::new().expect("Failed to create metrics"),
            Arc::clone(&counters),
        )
        .await
        .expect("Failed to handle response");
        assert!(success);

        let failing_mock = server
            .mock("GET", "/test")
            .with_body("still-nope")
            .match_header(
                "User-Agent",
                mockito::Matcher::Regex("epazote.*".to_string()),
            )
            .with_status(200)
            .create_async()
            .await;

        let request = build_http_request(&client, service).expect("Failed to build request");
        let response = client
            .execute(request.build().expect("Failed to build request"))
            .await
            .expect("Failed to execute request");

        let second_failure = handle_http_response(
            "test-threshold",
            service,
            response,
            &ServiceMetrics::new().expect("Failed to create metrics"),
            Arc::clone(&counters),
        )
        .await
        .expect("Failed to handle response");
        assert!(!second_failure);

        let counters_locked = counters.lock().await;
        let state = counters_locked
            .get("test-threshold")
            .expect("State not found");
        assert_eq!(state.consecutive_failures, 1);
        assert_eq!(state.fallback_executions, 0);

        failing_mock.remove();
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
    url: {mock_url}/test
    every: 30s
    expect:
      status: 200
      body: r"\b(?:sopas|cit-02)\b" # match sopas or cit-02
      if_not:
        stop: 2
    "#
        );

        let config = create_config(&yaml);
        let service = config.services.get("test-stop").expect("Service not found");

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

        let (builder, _client_config) =
            build_client(service).expect("Failed to build client builder");
        let client = builder.build().expect("Failed to build client");
        let request = build_http_request(&client, service).expect("Failed to build request");
        let response = client
            .execute(request.build().expect("Failed to build request"))
            .await
            .expect("Failed to execute request");

        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let rs1 = handle_http_response(
            "test-stop",
            service,
            response,
            &ServiceMetrics::new().expect("Failed to create metrics"),
            Arc::clone(&counters),
        )
        .await
        .expect("Failed to handle response");

        assert!(!rs1);

        // Check counter after first attempt
        let count1 = {
            let counters_locked = counters.lock().await;
            counters_locked
                .get("test-stop")
                .map_or(0, |state| state.fallback_executions)
        };
        assert_eq!(count1, 1, "Counter should be 1 after first attempt");

        let request = build_http_request(&client, service).expect("Failed to build request");
        let response = client
            .execute(request.build().expect("Failed to build request"))
            .await
            .expect("Failed to execute request");

        let rs2 = handle_http_response(
            "test-stop",
            service,
            response,
            &ServiceMetrics::new().expect("Failed to create metrics"),
            Arc::clone(&counters),
        )
        .await
        .expect("Failed to handle response");

        assert!(!rs2);

        // Check counter after first attempt
        let count2 = {
            let counters_locked = counters.lock().await;
            counters_locked
                .get("test-stop")
                .map_or(0, |state| state.fallback_executions)
        };
        assert_eq!(count2, 2, "Counter should be 1 after first attempt");
    }

    #[tokio::test]
    async fn test_handle_http_response_expect_if_not_http() {
        // Start mock server
        let mut server = Server::new_async().await;
        let mock_url = server.url();

        let yaml = format!(
            r"
---
services:
  test-stop:
    url: {mock_url}/test
    every: 30s
    expect:
      status: 200
      body: http
      if_not:
        stop: 2
        http: {mock_url}/notify?milei=libra
    "
        );

        let config = create_config(&yaml);
        let service = config.services.get("test-stop").expect("Service not found");

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

        let (builder, _client_config) =
            build_client(service).expect("Failed to build client builder");
        let client = builder.build().expect("Failed to build client");
        let request = build_http_request(&client, service).expect("Failed to build request");
        let response = client
            .execute(request.build().expect("Failed to build request"))
            .await
            .expect("Failed to execute request");

        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let rs1 = handle_http_response(
            "test-stop",
            service,
            response,
            &ServiceMetrics::new().expect("Failed to create metrics"),
            Arc::clone(&counters),
        )
        .await
        .expect("Failed to handle response");

        assert!(!rs1);

        // Check counter after first attempt
        let count1 = {
            let counters_locked = counters.lock().await;
            counters_locked
                .get("test-stop")
                .map_or(0, |state| state.fallback_executions)
        };
        assert_eq!(count1, 1, "Counter should be 1 after first attempt");

        let request = build_http_request(&client, service).expect("Failed to build request");
        let response = client
            .execute(request.build().expect("Failed to build request"))
            .await
            .expect("Failed to execute request");

        let rs2 = handle_http_response(
            "test-stop",
            service,
            response,
            &ServiceMetrics::new().expect("Failed to create metrics"),
            Arc::clone(&counters),
        )
        .await
        .expect("Failed to handle response");

        assert!(!rs2);

        // Check counter after first attempt
        let count2 = {
            let counters_locked = counters.lock().await;
            counters_locked
                .get("test-stop")
                .map_or(0, |state| state.fallback_executions)
        };
        assert_eq!(count2, 2, "Counter should be 1 after first attempt");
    }

    #[tokio::test]
    async fn test_handle_http_response_expect_body_regex_example() {
        // Start mock server
        let mut server = Server::new_async().await;
        let mock_url = server.url();

        let yaml = format!(
            r#"
---
services:
  test-stop:
    url: {mock_url}/test
    every: 30s
    expect:
      status: 200
      body: r"success|ok"
    "#
        );

        let config = create_config(&yaml);
        let service = config.services.get("test-stop").expect("Service not found");

        let _ = env_logger::try_init();
        let mock = server
            .mock("GET", "/test")
            .with_body("success")
            .match_header(
                "User-Agent",
                mockito::Matcher::Regex("epazote.*".to_string()),
            )
            .with_status(200)
            .create_async()
            .await;

        let (builder, _client_config) =
            build_client(service).expect("Failed to build client builder");
        let client = builder.build().expect("Failed to build client");
        let request = build_http_request(&client, service).expect("Failed to build request");
        let response = client
            .execute(request.build().expect("Failed to build request"))
            .await
            .expect("Failed to execute request");

        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let rs1 = handle_http_response(
            "test-stop",
            service,
            response,
            &ServiceMetrics::new().expect("Failed to create metrics"),
            Arc::clone(&counters),
        )
        .await
        .expect("Failed to handle response");

        assert!(rs1);

        mock.remove();
        let _mock = server
            .mock("GET", "/test")
            .with_body("-- error --")
            .match_header(
                "User-Agent",
                mockito::Matcher::Regex("epazote.*".to_string()),
            )
            .with_status(200)
            .create_async()
            .await;

        let request = build_http_request(&client, service).expect("Failed to build request");
        let response = client
            .execute(request.build().expect("Failed to build request"))
            .await
            .expect("Failed to execute request");

        let rs2 = handle_http_response(
            "test-stop",
            service,
            response,
            &ServiceMetrics::new().expect("Failed to create metrics"),
            Arc::clone(&counters),
        )
        .await
        .expect("Failed to handle response");

        assert!(!rs2);

        mock.remove();
        let _mock = server
            .mock("GET", "/test")
            .with_body("-- ok --")
            .match_header(
                "User-Agent",
                mockito::Matcher::Regex("epazote.*".to_string()),
            )
            .with_status(200)
            .create_async()
            .await;

        let request = build_http_request(&client, service).expect("Failed to build request");
        let response = client
            .execute(request.build().expect("Failed to build request"))
            .await
            .expect("Failed to execute request");

        let rs3 = handle_http_response(
            "test-stop",
            service,
            response,
            &ServiceMetrics::new().expect("Failed to create metrics"),
            Arc::clone(&counters),
        )
        .await
        .expect("Failed to handle response");

        assert!(rs3);
    }

    #[tokio::test]
    async fn test_handle_http_response_expect_body_max_bytes_20() {
        // Start mock server
        let mut server = Server::new_async().await;
        let mock_url = server.url();

        let yaml = format!(
            r#"
---
services:
  test-max_bytes:
    url: {mock_url}/test
    every: 30s
    expect:
      status: 200
      body: "34917f37-72b9-403f-887c-20c5e93b7173"
    max_bytes: 20
    "#
        );

        let config = create_config(&yaml);
        let service = config
            .services
            .get("test-max_bytes")
            .expect("Service not found");

        let _ = env_logger::try_init();
        let _mock = server
            .mock("GET", "/test")
            .with_body("hello world 0123456789 34917f37-72b9-403f-887c-20c5e93b7173")
            .match_header(
                "User-Agent",
                mockito::Matcher::Regex("epazote.*".to_string()),
            )
            .with_status(200)
            .create_async()
            .await;

        let (builder, _client_config) =
            build_client(service).expect("Failed to build client builder");
        let client = builder.build().expect("Failed to build client");
        let request = build_http_request(&client, service).expect("Failed to build request");
        let response = client
            .execute(request.build().expect("Failed to build request"))
            .await
            .expect("Failed to execute request");
        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));

        // print body
        let rs = handle_http_response(
            "test-max_bytes",
            service,
            response,
            &ServiceMetrics::new().expect("Failed to create metrics"),
            Arc::clone(&counters),
        )
        .await
        .expect("Failed to handle response");

        assert!(!rs);
    }

    #[tokio::test]
    async fn test_handle_http_response_expect_body_max_bytes_64k() {
        // Start mock server
        let mut server = Server::new_async().await;
        let mock_url = server.url();

        let yaml = format!(
            r#"
---
services:
  test-max_bytes:
    url: {mock_url}/test
    every: 30s
    expect:
      status: 200
      body: "34917f37-72b9-403f-887c-20c5e93b7173"
    max_bytes: 64000
    "#
        );

        let response_body = format!(
            "{}{} --- FIN",
            generate_numbers(64 * 1024, 0),
            "34917f37-72b9-403f-887c-20c5e93b7173"
        );

        let config = create_config(&yaml);
        let service = config
            .services
            .get("test-max_bytes")
            .expect("Service not found");

        let _ = env_logger::try_init();
        let _mock = server
            .mock("GET", "/test")
            .with_body(response_body)
            .match_header(
                "User-Agent",
                mockito::Matcher::Regex("epazote.*".to_string()),
            )
            .with_status(200)
            .create_async()
            .await;

        let (builder, _client_config) =
            build_client(service).expect("Failed to build client builder");
        let client = builder.build().expect("Failed to build client");
        let request = build_http_request(&client, service).expect("Failed to build request");
        let response = client
            .execute(request.build().expect("Failed to build request"))
            .await
            .expect("Failed to execute request");
        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));

        // print body
        let rs = handle_http_response(
            "test-max_bytes",
            service,
            response,
            &ServiceMetrics::new().expect("Failed to create metrics"),
            Arc::clone(&counters),
        )
        .await
        .expect("Failed to handle response");

        assert!(!rs);
    }

    #[tokio::test]
    async fn test_handle_http_response_expect_body_read_in_chunks() {
        // Start mock server
        let mut server = Server::new_async().await;
        let mock_url = server.url();

        let yaml = format!(
            r#"
---
services:
  test-max_bytes:
    url: {mock_url}/test
    every: 30s
    expect:
      status: 200
      body: "34917f37-72b9-403f-887c-20c5e93b7173"
    "#
        );

        let response_body = format!(
            "{} --- {} --- {} --- FIN",
            generate_numbers(1024, 0),
            "34917f37-72b9-403f-887c-20c5e93b7173",
            generate_numbers(128 * 1024, 0)
        );

        let config = create_config(&yaml);
        let service = config
            .services
            .get("test-max_bytes")
            .expect("Service not found");

        let _ = env_logger::try_init();
        let _mock = server
            .mock("GET", "/test")
            .with_body(response_body)
            .match_header(
                "User-Agent",
                mockito::Matcher::Regex("epazote.*".to_string()),
            )
            .with_status(200)
            .create_async()
            .await;

        let (builder, _client_config) =
            build_client(service).expect("Failed to build client builder");
        let client = builder.build().expect("Failed to build client");
        let request = build_http_request(&client, service).expect("Failed to build request");
        let response = client
            .execute(request.build().expect("Failed to build request"))
            .await
            .expect("Failed to execute request");
        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));

        // print body
        let rs = handle_http_response(
            "test-max_bytes",
            service,
            response,
            &ServiceMetrics::new().expect("Failed to create metrics"),
            Arc::clone(&counters),
        )
        .await
        .expect("Failed to handle response");

        assert!(rs);
    }
}
