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

## Durations

Every duration — `every`, `timeout`, `if_not.timeout` — is a **whole number followed by a unit**. The unit is required, and there are four:

| Unit | Meaning | Example |
| --- | --- | --- |
| `s` | seconds | `30s` |
| `m` | minutes | `5m` |
| `h` | hours | `12h` |
| `d` | days | `2d` |

**Fractions are rejected, not rounded** — `1.5m` is an error, because silently reading it as one minute or two would be a schedule you never wrote. **For anything finer than the next unit up, use seconds**: write `90s` rather than `1.5m`, and `36h` rather than `1.5d`.

Seconds are the floor. There is no millisecond unit, so `500ms` is an error too — the shortest duration epazote takes is `1s`, which is well below anything a health probe over a network can meaningfully resolve.

**Zero is rejected as well.** `0s` is not "as fast as possible", it is a schedule with no interval, so it is refused at start-up with the rest of the configuration rather than accepted and left to fail later.

## Serialize Conflicting Recoveries With `if_not.group`

Fallback commands run concurrently by default, so a slow restart for one service never delays recovery for another. Services that genuinely conflict — several sharing one restart script, or several restarts hitting one host — declare a `group`, and members of the same group run one at a time:

```yaml
services:
    db-primary:
        url: http://127.0.0.1:3306
        every: 30s
        expect:
            status: 200
            if_not:
                group: mysql                     # serialized against other 'mysql' members
                cmd: systemctl restart mariadb

    db-replica:
        url: http://127.0.0.1:3307
        every: 30s
        expect:
            status: 200
            if_not:
                group: mysql                     # waits for db-primary's restart to finish
                cmd: systemctl restart mariadb

    edge-cache:
        url: http://127.0.0.1:6081
        every: 30s
        expect:
            status: 200
            if_not:
                cmd: systemctl restart varnish   # no group — starts immediately
```

Declare a group when services share a fallback script (their output would otherwise interleave in one log file and you could not tell which line belonged to which service), or when their restarts contend for the same resource. Give *every* service the same group to serialize all recoveries process-wide.

The name is a label you choose — any non-empty string. It is never interpreted, only compared: two services queue behind each other exactly when their group names match, so `mysql` above could equally be `db-host-3` or `slow-restarts`. Matching is case-sensitive, surrounding whitespace is ignored, and an empty or whitespace-only group is refused at start-up rather than treated as absent.

To run a command ungrouped, **leave the key out**. A `group:` written with no value — including `group: null` and `group: ~` — is refused rather than read as absent, since writing the key is an intent to serialize and silently doing the opposite is the surprise groups exist to remove. The same applies to the rest of the `if_not` block: `cmd:`, `http:`, `stop:`, `threshold:` and `timeout:` must each carry a value if they are written at all. An `if_not` block must contain at least one action, `cmd` or `http`; budget and grouping settings alone are rejected. A non-empty `group` also requires `cmd`: groups serialize commands only, and an HTTP action never queues, so `group` beside only `http` would provide no protection. Empty or whitespace-only `test` and `if_not.cmd` strings are also rejected because the shell would otherwise report the no-op as successful.

A grouped `cmd` has two phases, and `if_not.timeout` applies to each separately: it waits up to `timeout` for its turn, and once it starts it gets the whole of `timeout` to run in. In the worst case a grouped fallback occupies twice `timeout`. An ungrouped command never waits, so it only ever has the second phase.

A command still queued when its wait runs out is skipped and logged rather than run late, and the next failed check retries it. `stop` bounds how many times the fallback actions actually execute, so an attempt in which *nothing ran at all* is handed back rather than spent on a restart that never happened — that is the case when `cmd` is the only action configured and it was skipped. When an `http` alert is also configured it was still sent, and that is an execution, so the attempt is kept and `stop` goes on capping how often you are alerted. If that alert succeeds the attempt is still recorded as `outcome="skipped"`: the label follows the command that was held back, while the refund follows what actually executed. If the alert fails, `failure` takes precedence over the simultaneous skip. Either way the failed check itself still counts toward `threshold`. None of this arises for an ungrouped command, which always runs and always spends its attempt.

The two phases are budgeted separately on purpose: sharing one deadline would let a queued restart start with only a sliver of time left and be killed part-way through — stopping a service without starting it again.

`if_not.http` takes no part in this. Alerts never queue and run concurrently with the command, so an alert goes out even while its command is waiting its turn, and even if that command is ultimately skipped.

At startup epazote warns when two services run an identical `cmd`, or invoke the same script with different arguments, without all of them sharing one group — the cases where a shared script is visible in the configuration. A group only covers the services actually in it, so grouping one side and forgetting the other is reported too, rather than passing as handled.

The check looks past a leading wrapper — `sudo`, `env`, a shell or interpreter — to find the script being run, since recovery commands routinely need privileges. It deliberately does *not* treat a system utility as a shared script, whether written as `systemctl` or `/usr/bin/systemctl`: those services have nothing in common beyond the tool, and reporting them would make the warning noise you learn to skip past. Where the command is too ambiguous to read — a wrapper with its own flags, or a shell construct like `cd /x && ./restart.sh` — it stays silent rather than guess.

It cannot detect a shared *resource* at all: nothing in `systemctl restart mariadb` says what else lives on that host, so grouping those remains your call.

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
