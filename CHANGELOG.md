Changelog
=========

## 4.1.0 (2026-08-29)

### Fixed

- **Health probes now open a fresh connection on every scan**: each service's `reqwest::Client` is built once and reused across scans, and reqwest's default idle-connection pool kept keep-alive sockets open between scans. When a monitored service closed an idle connection between scans — most easily when a service's `every` interval is close to that server's own idle-connection timeout — the next scan reused a stale socket and failed with a spurious `error sending request for url (...)`, even though the service was perfectly healthy on a new connection. A manual `curl` never reused the socket, so it never reproduced the error, which made the failures look random. Because each such error counts as a failed check, a run of them could reach an `if_not` threshold and fire an unwarranted restart/alert. The probe client now disables connection reuse (`pool_max_idle_per_host(0)`), so every scan connects fresh and the check reflects the service's current reachability. This has been the behaviour since the first Rust implementation, so it is a longstanding latent defect rather than a recent regression.
- **Fallback commands no longer overlap across services**: each service is scanned in its own task with its own failure counter, so when several services failed on the same tick — most easily when a shared dependency goes down, or when a burst of transient errors (such as the stale-connection failures above) trips multiple thresholds at once — their `if_not.cmd` scripts ran concurrently. Independent restart scripts fired at the same instant (a restart storm), and where several services share one fallback script their output interleaved in the shared log. Fallback commands are now serialized process-wide, so only one runs at a time and the rest queue behind it. Only the command takes that lock: `if_not.cmd` and `if_not.http` now run concurrently rather than command-then-HTTP, so an alert is never held up behind another service's restart — which, with the default 300s fallback `timeout`, could otherwise have been minutes. The alert now goes out while its command is still queued, as that command starts, or even when it is ultimately skipped — it no longer depends on the command's fate at all. Waiting for the lock is bounded by the fallback `timeout`, because the caller is the service's scan loop and an unbounded wait would stop probing that service entirely: a command still queued when its wait runs out is **skipped**, logged, and left for the next failed check to retry. `stop` bounds how many times the fallback actions actually *execute*, so an attempt in which nothing ran at all is handed back instead of spent — which is the case when `cmd` is the only action configured and it was skipped. This matters because lock contention only arises during a burst of simultaneous failures — precisely what the lock exists for — so counting skips would let a service exhaust its whole `stop` budget on attempts that never restarted anything, and then be abandoned while still down, having never been restarted once. When an `http` alert is also configured the refund is withheld: the alert takes no lock and was still sent, which is an execution, and refunding it would uncap alerting for exactly as long as the contention lasted. The failed check itself is real and still counts toward `threshold` in both cases. `timeout` is applied to each phase separately — up to `timeout` waiting for a turn, then the full `timeout` to run in — so that a command which does start is never killed part-way through a restart; the trade-off is that one fallback can occupy up to twice `timeout` in the worst case.
- **A failed probe is reported to Prometheus before the fallback runs**: on a transport error or a failed body read, `epazote_status` was only set to `0` after `execute_fallbacks` returned. Since fallback commands are serialized, a scan can sit queued behind another service's restart, so the gauge could keep reporting the last healthy scan for minutes while the service was already known to be down — and the body-read path returned early, so it never set the gauge at all. Both paths now mark the service down first, matching what the command-check and status-mismatch paths already did.
- **Killing a timed-out command's process group is no longer best-effort**: 4.0.0 started running each command in its own process group so a timeout could take its descendants with it, but the result of the `killpg` call was discarded. A kill the kernel refuses leaves those descendants running for as long as epazote does — the exact leak the process group exists to prevent, repeating on every scan — and swallowing the result left nothing in the log to explain a host slowly filling with orphaned restart scripts. The result is now checked: the group already having exited counts as success, and anything else is logged as an error naming the group. Exactly how much a refusal says about *which* members survived differs between Linux and the BSDs, so it is reported rather than interpreted. A group id of `0` is also refused outright, since `killpg(0, ...)` would signal epazote's own process group.

### Release

- **Smaller release binary**: the release profile now uses thin LTO, a single codegen unit and symbol stripping, cutting the binary by roughly a third (~14.3 MiB to ~9.8 MiB on `aarch64-apple-darwin`) with no behaviour change. `panic` stays the default `unwind`, and the build now refuses `panic = "abort"` outright rather than trusting the profile to stay that way: aborting would stop `Drop` cleanup — including the `kill_on_drop` that reaps a timed-out fallback child — and would also let a single panicking service task kill the whole supervisor, ending monitoring for every other service, instead of being contained and reported. Since tests never build with the release profile, that setting is the one part of this change no test could have caught.

