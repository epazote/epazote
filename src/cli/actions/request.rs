use crate::cli::{
    actions::{
        FallbackContext, FallbackServiceType, FallbackState, execute_fallbacks, get_fallback_state,
        metrics::ServiceMetrics, reset_fallback_state, should_continue_fallback,
    },
    config::{BodyType, Expect, ServiceDetails},
    telemetry,
};
use anyhow::{Result, anyhow};
use futures_util::StreamExt;
use regex::Regex;
use regex_syntax::hir::Look;
use reqwest::{
    Client, Method, RequestBuilder,
    header::{HeaderMap, HeaderValue},
};
use serde_json::Value;
use std::{collections::HashMap, fmt::Write as _, sync::Arc};
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use std::hash::BuildHasher;

// Default sliding-window size for streaming body/body_not scans. This bounds
// the memory retained per check (the whole body is still scanned), keeping the
// footprint small when many services are monitored from the same host.
const DEFAULT_SCAN_WINDOW_BYTES: usize = 64 * 1024;

// Default buffering limit for `expect.json` checks, which need the whole
// document in memory to be parsed.
const DEFAULT_JSON_MAX_BYTES: usize = 512 * 1024;

fn format_expected_status(status: Option<u16>) -> String {
    status.map_or_else(|| "any".to_string(), |status| status.to_string())
}

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
    expected_status: Option<u16>,
    matches: bool,
) -> String {
    let service_url = service_url.map_or("(none)", String::as_str);
    let expected_status = format_expected_status(expected_status);

    format!(
        "service_name: \"{service_name}\", service_url: \"{service_url}\", service_status: {service_status}, expected_status: {expected_status}, matches: {matches}"
    )
}

fn format_http_response_failure_log(
    service_name: &str,
    service_url: Option<&String>,
    service_status: u16,
    expected_status: Option<u16>,
    headers: &HeaderMap<HeaderValue>,
    matches: bool,
    reason: Option<&str>,
) -> String {
    let service_url = service_url.map_or("(none)", String::as_str);
    let expected_status = format_expected_status(expected_status);
    let reason = reason.map_or_else(String::new, |reason| format!("\nreason: {reason}"));

    format!(
        "service_name: \"{service_name}\", service_url: \"{service_url}\", service_status: {service_status}, expected_status: {expected_status}\nresponse_headers:{}{reason}\nmatches: {matches}",
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
    let status_matches = service_details.expect.status_matches(status.as_u16());

    // Check if the response body matches expected criteria
    let body_mismatch =
        match match_response_expectations(response, service_details, service_details.max_bytes)
            .await
        {
            Ok(mismatch) => mismatch,
            Err(e) => {
                // A failed or timed-out body read is a failed check: run the
                // normal threshold/stop fallback path before propagating the
                // error, otherwise remediation would be silently skipped.
                warn!("Service '{service_name}' body read failed, running fallback path: {e}");

                if let Some(action) = &service_details.expect.if_not
                    && should_continue_fallback(service_name, &counters, action).await
                {
                    let state = get_fallback_state(service_name, &counters)
                        .await
                        .unwrap_or_default();
                    let context = FallbackContext {
                        service_name,
                        service_type: FallbackServiceType::Http,
                        expected_status: service_details.expect.expected_status_i32(),
                        actual_status: Some(actual_status),
                        error: "body_read_error",
                        failure_count: state.consecutive_failures,
                        threshold: action.threshold.unwrap_or(1),
                        url: service_details.url.as_deref(),
                        test: None,
                    };

                    execute_fallbacks(action, &context, service_name).await?;
                }

                return Err(e);
            }
        };
    let body_matches = body_mismatch.is_none();

    let is_match = status_matches && body_matches;

    let failure_reason = if is_match {
        None
    } else {
        let mut reasons = Vec::new();
        if !status_matches {
            reasons.push("status_mismatch".to_string());
        }
        if let Some(mismatch) = &body_mismatch {
            reasons.push(format!("{}: {}", mismatch.reason, mismatch.detail));
        }
        Some(reasons.join("; "))
    };

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
                failure_reason.as_deref(),
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
            expected_status = %format_expected_status(service_details.expect.status),
            response_headers = %format_headers(&headers),
            matches = is_match
        );
    } else {
        warn!(
            service_name = service_name,
            service_url = service_details.url,
            service_status = status.as_u16(),
            expected_status = %format_expected_status(service_details.expect.status),
            response_headers = %format_headers(&headers),
            reason = failure_reason.as_deref().unwrap_or("unknown"),
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
            expected_status: service_details.expect.expected_status_i32(),
            actual_status: Some(actual_status),
            error: if status_matches {
                body_mismatch
                    .as_ref()
                    .map_or("request_error", |mismatch| mismatch.reason)
            } else {
                "status_mismatch"
            },
            failure_count: state.consecutive_failures,
            threshold: action.threshold.unwrap_or(1),
            url: service_details.url.as_deref(),
            test: None,
        };

        execute_fallbacks(action, &context, service_name).await?;
    }

    Ok(is_match)
}

struct BodyMismatch {
    reason: &'static str,
    detail: String,
}

struct BodyScanOutcome {
    body_found: bool,
    body_not_found: bool,
    bytes_scanned: usize,
    // First `prefix_limit` bytes of the body, kept for JSON parsing when
    // `json` and `body_not` are combined.
    prefix: Vec<u8>,
}

// A compiled body matcher. `needs_start`/`needs_end` mark patterns whose
// every match is anchored to the absolute start/end of the body; those are
// only evaluated against windows that truly touch the body start/end, so
// `^`/`$` cannot false-match at sliding-window edges. Known approximations:
// in mixed alternations (e.g. `^foo|bar`) the anchored branch may still
// false-match at a window edge, and `(?m)` line anchors or `\b` may misjudge
// the first bytes of a slid window; both are bounded by the half-window
// overlap. Plain substring patterns are always exact.
struct BodyPattern {
    regex: Regex,
    needs_start: bool,
    needs_end: bool,
}

impl BodyPattern {
    fn is_match(&self, text: &str, at_start: bool, at_end: bool) -> bool {
        if self.needs_start && !at_start {
            return false;
        }

        if self.needs_end && !at_end {
            return false;
        }

        self.regex.is_match(text)
    }
}

