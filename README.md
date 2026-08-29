[![Test & Build](https://github.com/epazote/epazote/actions/workflows/build.yml/badge.svg)](https://github.com/epazote/epazote/actions/workflows/build.yml)
[![codecov](https://codecov.io/gh/epazote/epazote/branch/main/graph/badge.svg?token=ztiGQV2sTm)](https://codecov.io/gh/epazote/epazote)

# Epazote 🌿
Automated HTTP (microservices) supervisor

# How to use it
First you need to install **Epazote**:

    cargo install epazote

Or download the latest release from the [releases](https://github.com/epazote/epazote/releases)


## Basic example

```yaml
services:
    my_app:
        url: http://0.0.0.0:8080
        every: 1m
        expect:
            status: 200
            if_not:
                cmd: systemctl restart app
```

## Match JSON responses

```yaml
services:
    vmagent_targets:
        url: http://127.0.0.1:8429/api/v1/targets
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
```

`expect.body` still performs text or regex matching against the raw response body. Use `expect.json` for structured JSON subset matching.

## Reject Matching Response Bodies

```yaml
services:
    alloy_metrics:
        url: http://127.0.0.1:12345/metrics
        every: 30s
        expect:
            body_not: r"error|failure|Fatal"
            if_not:
                cmd: /script/when/failure.sh
```

`expect.body_not` uses the same text or `r"..."` regex matching as `expect.body`, but the service fails when the pattern is found. HTTP checks may omit `expect.status` when another matcher such as `body_not` is configured.

## Delay Fallback Actions With `threshold`

```yaml
services:
    vmagent_targets:
        url: http://127.0.0.1:8429/api/v1/targets
        every: 30s
        expect:
            status: 200
            json:
                status: success
            if_not:
                threshold: 3
                stop: 2
                cmd: systemctl restart vmagent
```

`threshold` waits for N consecutive failures before running `if_not` actions. `stop` limits how many times those fallback actions execute during one outage; a healthy check resets the counter for the next outage.

`if_not.timeout` bounds each fallback action — how long `cmd` may run before it is killed, and how long the `http` request may take (default: `300s`). Recovery is not a health probe, so it gets a far more generous budget than the service `timeout`, which bounds the check itself (default: `5s`). Raise it for a slow restart, lower it when recovery should never linger:

```yaml
            if_not:
                timeout: 15m
                cmd: systemctl restart vmagent
```

Fallback commands run one at a time across every service, so a burst of simultaneous failures cannot fire a storm of restarts at the same instant. A `cmd` therefore has two phases, and `if_not.timeout` applies to each of them separately: it waits up to `timeout` for its turn in that queue, and once it starts it gets the whole of `timeout` to run in. In the worst case a single fallback occupies twice `timeout`.

A command still queued when its wait runs out is skipped and logged rather than run late, and the next failed check retries it. `stop` bounds how many times the fallback actions actually execute, so an attempt in which *nothing ran at all* is handed back rather than spent on a restart that never happened — that is the case when `cmd` is the only action configured and it was skipped. When an `http` alert is also configured it was still sent, and that is an execution, so the attempt is kept and `stop` goes on capping how often you are alerted. Either way the failed check itself still counts toward `threshold`.

The two phases are budgeted separately on purpose: sharing one deadline would let a queued restart start with only a sliver of time left and be killed part-way through — stopping a service without starting it again.

`if_not.http` takes no part in this. Alerts are not serialized and run concurrently with the command, so an alert goes out even while its command is queued, and even if that command is ultimately skipped.

## Use `EPAZOTE_*` Variables In `if_not.cmd`

Fallback commands receive service context through environment variables, which makes alert scripts easier to write:

```yaml
services:
    vmagent_targets:
        url: http://127.0.0.1:8429/api/v1/targets
        every: 30s
        expect:
            status: 200
            json:
                status: success
            if_not:
                threshold: 3
                stop: 1
                cmd: /usr/local/bin/send-alert.sh
```

Available variables:

- `EPAZOTE_SERVICE_NAME`
- `EPAZOTE_SERVICE_TYPE`
- `EPAZOTE_URL` for HTTP checks
- `EPAZOTE_TEST` for command checks
- `EPAZOTE_EXPECTED_STATUS`
- `EPAZOTE_ACTUAL_STATUS` when available
- `EPAZOTE_ERROR`
- `EPAZOTE_FAILURE_COUNT`
- `EPAZOTE_THRESHOLD`

## Run Epazote

    epazote -c epazote.yml

> default configuration file is `epazote.yml`

Prometheus metrics are served on port `9080` by default (`--port` / `EPAZOTE_PORT`).
The metrics server binds to all interfaces (`[::]`) by default; use `--bind` /
`EPAZOTE_BIND` to restrict it, e.g. `epazote -c epazote.yml --bind 127.0.0.1` to keep
`/metrics` local-only.

Logs are pretty-printed by default for easier debugging. Use `--json-logs` if you want structured JSON logs instead.


https://epazote.io
