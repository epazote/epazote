Changelog
=========

## 3.3.1 (2026-04-02)
- Improve fallback logging visibility by promoting threshold and stop limit messages from DEBUG to WARN/INFO levels for better operational awareness.
- Add execution counter display in fallback logs showing current execution number vs stop limit (e.g., "execution #1/3" or "execution #5/unlimited").
- Standardize fallback command logging across HTTP and command checks to consistently use INFO level.
- Update dependencies: clap 4.5 → 4.6, plus 54 transitive dependency updates including security patches for rustls-webpki and other critical components.

## 3.3.0
- Add native support for environment variables in CLI arguments (e.g., `EPAZOTE_VERBOSE`, `EPAZOTE_CONFIG`, `EPAZOTE_PORT`, `EPAZOTE_JSON_LOGS`) directly via `clap` `env` feature mappings.
- Update `contrib/systemd/epazote.service` to utilize CLI environment variables instead of explicitly passing command line arguments.
- Greatly optimize CPU and memory usage by entirely removing lock contention on tracking states across concurrent tasks.
- Prevent repeated TLS handshakes during fallback operations by utilizing a globally shared `reqwest::Client` connection pool via `LazyLock`.
- Improve runtime performance and avoid process-level OS lock micro-pauses by lazily fetching and caching the `SHELL` environment variable.
- Eliminate unnecessary heap memory allocations by converting `FallbackContext` to use strict string references (`&str`) during context generation.

## 3.2.0
- Pass `EPAZOTE_*` environment variables to `if_not.cmd` fallback scripts, including service name, failure reason, status, and threshold context.
- Default to pretty human-readable logs and add `--json-logs` for structured JSON output.
- Log failed expectation checks as `WARN` instead of `INFO`.
- Use compact pretty logs for successful HTTP checks and include response headers only for failed checks.

## 3.1.0
- Add `expect.json` for structured JSON response matching, including nested object and array subset checks.
- Add `if_not.threshold` to delay fallback actions until a configured number of consecutive failures is reached.
- Reset the fallback threshold counter after successful checks while keeping `if_not.stop` as the cap for fallback executions.
- Document `expect.json`, `if_not.threshold`, and the distinction between `threshold` and `stop`.
- Clarify that `test` and `if_not.cmd` use the current `SHELL`, falling back to `sh`.

## 3.0.5
- Make OTLP tracing opt-in unless an OTLP endpoint is configured.
- Cache HTTPS certificate expiry checks to avoid repeated TLS handshakes on every probe.
- Skip missed interval catch-up bursts after scheduler delays.
- Add packaged `contrib/` assets for systemd deployments, including `.deb` maintainer scripts and `.rpm`/`.deb` service files.
- Update packaging metadata to install the `epazote` systemd unit and environment file.

## 3.0.3
- Rust 2024 edition update.
- Switch from OpenSSL to Rustls.
- Updated dependencies.
- Code cleanup and strict linting.

## 3.0.0
- FreeBSD port `sysutils/epazote/`

## 0.11.0
- `max_bytes` to limit the size of the response body.
- when using `expect:body` the response body is processed in chunks, instead of loading the entire body.

## 0.10.0
- `epazote_` namespace/prefix for metrics.
- set service status to `0` apart incrementing the failure counter.

## 0.9.0
- implemented `http` in `if_not` to call a URL in case of failure.

## 0.8.0
- implemented `STOP` in `if_not` to establish a limit on how many times to retry the action, defaults no limit.

## 0.7.0
- expect:body added support for regex matching when starting with `r"`, defaults to `r".*<input>.*"`.
- default port /metrics to 9080

## 0.6.0
- Allow POST, PUT, DELETE, PATCH, OPTIONS, HEAD, TRACE, CONNECT methods.

## 0.5.0
- Complete rewrite of the project in Rust 🦀