async fn match_response_expectations(
    response: reqwest::Response,
    service_details: &ServiceDetails,
    max_bytes: Option<usize>,
) -> Result<Option<BodyMismatch>> {
    let expect = &service_details.expect;

    if expect.body.is_none() && expect.json.is_none() && expect.body_not.is_none() {
        return Ok(None);
    }

    let body_regex = expect
        .body
        .as_deref()
        .map(compile_body_pattern)
        .transpose()?;
    let body_not_regex = expect
        .body_not
        .as_deref()
        .map(compile_body_pattern)
        .transpose()?;

    // JSON matching needs the whole (bounded) document in memory; body and
    // body_not are scanned over the full stream with bounded memory instead.
    if let Some(expected_json) = &expect.json {
        let json_limit = max_bytes.unwrap_or(DEFAULT_JSON_MAX_BYTES);
        let content_length = response.content_length();
        let truncation = truncation_note(Some(json_limit), content_length);

        let json_body = if body_not_regex.is_some() {
            // Single pass: stream-scan body_not over the whole body while
            // keeping the first `json_limit` bytes for the JSON parser. The
            // scan window is capped at the default so a large `max_bytes`
            // (meant for the JSON buffer) does not double the footprint, while
            // a smaller `max_bytes` (including 0 = don't read) still bounds it.
            let window = max_bytes.map_or(DEFAULT_SCAN_WINDOW_BYTES, |m| {
                m.min(DEFAULT_SCAN_WINDOW_BYTES)
            });
            let scan =
                scan_response_body(response, window, None, body_not_regex.as_ref(), json_limit)
                    .await?;

            if scan.body_not_found {
                return Ok(Some(forbidden_body_mismatch(expect)));
            }

            scan.prefix
        } else {
            collect_response_bytes(response, Some(json_limit)).await?
        };

        if !match_response_json(&json_body, expected_json) {
            return Ok(Some(BodyMismatch {
                reason: "json_mismatch",
                detail: format!("expected JSON not found in response body{truncation}"),
            }));
        }

        return Ok(None);
    }

    let scan = scan_response_body(
        response,
        max_bytes.unwrap_or(DEFAULT_SCAN_WINDOW_BYTES),
        body_regex.as_ref(),
        body_not_regex.as_ref(),
        0,
    )
    .await?;

    if body_regex.is_some() && !scan.body_found {
        return Ok(Some(BodyMismatch {
            reason: "body_mismatch",
            detail: format!(
                "expected body '{}' not found in {} bytes scanned",
                expect.body.as_deref().unwrap_or_default(),
                scan.bytes_scanned
            ),
        }));
    }

    if scan.body_not_found {
        return Ok(Some(forbidden_body_mismatch(expect)));
    }

    Ok(None)
}

fn forbidden_body_mismatch(expect: &Expect) -> BodyMismatch {
    BodyMismatch {
        reason: "body_not_match",
        detail: format!(
            "forbidden body '{}' found in response",
            expect.body_not.as_deref().unwrap_or_default()
        ),
    }
}

fn truncation_note(max_bytes: Option<usize>, content_length: Option<u64>) -> String {
    let Some(max) = max_bytes else {
        return String::new();
    };

    let max_u64 = u64::try_from(max).unwrap_or(u64::MAX);

    match content_length {
        Some(length) if length > max_u64 => {
            format!(" (body truncated to max_bytes={max}, content-length={length})")
        }
        _ => String::new(),
    }
}

fn read_chunk_error(e: &reqwest::Error) -> anyhow::Error {
    if e.is_timeout() {
        anyhow!("service 'timeout' exceeded while reading the response body: {e}")
    } else {
        anyhow!("Failed to read response chunk: {e}")
    }
}

fn compile_body_pattern(input: &str) -> Result<BodyPattern> {
    let (pattern, _raw) = regex_source(input)?;
    let regex = Regex::new(&pattern).map_err(|e| {
        error!(
            "Invalid regex pattern in Expect body: {}, Error: {}",
            input, e
        );
        e
    })?;

    let (needs_start, needs_end) = pattern_anchors(&pattern);

    Ok(BodyPattern {
        regex,
        needs_start,
        needs_end,
    })
}

/// Reports whether every match of `pattern` is anchored to the absolute start
/// and/or end of the haystack, using the regex HIR instead of guessing from
/// the pattern text. This correctly classifies shapes like `(?i)^foo`,
/// `(^foo)`, `(foo$)` (anchored) and `^foo|bar`, `(?m)foo$` (not anchored to
/// the whole body, so they must be evaluated on every window).
fn pattern_anchors(pattern: &str) -> (bool, bool) {
    // `Regex::new` already validated the pattern, so a parse failure here is
    // unreachable in practice; fall back to unanchored (evaluate everywhere).
    regex_syntax::parse(pattern).map_or((false, false), |hir| {
        let props = hir.properties();
        (
            props.look_set_prefix().contains(Look::Start),
            props.look_set_suffix().contains(Look::End),
        )
    })
}

/// Runs the configured matchers against the current window. Returns `true`
/// once every configured matcher has found its text, meaning the scan can
/// stop early.
fn evaluate_window(
    buffer: &[u8],
    body_regex: Option<&BodyPattern>,
    body_not_regex: Option<&BodyPattern>,
    outcome: &mut BodyScanOutcome,
    at_start: bool,
    at_end: bool,
) -> bool {
    let text = String::from_utf8_lossy(buffer);

    if let Some(regex) = body_regex
        && !outcome.body_found
        && regex.is_match(&text, at_start, at_end)
    {
        debug!("Match found in response body");
        outcome.body_found = true;
    }

    if let Some(regex) = body_not_regex
        && !outcome.body_not_found
        && regex.is_match(&text, at_start, at_end)
    {
        outcome.body_not_found = true;
    }

    (body_regex.is_none() || outcome.body_found)
        && (body_not_regex.is_none() || outcome.body_not_found)
}

