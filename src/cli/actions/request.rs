use crate::cli::{
    actions::metrics::ServiceMetrics,
    config::{BodyType, ServiceDetails},
};
use anyhow::{anyhow, Result};
use reqwest::{header::HeaderMap, Client, Method, RequestBuilder, StatusCode};
use tokio::process::Command;
use tracing::info;

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
        let mut content_type_set = false;

        // Check if Content-Type is already set in headers
        if let Some(headers) = &service_details.headers {
            if headers.contains_key("Content-Type") || headers.contains_key("content-type") {
                content_type_set = true;
            }
        }
        match body {
            BodyType::Json(json) => {
                request = request.json(json);
                if !content_type_set {
                    request = request.header(reqwest::header::CONTENT_TYPE, "application/json");
                }
            }
            BodyType::Form(form_data) => {
                request = request.form(form_data);
                if !content_type_set {
                    request = request.header(
                        reqwest::header::CONTENT_TYPE,
                        "application/x-www-form-urlencoded",
                    );
                }
            }
            BodyType::Text(text) => {
                request = request.body(text.clone()); // Handles XML, plain text, etc.
                if !content_type_set {
                    request = request.header(
                        reqwest::header::CONTENT_TYPE,
                        "application/x-www-form-urlencoded",
                    );
                }
            }
        }
    }

    Ok(request)
}

/// Handles the HTTP response
pub async fn handle_http_response(
    service_name: &str,
    service_details: &ServiceDetails,
    status: StatusCode,
    headers: &HeaderMap,
    metrics: &ServiceMetrics,
) -> Result<()> {
    if status.as_u16() != service_details.expect.status {
        // Set service status to NOT OK (0)
        metrics
            .service_status
            .with_label_values(&[service_name])
            .set(0);

        if let Some(if_not) = &service_details.expect.if_not {
            let exit_code = execute_fallback_command(&if_not.cmd).await?;

            info!(
                service_name = service_name,
                service_url = service_details.url,
                service_status = status.as_u16(),
                expect_status = service_details.expect.status,
                cmd_exit_code = exit_code,
                response_headers = ?headers
            );
        }
    } else {
        // Set service status to OK (1)
        metrics
            .service_status
            .with_label_values(&[service_name])
            .set(1);

        info!(
            service_name = service_name,
            service_url = service_details.url,
            service_status = status.as_u16(),
            response_headers = ?headers
        );
    }

    Ok(())
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
    use crate::cli::config::{Expect, HttpMethod, ServiceDetails};
    use mockito::Server;
    use reqwest::StatusCode;
    use std::sync::Arc;
    use tokio::time::Duration;

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

        let service_details = ServiceDetails {
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

        let response = handle_http_response(
            "test",
            &service_details,
            StatusCode::OK,
            &HeaderMap::new(),
            &metrics,
        )
        .await;

        assert!(response.is_ok());
        println!("{:?}", response.unwrap());
    }
}
