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

`threshold` waits for N consecutive failures before running `if_not` actions. `stop` limits how many times those fallback actions will be executed after the threshold is reached.

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

Logs are pretty-printed by default for easier debugging. Use `--json-logs` if you want structured JSON logs instead.


https://epazote.io