## 4.0.1 (2026-08-28)

### Release

- **aarch64 release artifacts**: releases now also build `aarch64-unknown-linux-musl` (on a native ARM runner) and `aarch64-apple-darwin`, so each release ships eight artifacts instead of four — tarballs, RPM and DEB for both architectures. One architecture failing no longer cancels the others, so a partial build cannot silently produce a partial release. The test matrix gained `ubuntu-24.04-arm` to match, since Linux is the primary target.
- **Dependency refresh**: `chacha20`, `cpufeatures`, `flate2` and `miniz_oxide` were updated to their current releases. No behaviour or API change.

## 4.0.0 (2026-08-27)

### Breaking changes

- **`test` commands can now be killed**: command checks are bounded by the service `timeout` (default **5s**). A command that never returned previously blocked that service's task forever, silently stopping every later scan. A command that exceeds the timeout is now killed and counts as a failed check, so the normal `if_not` threshold/stop path runs. **A health-check script that legitimately runs longer than 5s must now set `timeout` explicitly**, e.g. `timeout: 30s`.

### Added

- **`if_not.timeout`**: recovery actions get their own time budget, default **300s**, since a restart legitimately takes far longer than a health probe. Applies to both `cmd` and `http`. Set per fallback, e.g. `if_not: { cmd: "systemctl restart app", timeout: 15m }`. Note that a hung recovery still holds that service for the full budget, so lower it when recovery should never linger. Accepts `s`/`m`/`h`/`d` like every other duration, and the unit is required.
- **Fallback command output**: stderr from a command is now logged instead of captured and discarded, making a failing recovery diagnosable. It is logged at warning level only when the command fails, since healthy commands legitimately write to stderr, and retention is capped while the pipe is still drained so a chatty command cannot grow memory or block.
- **Command services now emit metrics**: `test` services previously recorded no metrics at all — not `epazote_status`, not `epazote_response_time_seconds` — so a failing command service was invisible to Prometheus and only appeared if its fallback errored. Both are now reported per check. Existing dashboard panels will start showing command services that were previously absent. `epazote_failures_total` is unchanged and still counts scan errors only, matching the HTTP path.

### Fixed

- **`if_not` now runs for unreachable HTTPS services**: a failed SSL certificate check no longer aborts the scan before the HTTP request is made. Previously the check was error-propagated, so an unreachable `https://` service returned early, was counted in `epazote_failures_total` with `epazote_status` `0`, and its configured `if_not` fallback never ran — exactly when remediation is needed. The scan now continues to the HTTP request, so the outcome flows through the normal `expect`/`if_not` path instead. **If you alert on `epazote_failures_total` for HTTPS services, expect fewer counter increments and fallbacks that now actually fire.** Certificate failures are logged as a warning. Note that a service whose recovery has silently never run will now start executing it.
- **The certificate check is now bounded too**: neither the TCP connect nor the TLS handshake had a timeout, so a blackholed address or a peer that accepts TCP and never completes the handshake stalled every later scan for that service, even after the check stopped aborting the scan. It is now bounded by the service `timeout`.
- **A command that could not run is never reported healthy**: execution failures (a spawn error, or a command killed at its `timeout`) were reported as exit code 1. A hung command configured with `expect.status: 1` therefore matched, and was reported healthy with no fallback — the worst possible outcome for a service that was not being checked at all. Execution errors are now distinct from a genuine exit status: they always fail the check, `EPAZOTE_ACTUAL_STATUS` is unset for them, and `EPAZOTE_ERROR` is `command_error` rather than `command_failed`.
- **`if_not.http` is time-bounded**: the shared fallback HTTP client had no timeout, so an alert endpoint that accepted the connection and never answered hung the request and stalled every later scan. Both fallback actions are now bounded by `if_not.timeout`.
- **Failed certificate checks back off**: only successful checks were cached, so a service that was down repeated the whole connect and handshake on every scan ahead of the HTTP request. A failed check is now remembered for 60s before being retried, and is stamped when it finishes rather than when it starts — a check that burned its whole `timeout` was otherwise cached already expired, defeating the backoff for exactly the slow failures it exists for. Successful checks are stamped the same way, so the exported expiry is no longer short by the duration of the check.
- **Failed fallback HTTP bodies are logged**: the response body was consumed with its result discarded, so a fallback whose headers arrived and whose body then failed or timed out was reported as a clean success.
- **Timed-out commands no longer leak descendants**: killing the spawned shell left any processes it had started running, so a timeout leaked them on every scan. The command now runs in its own process group, which is signalled as a whole.
- **`if_not.cmd` and `if_not.http` are genuinely independent**: a failing or timed-out fallback command no longer skips the configured HTTP action, which is often the only way an operator learns that recovery failed. Both run, and the first error is still reported.