/// Streams the response body and runs the configured matchers over a sliding
/// window, so the whole body is scanned while the retained buffer never grows
/// beyond `max_bytes`: incoming chunks are fed into the window in bounded
/// slices instead of being appended whole. Half of the window is kept as
/// overlap between evaluations so matches crossing a window boundary are
/// still found; matches longer than half the window may be missed if they
/// span a boundary. Stops reading as soon as every configured matcher has
/// found a match. When `prefix_limit` is non-zero, the first `prefix_limit`
/// bytes are additionally retained in `prefix` (used to parse JSON while
/// `body_not` scans the full stream).
async fn scan_response_body(
    response: reqwest::Response,
    window: usize,
    body_regex: Option<&BodyPattern>,
    body_not_regex: Option<&BodyPattern>,
    prefix_limit: usize,
) -> Result<BodyScanOutcome> {
    let mut outcome = BodyScanOutcome {
        body_found: false,
        body_not_found: false,
        bytes_scanned: 0,
        prefix: Vec::new(),
    };

    if window == 0 {
        return Ok(outcome);
    }

    let overlap = window / 2;
    let mut stream = response.bytes_stream();
    let mut buffer: Vec<u8> = Vec::new();
    let mut buffer_start = 0usize;
    let mut resolved = false;

    'read: while let Some(chunk) = stream.next().await {
        // Propagate the read failure instead of masking it as an empty body.
        // A truncated/failed read treated as success would corrupt match
        // decisions (e.g. `body_not` would falsely pass).
        let bytes = chunk.map_err(|e| read_chunk_error(&e))?;
        outcome.bytes_scanned = outcome.bytes_scanned.saturating_add(bytes.len());

        if outcome.prefix.len() < prefix_limit {
            let take = (prefix_limit - outcome.prefix.len()).min(bytes.len());
            let (head, _) = bytes.split_at(take);
            outcome.prefix.extend_from_slice(head);
        }

        let mut remaining: &[u8] = &bytes;

        while !remaining.is_empty() {
            let capacity = window.saturating_sub(buffer.len());

            if capacity == 0 {
                if evaluate_window(
                    &buffer,
                    body_regex,
                    body_not_regex,
                    &mut outcome,
                    buffer_start == 0,
                    false,
                ) {
                    resolved = true;
                    break 'read;
                }

                // Keep the newest `overlap` bytes so matches spanning the
                // window boundary survive into the next evaluation.
                let drain_to = buffer.len().saturating_sub(overlap);
                buffer.drain(..drain_to);
                buffer_start = buffer_start.saturating_add(drain_to);
                continue;
            }

            let take = remaining.len().min(capacity);
            let (head, tail) = remaining.split_at(take);
            buffer.extend_from_slice(head);
            remaining = tail;
        }
    }

    if !resolved {
        evaluate_window(
            &buffer,
            body_regex,
            body_not_regex,
            &mut outcome,
            buffer_start == 0,
            true,
        );
    }

    Ok(outcome)
}

