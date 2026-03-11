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

## Run Epazote

    epazote -c epazote.yml

> default configuration file is `epazote.yml`


https://epazote.io