### Performance

- `expect.body`/`expect.body_not` patterns are compiled once and cached instead of recompiled on every response. Compilation costs orders of magnitude more than the match itself (~268us to compile a complex pattern versus ~6us to run it over a 64KB body).
- The system root certificate store is no longer cloned on every HTTPS scan. It was passed by value, so all ~150 system roots were copied even on the scans that hit the 12h certificate cache and open no TLS connection.
- The HTTP method is mapped to a constant instead of being formatted to a `String` and re-parsed on every request, and the response buffer is sized from `Content-Length` (bounded by `max_bytes`) instead of growing chunk by chunk.

### Security

- Refreshed dependencies, including `h2` 0.4.15 -> 0.4.19, which clears RUSTSEC-2026-0258 (unbounded empty DATA frames). A daily `cargo audit` workflow now runs in CI, and the repository has a published security policy.

## 3.7.1 (2026-08-02)
- **Maintenance**: Refresh dependencies and fix Rust 1.97 Clippy warnings.

## 3.7.0 (2026-06-10)
- **Full-Body Scanning (#20)**: `expect.body` and `expect.body_not` now scan the entire response body using a sliding window instead of only checking the first `max_bytes`. `max_bytes` now bounds memory only — it no longer truncates the search — so expected text deep inside a multi-MB body (e.g. `pg_up` in a 5MB `/metrics` page) is found. Reading stops early once every configured matcher has found its text, and the scan is time-bounded by the service `timeout` (default 5s): a body that takes longer to read is aborted and reported as `service 'timeout' exceeded while reading the response body`. Note: matches longer than half the window may be missed if they span a window boundary, so keep `max_bytes` comfortably larger than the longest expected match.
- **Lower Default Memory Footprint**: when `max_bytes` is unset, defaults are now matcher-aware: `body`/`body_not` scans use a 64KB sliding window (down from buffering 512KB) so monitoring many services from one host stays lightweight, while `expect.json` keeps buffering up to 512KB since a JSON document must be parsed whole. Setting `max_bytes` explicitly overrides the JSON buffer and non-JSON scan window; when `body_not` is combined with `expect.json`, the scan window is capped at 64KB and never larger than `max_bytes` (so `max_bytes: 0` still means "don't read the body"), while JSON buffering follows `max_bytes`.
- **Anchored Regex Correctness**: `^`/`$` (and `\A`/`\z`) in raw `r"..."` patterns refer to the start/end of the whole response body, not of each scan window. Anchor detection now uses exact regex parsing (`regex-syntax` HIR), so shapes like `(?i)^foo` or `(foo$)` are treated as anchored, while `^foo|bar` and `(?m)foo$` are correctly evaluated on every window. Known approximation: in mixed anchored/unanchored alternations the anchored branch may still match at a window edge, and `(?m)` line anchors are approximated near window boundaries.
- **Fallback on Body-Read Failures**: a body read that fails or times out mid-stream now runs the normal `if_not` threshold/stop fallback path with `EPAZOTE_ERROR=body_read_error` before the error is reported, instead of silently skipping remediation.
- **`body_not` + `json` Full Coverage**: when `body_not` is combined with `expect.json`, the forbidden pattern is now scanned over the entire body in a single streaming pass (the JSON document is still parsed from the buffered prefix), instead of only checking the buffered bytes.
- **Clear Failure Reasons (#20)**: when a check fails, logs now state why — e.g. `reason: body_mismatch: expected body 'pg_up' not found in 5765639 bytes scanned` — in both pretty and JSON log formats, instead of only `matches: false`. JSON-body checks report when the body was truncated by `max_bytes`.
- **Strict Config Validation (#20)**: unknown keys in `epazote.yml` now fail at startup with a clear error instead of being silently ignored (e.g. `max_size` reports `unknown field 'max_size', expected one of ... 'max_bytes' ...`). Invalid or empty `body`/`body_not` regex patterns are also rejected at startup (e.g. `invalid regex in 'expect.body': r"(unclosed"`) instead of failing every check at runtime. Review your config for stray keys before upgrading.
- **Verbosity Errors (#19)**: invalid `EPAZOTE_VERBOSE` values now produce a helpful error (`expected a number, e.g. EPAZOTE_VERBOSE=2 equals -vv`) instead of `invalid digit found in string`. Numeric values map as before: 1=info, 2=debug, 3+=trace.

## 3.6.1 (2026-06-09)
- **Bug Fix**: Reset `if_not.stop` execution counters after a service recovers so `stop: 1` runs once per outage instead of only once for the lifetime of the Epazote process.
- **Test Coverage**: Add regressions for HTTP and command checks with `threshold` + `stop` across recovery.
- **Docs**: Clarify that `if_not.stop` applies per outage and is reset by a healthy check.

## 3.6.0 (2026-06-08)
- **Configurable Metrics Bind Address**: Add `--bind` / `EPAZOTE_BIND` (default `[::]`) to control the interface the metrics server listens on. Set `--bind 127.0.0.1` or `--bind ::1` to keep `/metrics` local. Default behavior is unchanged, including the IPv4 (`0.0.0.0`) fallback when binding dual-stack `[::]`.
- **Correctness Fix**: `collect_response_bytes` now propagates a mid-stream read error instead of returning an empty body, preventing a failed/truncated read from being silently treated as a successful empty response (which could falsely pass `body_not` or falsely fail `body`/`json` checks).
- **Maintainability**: De-duplicate fallback execution (command + HTTP) into a single `execute_fallbacks` helper shared by the HTTP request-error, HTTP response-mismatch, and command-check paths.
- **Dependency Updates**: Replace the deprecated/unmaintained `serde_yaml` 0.9 with the maintained drop-in `serde_yaml_ng` 0.10, remove the unused `webpki` dependency, and refresh the lockfile.

## 3.5.2 (2026-05-20)
- **Dependency Updates**: Update OpenTelemetry crates to 0.32, `tracing-opentelemetry` to 0.33, `ctor` to 1.0, and refresh transitive dependencies.
- **Development Workflow**: Remove `.devcontainer` setup and document direct Cargo workflows for portable development across toolbox, Linux, and macOS environments.
- **Robustness**: Propagate metrics server failures instead of exiting successfully, make SSL root certificate loading return errors instead of panicking, and harden duration parsing against empty or overflowing values.
- **Test Coverage**: Replace public-network SSL checks with local TLS coverage and add regressions for metrics bind failures, duration parsing, and user-agent formatting.

## 3.5.1 (2026-05-04)
- **Critical Bug Fix**: Fix `if_not.stop` off-by-one error where `stop: 1` would never execute the fallback command. The check was comparing execution count before incrementing, causing `stop: 1` to incorrectly skip the first (and only intended) execution. Now executes exactly N times as configured before stopping.
- **Test Coverage**: Add `test_should_continue_fallback_stop_one()` and `test_should_continue_fallback_stop_zero()` regression tests to prevent future regressions with edge case `stop` values.

## 3.5.0 (2026-04-30)
- **Negative Body Matching**: Add `expect.body_not` to fail HTTP checks when a response body contains a forbidden plain-text or `r"..."` regex match.
- **Body-Only HTTP Checks**: Allow HTTP checks to omit `expect.status` when another matcher such as `body`, `body_not`, or `json` is configured. Command checks using `test` still require `expect.status`.
- **Fallback Context**: Report `EPAZOTE_ERROR=body_not_match` when `body_not` triggers `if_not`, and omit `EPAZOTE_EXPECTED_STATUS` when no expected HTTP status is configured.
- **Docs**: Document `body_not`, body-only checks such as `body_not: r"error|failure|Fatal"`, and add an `epazote-docs` DevPod setup for building docs without installing dependencies on the host.
- **Dependency Updates**: Run `cargo upgrade`/`cargo update`, including `ctor` 0.10 → 0.11 and transitive updates such as `reqwest` 0.13.2 → 0.13.3 and `rustls` 0.23.38 → 0.23.40. Update docs npm lockfile dependencies.

## 3.4.0 (2026-04-19)
- **OOM Protection**: Introduce a safe default limit of **512KB** for `max_bytes` to prevent memory exhaustion on large HTTP responses.
- **UTF-8 Bug Fix**: Fix high-severity bug in `match_response_body` where multi-byte characters split across network chunks caused data loss.
- **CPU Optimization**: 
    - Eliminate redundant `.*` padding in regex patterns for plain substring matches.
    - Switch to O(N) regex evaluation (single match at end of stream) instead of O(N²) eager matching on every chunk.
    - Cache `rustls::RootCertStore` in a static `LazyLock` to avoid synchronous certificate loading on every SSL check task.
- **Resilience**: Implement a supervision model where the main process exits gracefully if any service monitoring task fails, enabling external managers (like systemd) to restart the process.
- **Connection Stewardship**: Explicitly consume response bodies in fallback HTTP requests to ensure TCP connections are returned to the pool immediately.
- **Dependency Updates**: Update all dependencies to latest versions, including `ctor` 0.6 → 0.10.
- **Linting**: Full compliance with **Rust 1.95** Clippy pedantic and safety-critical lints.

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
