# TODO

## Pass context to `if_not.cmd` via environment variables

Goal: make fallback scripts easier to use for alerts, notifications, and incident handling by exposing service context through `EPAZOTE_*` environment variables.

### Scope

- Keep the current `if_not.cmd` behavior unchanged for existing users.
- Inject extra environment variables only when epazote executes fallback commands.
- Preserve the current shell behavior:
  - use `$SHELL` when available
  - fallback to `sh`

### Proposed variables

- `EPAZOTE_SERVICE_NAME`
  - service key from the config file
- `EPAZOTE_SERVICE_TYPE`
  - `http` or `command`
- `EPAZOTE_URL`
  - set for HTTP checks
- `EPAZOTE_TEST`
  - set for command-based checks
- `EPAZOTE_EXPECTED_STATUS`
  - expected HTTP status or expected command exit code
- `EPAZOTE_ACTUAL_STATUS`
  - actual HTTP status or actual command exit code when available
- `EPAZOTE_ERROR`
  - short reason such as `status_mismatch`, `body_mismatch`, `json_mismatch`, `request_error`, or `command_failed`
- `EPAZOTE_FAILURE_COUNT`
  - current consecutive failure count
- `EPAZOTE_THRESHOLD`
  - active threshold value

### Matching and failure semantics

- For HTTP services, set:
  - `EPAZOTE_SERVICE_TYPE=http`
  - `EPAZOTE_URL`
  - expected and actual status
  - a short error reason based on the failure
- For command services, set:
  - `EPAZOTE_SERVICE_TYPE=command`
  - `EPAZOTE_TEST`
  - expected and actual exit codes
  - a short error reason
- If multiple checks fail on the same HTTP response, prefer a deterministic error reason order:
  1. `status_mismatch`
  2. `json_mismatch`
  3. `body_mismatch`
- Reset behavior should stay unchanged:
  - success resets the consecutive failure counter
  - `stop` remains the cap on fallback executions

### Implementation plan

1. Introduce a small fallback context type in `src/cli/actions/mod.rs`.
   - Include service name, service type, expected status, actual status, failure count, threshold, URL/test, and error reason.
   - Add a helper to convert that context into `Command::env(...)` pairs.

2. Refactor fallback command execution to accept context.
   - Change `execute_fallback_command(cmd: &str)` into a variant that also accepts fallback context.
   - Keep a small wrapper if needed for existing unit tests.

3. Surface failure details from HTTP checks.
   - In `src/cli/actions/request.rs`, determine whether the mismatch came from status, body, or JSON.
   - Build the fallback context before invoking `if_not.cmd`.

4. Surface failure details from command checks.
   - In `src/cli/actions/run.rs`, capture the expected exit code, actual exit code, and test command.
   - Build the fallback context before invoking `if_not.cmd`.

5. Reuse current threshold bookkeeping.
   - Use the current `FallbackState` values for `EPAZOTE_FAILURE_COUNT`.
   - Pass the resolved threshold value, defaulting to `1`.

6. Add tests.
   - Unit test that fallback env vars are populated for HTTP failures.
   - Unit test that fallback env vars are populated for command failures.
   - Test threshold values in env output after repeated failures.
   - Test that success resets `EPAZOTE_FAILURE_COUNT`.
   - Test that scripts with shebangs can read the injected env vars.

7. Update docs.
   - Add config examples using `if_not.cmd` with env vars.
   - Document every supported `EPAZOTE_*` variable and when it is present.
   - Add a practical alert example, such as mail or webhook scripts.

### Open questions

- Should unknown values be omitted or set as empty strings?
  - Recommendation: omit optional variables that do not apply.
- Should epazote also expose response body snippets?
  - Recommendation: no for the first version to avoid leaking sensitive data.
- Should we eventually support JSON payload on stdin in addition to env vars?
  - Recommendation: maybe later, but env vars are the right first step.
