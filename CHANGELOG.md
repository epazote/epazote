Changelog
=========

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
