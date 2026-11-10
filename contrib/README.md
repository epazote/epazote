# contrib

Packaging and service-management assets for `epazote`.

Current layout:

* `systemd/epazote.service`: systemd unit for packaged installs
* `systemd/epazote.env.example`: environment file installed as `/etc/epazote/epazote.env`
* `debian/`: maintainer scripts for `cargo deb`

Build flow for native Linux (use `aarch64-unknown-linux-musl` on ARM64):

```bash
target=x86_64-unknown-linux-musl
rustup target add "$target"
# Install musl-dev and musl-tools first.
cargo build --release --locked --target "$target"
strip -s "target/$target/release/epazote"
mkdir -p target/release
ln -sf "../$target/release/epazote" target/release/epazote
cargo generate-rpm
cargo deb --no-build --target "$target"
```

The shared [build-binary](../.github/actions/build-binary/action.yml) and
[linux-packages](../.github/actions/linux-packages/action.yml) actions run these
steps in CI and releases, with pinned cargo-generate-rpm 0.21.0 and cargo-deb 3.8.0.
The RPM is under `target/generate-rpm/`; the DEB is under `target/debian/`.
The pinned cargo-deb 3.8.0 also makes a deprecated compatibility copy under
`target/<target>/debian/`, which CI uses only as a fallback. Checks validate package
metadata, payloads, modes, and Debian control scripts without installing packages.
RPM inspection uses `rpm2archive` and `tar`: Ubuntu 24.04's `rpm2cpio` requires
the legacy archive-size tag omitted by the pinned packager.

Notes:

* the package installs the binary at `/usr/bin/epazote`
* the systemd unit expects the active config at `/etc/epazote/epazote.yml`
* the package installs `/etc/epazote/epazote.env` for config, port, verbosity, and optional OTLP log-export overrides; OTLP support requires building with `--features telemetry`
* the service unit runs as `root` by default so fallback commands can restart local services when needed
* Debian-specific `postinst`, `prerm`, and `postrm` scripts manage service lifecycle;
  RPM currently has no equivalent lifecycle scripts
* the Debian post-install script enables the service and only tries to start it when `/etc/epazote/epazote.yml` exists
* after the Debian service is enabled, systemd will keep retrying startup until the config exists and `epazote` can start successfully

This is intended to be the packaging base for deployments that install `.deb`
or `.rpm` artifacts instead of pushing a raw release binary.
