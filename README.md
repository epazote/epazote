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

## Run Epazote

    epazote -c epazote.yml

> default configuration file is `epazote.yml`


https://epazote.io