fn match_response_json(body: &[u8], expected_json: &Value) -> bool {
    match serde_json::from_slice::<Value>(body) {
        Ok(actual_json) => json_contains(expected_json, &actual_json),
        Err(e) => {
            error!("Failed to parse response body as JSON: {}", e);
            false
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
                // Propagate the read failure instead of masking it as an empty body.
                // Returning an empty buffer here would let a truncated/failed read be
                // treated as a successful empty response, corrupting match decisions
                // (e.g. `body_not` would falsely pass).
                return Err(read_chunk_error(&e));
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
#[cfg(test)]
fn generate_regex_pattern(input: &str) -> Result<Regex> {
    let (pattern, _) = regex_source(input)?;

    debug!(
        "Generated regex for: {}, pattern: {}",
        input.trim(),
        pattern
    );

    Regex::new(&pattern).map_err(|e| {
        debug!("Regex compilation failed: {}", e);
        e.into()
    })
}

fn regex_source(input: &str) -> Result<(String, bool)> {
    let trimmed_input = input.trim();

    if trimmed_input.is_empty() {
        return Err(anyhow!("Input regex pattern cannot be empty"));
    }

    let raw = trimmed_input.strip_prefix("r\"");

    let pattern = raw.map_or_else(
        // Escape the input to prevent regex injection
        || regex::escape(trimmed_input),
        // If prefix exists, strip suffix and use raw regex
        |raw| raw.strip_suffix('"').unwrap_or(raw).to_string(),
    );

    Ok((pattern, raw.is_some()))
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
        // Normal input should be escaped
        let pattern = generate_regex_pattern("test").expect("Failed to generate regex pattern");
        assert_eq!(pattern.as_str(), "test");

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
        assert_eq!(pattern.as_str(), "hello world");

        // Ensure regex matching works
        assert!(pattern.is_match("this is a hello world test"));
        assert!(!pattern.is_match("this is a goodbye test"));

        // the . should be escaped
        let pattern =
            generate_regex_pattern("hello.world").expect("Failed to generate regex pattern");
        assert_eq!(pattern.as_str(), "hello\\.world");
        //
        let pattern = generate_regex_pattern("a+b*").expect("Failed to generate regex pattern");
        assert_eq!(pattern.as_str(), "a\\+b\\*");
    }

    #[test]
    fn test_regex_matching_behavior() {
        // 1. Test plain substring (should be unanchored/match anywhere)
        let pattern = generate_regex_pattern("findme").expect("Failed to generate regex");
        assert!(pattern.is_match("here is findme in the middle"));
        assert!(pattern.is_match("findme at the start"));
        assert!(pattern.is_match("at the end findme"));
        assert!(!pattern.is_match("nowhere to be found"));

        // 2. Test raw regex with anchors
        let pattern = generate_regex_pattern(r#"r"^start""#).expect("Failed to generate regex");
        assert!(pattern.is_match("start of string"));
        assert!(!pattern.is_match("not the start of string"));

        // 3. Test raw regex with case insensitivity
        let pattern = generate_regex_pattern(r#"r"(?i)apple""#).expect("Failed to generate regex");
        assert!(pattern.is_match("I like Apple"));
        assert!(pattern.is_match("apple pie"));
        assert!(!pattern.is_match("orange juice"));

        // 4. Test raw regex with word boundaries
        let pattern = generate_regex_pattern(r#"r"\bcat\b""#).expect("Failed to generate regex");
        assert!(pattern.is_match("the cat is here"));
        assert!(!pattern.is_match("category is feline"));
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
            Some(301),
            &headers,
            true,
            None,
        );

        assert!(formatted.contains(
            "service_name: \"google\", service_url: \"https://google.com\", service_status: 301, expected_status: 301\nresponse_headers:"
        ));
        assert!(formatted.contains("\n  content-type: text/html"));
        assert!(formatted.contains("\n  location: https://www.google.com/"));
        assert!(formatted.ends_with("\nmatches: true"));
        assert!(!formatted.contains("\nreason:"));

        let formatted = format_http_response_failure_log(
            "google",
            Some(&"https://google.com".to_string()),
            200,
            Some(200),
            &headers,
            false,
            Some("body_mismatch: expected body 'pg_up' not found in 5765639 bytes scanned"),
        );

        assert!(formatted.contains(
            "\nreason: body_mismatch: expected body 'pg_up' not found in 5765639 bytes scanned"
        ));
        assert!(formatted.ends_with("\nmatches: false"));
    }

    #[test]
    fn test_format_http_response_success_log() {
        let formatted = format_http_response_success_log(
            "google",
            Some(&"https://google.com".to_string()),
            301,
            Some(301),
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
                status: Some(200),
                header: None,
                body: None,
                body_not: None,
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
    async fn test_collect_response_bytes_propagates_stream_error() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        // Raw server: advertise a larger Content-Length than the body actually sent,
        // then close the connection so reqwest's body stream errors mid-read.
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("Failed to bind listener");
        let addr = listener.local_addr().expect("Failed to get local addr");

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await;
                // Claim 100 bytes but deliver only a few, then drop the connection.
                let _ = socket
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\npartial")
                    .await;
                let _ = socket.flush().await;
            }
        });

        let client = reqwest::Client::new();
        let response = client
            .get(format!("http://{addr}/"))
            .send()
            .await
            .expect("Failed to send request");

        let result = collect_response_bytes(response, None).await;
        assert!(
            result.is_err(),
            "expected the stream read error to propagate, got {result:?}"
        );
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
    async fn test_handle_http_response_expect_body_not() {
        let mut server = Server::new_async().await;
        let mock_url = server.url();

        let yaml = format!(
            r#"
---
services:
  test:
    url: {mock_url}/test
    every: 30s
    expect:
      body_not: r"error|failure|Fatal"
    "#
        );

        let config = create_config(&yaml);
        let service = config.services.get("test").expect("Service not found");

        let success_mock = server
            .mock("GET", "/test")
            .with_body("all components healthy")
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

        let rs = handle_http_response(
            "test",
            service,
            response,
            &ServiceMetrics::new().expect("Failed to create metrics"),
            Arc::clone(&counters),
        )
        .await
        .expect("Failed to handle response");

        assert!(rs);

        success_mock.remove();
        let _failure_mock = server
            .mock("GET", "/test")
            .with_body("Fatal writing output to destination")
            .with_status(500)
            .create_async()
            .await;

        let request = build_http_request(&client, service).expect("Failed to build request");
        let response = client
            .execute(request.build().expect("Failed to build request"))
            .await
            .expect("Failed to execute request");

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

    #[tokio::test]
    async fn test_handle_http_response_body_not_sets_error() {
        let mut server = Server::new_async().await;
        let mock_url = server.url();
        let (_tempdir, script_path, output_path) = create_env_capture_script(&["EPAZOTE_ERROR"]);

        let yaml = format!(
            r"
---
services:
  test-body-not:
    url: {mock_url}/test
    every: 30s
    expect:
      body_not: Failure
      if_not:
        cmd: {script_path}
    "
        );

        let config = create_config(&yaml);
        let service = config
            .services
            .get("test-body-not")
            .expect("Service not found");

        let _mock = server
            .mock("GET", "/test")
            .with_body("Failure writing output to destination")
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
            "test-body-not",
            service,
            response,
            &ServiceMetrics::new().expect("Failed to create metrics"),
            counters,
        )
        .await
        .expect("Failed to handle response");

        assert!(!rs);

        let output = fs::read_to_string(output_path).expect("Failed to read env capture");
        assert_eq!(output.trim(), "body_not_match");
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

    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn test_handle_http_response_success_resets_stop_counter() {
        let mut server = Server::new_async().await;
        let mock_url = server.url();
        let tempdir = tempfile::Builder::new()
            .prefix("epazote-http-stop-reset-")
            .tempdir_in(".")
            .expect("Failed to create temp dir");
        let script_path = tempdir.path().join("capture.sh");
        let output_path = tempdir.path().join("output.txt");

        fs::write(
            &script_path,
            format!(
                "#!/bin/sh\nprintenv EPAZOTE_FAILURE_COUNT >> {}\n",
                output_path.display()
            ),
        )
        .expect("Failed to write capture script");

        let mut permissions = fs::metadata(&script_path)
            .expect("Failed to stat script")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).expect("Failed to chmod script");

        let yaml = format!(
            r"
---
services:
  test-stop-reset:
    url: {mock_url}/test
    every: 30s
    expect:
      status: 200
      body: ok
      if_not:
        threshold: 2
        stop: 1
        cmd: {}
    ",
            script_path.display()
        );

        let config = create_config(&yaml);
        let service = config
            .services
            .get("test-stop-reset")
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
        let metrics = ServiceMetrics::new().expect("Failed to create metrics");

        for _ in 0..2 {
            let request = build_http_request(&client, service).expect("Failed to build request");
            let response = client
                .execute(request.build().expect("Failed to build request"))
                .await
                .expect("Failed to execute request");

            assert!(
                !handle_http_response(
                    "test-stop-reset",
                    service,
                    response,
                    &metrics,
                    Arc::clone(&counters),
                )
                .await
                .expect("Failed to handle response")
            );
        }

        let output = fs::read_to_string(&output_path).expect("Failed to read env capture");
        assert_eq!(output.lines().collect::<Vec<_>>(), vec!["2"]);

        failing_mock.remove();
        let success_mock = server
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

        assert!(
            handle_http_response(
                "test-stop-reset",
                service,
                response,
                &metrics,
                Arc::clone(&counters),
            )
            .await
            .expect("Failed to handle response")
        );

        success_mock.remove();
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

        for _ in 0..2 {
            let request = build_http_request(&client, service).expect("Failed to build request");
            let response = client
                .execute(request.build().expect("Failed to build request"))
                .await
                .expect("Failed to execute request");

            assert!(
                !handle_http_response(
                    "test-stop-reset",
                    service,
                    response,
                    &metrics,
                    Arc::clone(&counters),
                )
                .await
                .expect("Failed to handle response")
            );
        }

        let output = fs::read_to_string(output_path).expect("Failed to read env capture");
        assert_eq!(output.lines().collect::<Vec<_>>(), vec!["2", "2"]);

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

        // The expected text (36 bytes) is longer than the 20-byte scan window,
        // so it can never fit in a single evaluation: max_bytes must be larger
        // than the longest expected match.
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

        // The pattern sits just past the 64000-byte window; the sliding-window
        // scan keeps reading past max_bytes and finds it (issue #20).
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

    #[tokio::test]
    async fn test_handle_http_response_expect_body_found_beyond_max_bytes() {
        // Regression for issue #20: a body much larger than max_bytes with the
        // expected text near the end must still match; max_bytes only bounds
        // memory, not how far the scan goes.
        let mut server = Server::new_async().await;
        let mock_url = server.url();

        let yaml = format!(
            r"
---
services:
  test-beyond:
    url: {mock_url}/metrics
    every: 30s
    expect:
      status: 200
      body: pg_up
    max_bytes: 1024
    "
        );

        let response_body = format!("{}\npg_up 1\n", generate_numbers(2 * 1024 * 1024, 0));

        let config = create_config(&yaml);
        let service = config
            .services
            .get("test-beyond")
            .expect("Service not found");

        let _mock = server
            .mock("GET", "/metrics")
            .with_body(response_body)
            .with_status(200)
            .create_async()
            .await;

        let client = reqwest::Client::new();
        let response = client
            .get(format!("{mock_url}/metrics"))
            .send()
            .await
            .unwrap();
        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let rs = handle_http_response(
            "test-beyond",
            service,
            response,
            &ServiceMetrics::new().expect("Failed to create metrics"),
            Arc::clone(&counters),
        )
        .await
        .expect("Failed to handle response");

        assert!(rs);
    }

    #[tokio::test]
    async fn test_handle_http_response_expect_body_default_window_scans_large_body() {
        // With no max_bytes configured, the default 64KB scan window must
        // still find text near the end of a multi-MB body.
        let mut server = Server::new_async().await;
        let mock_url = server.url();

        let yaml = format!(
            r"
---
services:
  test-default-window:
    url: {mock_url}/metrics
    every: 30s
    expect:
      status: 200
      body: pg_up
    "
        );

        let response_body = format!("{}\npg_up 1\n", generate_numbers(3 * 1024 * 1024, 0));

        let config = create_config(&yaml);
        let service = config
            .services
            .get("test-default-window")
            .expect("Service not found");

        let _mock = server
            .mock("GET", "/metrics")
            .with_body(response_body)
            .with_status(200)
            .create_async()
            .await;

        let client = reqwest::Client::new();
        let response = client
            .get(format!("{mock_url}/metrics"))
            .send()
            .await
            .unwrap();
        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let rs = handle_http_response(
            "test-default-window",
            service,
            response,
            &ServiceMetrics::new().expect("Failed to create metrics"),
            Arc::clone(&counters),
        )
        .await
        .expect("Failed to handle response");

        assert!(rs);
    }

    #[tokio::test]
    async fn test_handle_http_response_expect_body_not_found_beyond_max_bytes() {
        // A forbidden pattern past max_bytes must still be detected.
        let mut server = Server::new_async().await;
        let mock_url = server.url();

        let yaml = format!(
            r"
---
services:
  test-not-beyond:
    url: {mock_url}/test
    every: 30s
    expect:
      body_not: Fatal
    max_bytes: 1024
    "
        );

        let response_body = format!(
            "{}\nFatal writing output to destination\n",
            generate_numbers(512 * 1024, 0)
        );

        let config = create_config(&yaml);
        let service = config
            .services
            .get("test-not-beyond")
            .expect("Service not found");

        let _mock = server
            .mock("GET", "/test")
            .with_body(response_body)
            .with_status(200)
            .create_async()
            .await;

        let client = reqwest::Client::new();
        let response = client.get(format!("{mock_url}/test")).send().await.unwrap();
        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let rs = handle_http_response(
            "test-not-beyond",
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
    async fn test_handle_http_response_body_read_error_triggers_fallback() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        // A body read that fails mid-stream must still run the if_not
        // fallback path (EPAZOTE_ERROR=body_read_error) before erroring.
        let (_tempdir, script_path, output_path) = create_env_capture_script(&["EPAZOTE_ERROR"]);

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("Failed to bind listener");
        let addr = listener.local_addr().expect("Failed to get local addr");

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await;
                // Claim 100 bytes but deliver only a few, then drop the connection.
                let _ = socket
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\npartial")
                    .await;
                let _ = socket.flush().await;
            }
        });

        let yaml = format!(
            r"
---
services:
  test-read-error:
    url: http://{addr}/
    every: 30s
    expect:
      status: 200
      body: pg_up
      if_not:
        cmd: {script_path}
    "
        );

        let config = create_config(&yaml);
        let service = config
            .services
            .get("test-read-error")
            .expect("Service not found");

        let client = reqwest::Client::new();
        let response = client
            .get(format!("http://{addr}/"))
            .send()
            .await
            .expect("Failed to send request");

        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let rs = handle_http_response(
            "test-read-error",
            service,
            response,
            &ServiceMetrics::new().expect("Failed to create metrics"),
            Arc::clone(&counters),
        )
        .await;

        assert!(rs.is_err(), "read error should propagate, got {rs:?}");

        let output = fs::read_to_string(output_path).expect("Failed to read env capture");
        assert_eq!(output.trim(), "body_read_error");

        let counters_locked = counters.lock().await;
        let state = counters_locked
            .get("test-read-error")
            .expect("State not found");
        assert_eq!(state.consecutive_failures, 1);
        assert_eq!(state.fallback_executions, 1);
    }

    #[tokio::test]
    async fn test_handle_http_response_json_with_body_not_scans_full_body() {
        // body_not combined with expect.json must scan the whole body, not
        // just the buffered JSON prefix.
        let mut server = Server::new_async().await;
        let mock_url = server.url();

        let yaml = format!(
            r"
---
services:
  test-json-body-not:
    url: {mock_url}/api
    every: 30s
    expect:
      status: 200
      body_not: Fatal
      json:
        status: success
    max_bytes: 1024
    "
        );

        // Valid JSON larger than max_bytes, with the forbidden text near the
        // end: json parsing of the 1024-byte prefix fails AND body_not must
        // still catch the forbidden text past the prefix.
        let dirty_body = format!(
            r#"{{"status":"success","data":"{}Fatal error"}}"#,
            "x".repeat(200 * 1024)
        );

        let config = create_config(&yaml);
        let service = config
            .services
            .get("test-json-body-not")
            .expect("Service not found");

        let dirty_mock = server
            .mock("GET", "/api")
            .with_body(dirty_body)
            .with_status(200)
            .create_async()
            .await;

        let client = reqwest::Client::new();
        let response = client.get(format!("{mock_url}/api")).send().await.unwrap();
        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let rs = handle_http_response(
            "test-json-body-not",
            service,
            response,
            &ServiceMetrics::new().expect("Failed to create metrics"),
            Arc::clone(&counters),
        )
        .await
        .expect("Failed to handle response");

        assert!(!rs);

        // Clean small JSON: body_not passes and json matches via the prefix.
        dirty_mock.remove();
        let _clean_mock = server
            .mock("GET", "/api")
            .with_body(r#"{"status":"success","data":"all good"}"#)
            .with_status(200)
            .create_async()
            .await;

        let response = client.get(format!("{mock_url}/api")).send().await.unwrap();

        let rs = handle_http_response(
            "test-json-body-not",
            service,
            response,
            &ServiceMetrics::new().expect("Failed to create metrics"),
            Arc::clone(&counters),
        )
        .await
        .expect("Failed to handle response");

        assert!(rs);
    }

    #[tokio::test]
    async fn test_handle_http_response_json_with_body_not_uses_small_scan_window() {
        // A large max_bytes value is needed for JSON parsing here, but it must
        // not also become the body_not scan window.
        let mut server = Server::new_async().await;
        let mock_url = server.url();

        let yaml = format!(
            r"
---
services:
  test-json-body-not-large-prefix:
    url: {mock_url}/api
    every: 30s
    expect:
      status: 200
      body_not: Fatal
      json:
        status: success
    max_bytes: 262144
    "
        );

        let response_body = format!(
            r#"{{"status":"success","data":"{}Fatal error"}}"#,
            "x".repeat(200 * 1024)
        );

        let config = create_config(&yaml);
        let service = config
            .services
            .get("test-json-body-not-large-prefix")
            .expect("Service not found");

        let _mock = server
            .mock("GET", "/api")
            .with_body(response_body)
            .with_status(200)
            .create_async()
            .await;

        let client = reqwest::Client::new();
        let response = client.get(format!("{mock_url}/api")).send().await.unwrap();
        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let rs = handle_http_response(
            "test-json-body-not-large-prefix",
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
    async fn test_handle_http_response_raw_start_anchor_uses_response_start() {
        let mut server = Server::new_async().await;
        let mock_url = server.url();

        let yaml = format!(
            r#"
---
services:
  late-start:
    url: {mock_url}/late
    every: 30s
    expect:
      status: 200
      body: r"^start"
    max_bytes: 16
  true-start:
    url: {mock_url}/true
    every: 30s
    expect:
      status: 200
      body: r"^start"
    max_bytes: 16
    "#
        );

        let config = create_config(&yaml);
        let late_service = config
            .services
            .get("late-start")
            .expect("Service not found");
        let true_service = config
            .services
            .get("true-start")
            .expect("Service not found");

        let _late_mock = server
            .mock("GET", "/late")
            .with_body(format!("{}start{}", "x".repeat(16), "y".repeat(20)))
            .with_status(200)
            .create_async()
            .await;
        let _true_mock = server
            .mock("GET", "/true")
            .with_body(format!("start{}", "y".repeat(20)))
            .with_status(200)
            .create_async()
            .await;

        let client = reqwest::Client::new();
        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let late_response = client.get(format!("{mock_url}/late")).send().await.unwrap();
        let late_match = handle_http_response(
            "late-start",
            late_service,
            late_response,
            &ServiceMetrics::new().expect("Failed to create metrics"),
            Arc::clone(&counters),
        )
        .await
        .expect("Failed to handle response");

        assert!(!late_match);

        let true_response = client.get(format!("{mock_url}/true")).send().await.unwrap();
        let true_match = handle_http_response(
            "true-start",
            true_service,
            true_response,
            &ServiceMetrics::new().expect("Failed to create metrics"),
            Arc::clone(&counters),
        )
        .await
        .expect("Failed to handle response");

        assert!(true_match);
    }

    #[tokio::test]
    async fn test_handle_http_response_raw_end_anchor_uses_response_end() {
        let mut server = Server::new_async().await;
        let mock_url = server.url();

        let yaml = format!(
            r#"
---
services:
  internal-end:
    url: {mock_url}/internal
    every: 30s
    expect:
      status: 200
      body_not: r"end$"
    max_bytes: 16
  true-end:
    url: {mock_url}/true
    every: 30s
    expect:
      status: 200
      body_not: r"end$"
    max_bytes: 16
    "#
        );

        let config = create_config(&yaml);
        let internal_service = config
            .services
            .get("internal-end")
            .expect("Service not found");
        let true_service = config.services.get("true-end").expect("Service not found");

        let _internal_mock = server
            .mock("GET", "/internal")
            .with_body(format!("{}end{}", "x".repeat(13), "y".repeat(20)))
            .with_status(200)
            .create_async()
            .await;
        let _true_mock = server
            .mock("GET", "/true")
            .with_body(format!("{}end", "x".repeat(20)))
            .with_status(200)
            .create_async()
            .await;

        let client = reqwest::Client::new();
        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let internal_response = client
            .get(format!("{mock_url}/internal"))
            .send()
            .await
            .unwrap();
        let internal_match = handle_http_response(
            "internal-end",
            internal_service,
            internal_response,
            &ServiceMetrics::new().expect("Failed to create metrics"),
            Arc::clone(&counters),
        )
        .await
        .expect("Failed to handle response");

        assert!(internal_match);

        let true_response = client.get(format!("{mock_url}/true")).send().await.unwrap();
        let true_match = handle_http_response(
            "true-end",
            true_service,
            true_response,
            &ServiceMetrics::new().expect("Failed to create metrics"),
            Arc::clone(&counters),
        )
        .await
        .expect("Failed to handle response");

        assert!(!true_match);
    }

    #[tokio::test]
    async fn test_handle_http_response_inline_flag_start_anchor_not_window_relative() {
        // `(?i)^foo` is start-anchored even though the pattern text does not
        // begin with `^`; it must not match a window that merely starts at
        // "foo" mid-body (window 16, overlap 8 -> windows start at 0, 8, ...).
        let mut server = Server::new_async().await;
        let mock_url = server.url();

        let yaml = format!(
            r#"
---
services:
  mid-foo:
    url: {mock_url}/mid
    every: 30s
    expect:
      status: 200
      body: r"(?i)^foo"
    max_bytes: 16
  true-foo:
    url: {mock_url}/true
    every: 30s
    expect:
      status: 200
      body: r"(?i)^foo"
    max_bytes: 16
    "#
        );

        let config = create_config(&yaml);
        let mid_service = config.services.get("mid-foo").expect("Service not found");
        let true_service = config.services.get("true-foo").expect("Service not found");

        // "FOO" sits exactly at byte 8, where the second window starts.
        let _mid_mock = server
            .mock("GET", "/mid")
            .with_body(format!("{}FOO{}", "x".repeat(8), "y".repeat(20)))
            .with_status(200)
            .create_async()
            .await;
        let _true_mock = server
            .mock("GET", "/true")
            .with_body(format!("FOObar{}", "y".repeat(20)))
            .with_status(200)
            .create_async()
            .await;

        let client = reqwest::Client::new();
        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let mid_response = client.get(format!("{mock_url}/mid")).send().await.unwrap();
        let mid_match = handle_http_response(
            "mid-foo",
            mid_service,
            mid_response,
            &ServiceMetrics::new().expect("Failed to create metrics"),
            Arc::clone(&counters),
        )
        .await
        .expect("Failed to handle response");

        assert!(!mid_match);

        let true_response = client.get(format!("{mock_url}/true")).send().await.unwrap();
        let true_match = handle_http_response(
            "true-foo",
            true_service,
            true_response,
            &ServiceMetrics::new().expect("Failed to create metrics"),
            Arc::clone(&counters),
        )
        .await
        .expect("Failed to handle response");

        assert!(true_match);
    }

    #[tokio::test]
    async fn test_handle_http_response_mixed_alternation_unanchored_branch_matches() {
        // `^start|backup` has an unanchored branch, so it must be evaluated on
        // every window: "backup" deep in the body has to match.
        let mut server = Server::new_async().await;
        let mock_url = server.url();

        let yaml = format!(
            r#"
---
services:
  deep-backup:
    url: {mock_url}/deep
    every: 30s
    expect:
      status: 200
      body: r"^start|backup"
    max_bytes: 16
    "#
        );

        let config = create_config(&yaml);
        let service = config
            .services
            .get("deep-backup")
            .expect("Service not found");

        let _mock = server
            .mock("GET", "/deep")
            .with_body(format!("{}backup{}", "x".repeat(40), "y".repeat(10)))
            .with_status(200)
            .create_async()
            .await;

        let client = reqwest::Client::new();
        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let response = client.get(format!("{mock_url}/deep")).send().await.unwrap();
        let rs = handle_http_response(
            "deep-backup",
            service,
            response,
            &ServiceMetrics::new().expect("Failed to create metrics"),
            Arc::clone(&counters),
        )
        .await
        .expect("Failed to handle response");

        assert!(rs);
    }

    #[tokio::test]
    async fn test_handle_http_response_multiline_end_anchor_matches_mid_body() {
        // `(?m)ok$` anchors to line ends, not the body end, so a matching line
        // in the middle of the body must be found in a mid-stream window.
        let mut server = Server::new_async().await;
        let mock_url = server.url();

        let yaml = format!(
            r#"
---
services:
  mid-line:
    url: {mock_url}/mid
    every: 30s
    expect:
      status: 200
      body: r"(?m)ok$"
    max_bytes: 16
    "#
        );

        let config = create_config(&yaml);
        let service = config.services.get("mid-line").expect("Service not found");

        let _mock = server
            .mock("GET", "/mid")
            .with_body(format!("{}\nok\n{}", "x".repeat(30), "y".repeat(30)))
            .with_status(200)
            .create_async()
            .await;

        let client = reqwest::Client::new();
        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let response = client.get(format!("{mock_url}/mid")).send().await.unwrap();
        let rs = handle_http_response(
            "mid-line",
            service,
            response,
            &ServiceMetrics::new().expect("Failed to create metrics"),
            Arc::clone(&counters),
        )
        .await
        .expect("Failed to handle response");

        assert!(rs);
    }

    #[tokio::test]
    async fn test_handle_http_response_grouped_end_anchor_not_window_relative() {
        // `(end$)` is end-anchored despite the trailing `)`: as body_not it
        // must not trigger when a mid-stream window happens to end at "end".
        let mut server = Server::new_async().await;
        let mock_url = server.url();

        let yaml = format!(
            r#"
---
services:
  internal-end:
    url: {mock_url}/internal
    every: 30s
    expect:
      status: 200
      body_not: r"(end$)"
    max_bytes: 16
  true-end:
    url: {mock_url}/true
    every: 30s
    expect:
      status: 200
      body_not: r"(end$)"
    max_bytes: 16
    "#
        );

        let config = create_config(&yaml);
        let internal_service = config
            .services
            .get("internal-end")
            .expect("Service not found");
        let true_service = config.services.get("true-end").expect("Service not found");

        // "end" fills bytes 13..16: the first 16-byte window ends with it.
        let _internal_mock = server
            .mock("GET", "/internal")
            .with_body(format!("{}end{}", "x".repeat(13), "y".repeat(20)))
            .with_status(200)
            .create_async()
            .await;
        let _true_mock = server
            .mock("GET", "/true")
            .with_body(format!("{}end", "x".repeat(20)))
            .with_status(200)
            .create_async()
            .await;

        let client = reqwest::Client::new();
        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let internal_response = client
            .get(format!("{mock_url}/internal"))
            .send()
            .await
            .unwrap();
        let internal_match = handle_http_response(
            "internal-end",
            internal_service,
            internal_response,
            &ServiceMetrics::new().expect("Failed to create metrics"),
            Arc::clone(&counters),
        )
        .await
        .expect("Failed to handle response");

        assert!(internal_match);

        let true_response = client.get(format!("{mock_url}/true")).send().await.unwrap();
        let true_match = handle_http_response(
            "true-end",
            true_service,
            true_response,
            &ServiceMetrics::new().expect("Failed to create metrics"),
            Arc::clone(&counters),
        )
        .await
        .expect("Failed to handle response");

        assert!(!true_match);
    }

    #[tokio::test]
    async fn test_handle_http_response_json_body_not_max_bytes_zero_reads_nothing() {
        // max_bytes: 0 means "don't read the body" in every matcher
        // combination, including json + body_not.
        let mut server = Server::new_async().await;
        let mock_url = server.url();

        let yaml = format!(
            r"
---
services:
  test-zero-combo:
    url: {mock_url}/api
    every: 30s
    expect:
      status: 200
      body_not: Fatal
      json:
        status: success
    max_bytes: 0
    "
        );

        let config = create_config(&yaml);
        let service = config
            .services
            .get("test-zero-combo")
            .expect("Service not found");

        let _mock = server
            .mock("GET", "/api")
            .with_body(r#"{"status":"success"}"#)
            .with_status(200)
            .create_async()
            .await;

        let client = reqwest::Client::new();
        let response = client.get(format!("{mock_url}/api")).send().await.unwrap();
        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let rs = handle_http_response(
            "test-zero-combo",
            service,
            response,
            &ServiceMetrics::new().expect("Failed to create metrics"),
            Arc::clone(&counters),
        )
        .await
        .expect("Failed to handle response");

        // Nothing is read, so the JSON expectation cannot be satisfied.
        assert!(!rs);
    }

    #[tokio::test]
    async fn test_handle_http_response_body_read_error_recovery_resets_counters() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        // After a body_read_error incremented the failure counters, a healthy
        // check must reset them for the next outage.
        let (_tempdir, script_path, _output_path) = create_env_capture_script(&["EPAZOTE_ERROR"]);

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("Failed to bind listener");
        let addr = listener.local_addr().expect("Failed to get local addr");

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await;
                let _ = socket
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\npartial")
                    .await;
                let _ = socket.flush().await;
            }
        });

        let mut server = Server::new_async().await;
        let mock_url = server.url();

        let yaml = format!(
            r"
---
services:
  test-recovery:
    url: {mock_url}/health
    every: 30s
    expect:
      status: 200
      body: pg_up
      if_not:
        cmd: {script_path}
    "
        );

        let config = create_config(&yaml);
        let service = config
            .services
            .get("test-recovery")
            .expect("Service not found");

        let client = reqwest::Client::new();
        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let broken_response = client
            .get(format!("http://{addr}/"))
            .send()
            .await
            .expect("Failed to send request");

        let rs = handle_http_response(
            "test-recovery",
            service,
            broken_response,
            &ServiceMetrics::new().expect("Failed to create metrics"),
            Arc::clone(&counters),
        )
        .await;
        assert!(rs.is_err());

        {
            let counters_locked = counters.lock().await;
            let state = counters_locked
                .get("test-recovery")
                .expect("State not found");
            assert_eq!(state.consecutive_failures, 1);
            assert_eq!(state.fallback_executions, 1);
        }

        let _mock = server
            .mock("GET", "/health")
            .with_body("pg_up 1")
            .with_status(200)
            .create_async()
            .await;

        let healthy_response = client
            .get(format!("{mock_url}/health"))
            .send()
            .await
            .unwrap();

        let rs = handle_http_response(
            "test-recovery",
            service,
            healthy_response,
            &ServiceMetrics::new().expect("Failed to create metrics"),
            Arc::clone(&counters),
        )
        .await
        .expect("Failed to handle response");
        assert!(rs);

        let counters_locked = counters.lock().await;
        let state = counters_locked
            .get("test-recovery")
            .expect("State not found");
        assert_eq!(state.consecutive_failures, 0);
        assert_eq!(state.fallback_executions, 0);
    }

    #[tokio::test]
    async fn test_handle_http_response_expect_body_utf8_multibyte() {
        let mut server = Server::new_async().await;
        let mock_url = server.url();

        let yaml = format!(
            r#"
---
services:
  test-utf8:
    url: {mock_url}/test
    every: 30s
    expect:
      status: 200
      body: "epazote 🌿"
    "#
        );

        let config = create_config(&yaml);
        let service = config.services.get("test-utf8").expect("Service not found");

        let _mock = server
            .mock("GET", "/test")
            .with_body("Automated HTTP supervisor: epazote 🌿")
            .with_status(200)
            .create_async()
            .await;

        let client = reqwest::Client::new();
        let response = client.get(format!("{mock_url}/test")).send().await.unwrap();
        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let rs = handle_http_response(
            "test-utf8",
            service,
            response,
            &ServiceMetrics::new().expect("Failed to create metrics"),
            Arc::clone(&counters),
        )
        .await
        .expect("Failed to handle response");

        assert!(rs);
    }

    #[tokio::test]
    async fn test_handle_http_response_expect_body_max_bytes_exact_match() {
        let mut server = Server::new_async().await;
        let mock_url = server.url();
        let body_str = "0123456789";

        let yaml = format!(
            r#"
---
services:
  test-exact:
    url: {mock_url}/test
    every: 30s
    expect:
      status: 200
      body: "89"
    max_bytes: 10
    "#
        );

        let config = create_config(&yaml);
        let service = config
            .services
            .get("test-exact")
            .expect("Service not found");

        let _mock = server
            .mock("GET", "/test")
            .with_body(body_str)
            .with_status(200)
            .create_async()
            .await;

        let client = reqwest::Client::new();
        let response = client.get(format!("{mock_url}/test")).send().await.unwrap();
        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let rs = handle_http_response(
            "test-exact",
            service,
            response,
            &ServiceMetrics::new().expect("Failed to create metrics"),
            Arc::clone(&counters),
        )
        .await
        .expect("Failed to handle response");

        assert!(rs);
    }

    #[tokio::test]
    async fn test_handle_http_response_expect_body_max_bytes_too_small() {
        let mut server = Server::new_async().await;
        let mock_url = server.url();
        let body_str = "0123456789ABCDEF";

        let yaml = format!(
            r#"
---
services:
  test-small:
    url: {mock_url}/test
    every: 30s
    expect:
      status: 200
      body: "ABC"
    max_bytes: 5
    "#
        );

        let config = create_config(&yaml);
        let service = config
            .services
            .get("test-small")
            .expect("Service not found");

        let _mock = server
            .mock("GET", "/test")
            .with_body(body_str)
            .with_status(200)
            .create_async()
            .await;

        let client = reqwest::Client::new();
        let response = client.get(format!("{mock_url}/test")).send().await.unwrap();
        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let rs = handle_http_response(
            "test-small",
            service,
            response,
            &ServiceMetrics::new().expect("Failed to create metrics"),
            Arc::clone(&counters),
        )
        .await
        .expect("Failed to handle response");

        // "ABC" is at index 10, past max_bytes=5, but the scan covers the whole
        // body so it is still found (issue #20)
        assert!(rs);
    }

    #[tokio::test]
    async fn test_handle_http_response_expect_json_max_bytes_limit() {
        let mut server = Server::new_async().await;
        let mock_url = server.url();
        let body_json =
            r#"{"status":"success","data":"some long data that we might want to truncate"}"#;

        let yaml = format!(
            r"
---
services:
  test-json-limit:
    url: {mock_url}/test
    every: 30s
    expect:
      status: 200
      json:
        status: success
    max_bytes: 10
    "
        );

        let config = create_config(&yaml);
        let service = config
            .services
            .get("test-json-limit")
            .expect("Service not found");

        let _mock = server
            .mock("GET", "/test")
            .with_body(body_json)
            .with_status(200)
            .create_async()
            .await;

        let client = reqwest::Client::new();
        let response = client.get(format!("{mock_url}/test")).send().await.unwrap();
        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let rs = handle_http_response(
            "test-json-limit",
            service,
            response,
            &ServiceMetrics::new().expect("Failed to create metrics"),
            Arc::clone(&counters),
        )
        .await
        .expect("Failed to handle response");

        // Should fail because we only read 10 bytes, so the JSON is invalid/incomplete
        assert!(!rs);
    }

    #[tokio::test]
    async fn test_handle_http_response_expect_body_max_bytes_zero() {
        let mut server = Server::new_async().await;
        let mock_url = server.url();

        let yaml = format!(
            r#"
---
services:
  test-zero:
    url: {mock_url}/test
    every: 30s
    expect:
      status: 200
      body: "anything"
    max_bytes: 0
    "#
        );

        let config = create_config(&yaml);
        let service = config.services.get("test-zero").expect("Service not found");

        let _mock = server
            .mock("GET", "/test")
            .with_body("some content")
            .with_status(200)
            .create_async()
            .await;

        let client = reqwest::Client::new();
        let response = client.get(format!("{mock_url}/test")).send().await.unwrap();
        let counters: Arc<Mutex<HashMap<String, FallbackState>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let rs = handle_http_response(
            "test-zero",
            service,
            response,
            &ServiceMetrics::new().expect("Failed to create metrics"),
            Arc::clone(&counters),
        )
        .await
        .expect("Failed to handle response");

        // Should fail because we read 0 bytes
        assert!(!rs);
    }
}
