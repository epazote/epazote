# contrib

Packaging and service-management assets for `epazote`.

Current layout:

* `systemd/epazote.service`: systemd unit for packaged installs
* `systemd/epazote.env.example`: environment file installed as `/etc/epazote/epazote.env`
* `debian/`: maintainer scripts for `cargo deb`

Build flow:

```bash
cargo build --release --locked
cargo deb --no-build
cargo generate-rpm
```

Notes:

* the package installs the binary at `/usr/bin/epazote`
* the systemd unit expects the active config at `/etc/epazote/epazote.yml`
* the package installs `/etc/epazote/epazote.env` for config, port, verbosity, and OTEL overrides
* the service unit runs as `root` by default so fallback commands can restart local services when needed
* the post-install script enables the service and only tries to start it when `/etc/epazote/epazote.yml` exists
* after the service is enabled, systemd will keep retrying startup until the config exists and `epazote` can start successfully

This is intended to be the packaging base for deployments that install `.deb`
or `.rpm` artifacts instead of pushing a raw release binary.
