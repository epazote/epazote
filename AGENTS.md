# Repository Guidelines

## Project Structure & Module Organization

Epazote is a Rust CLI and library. Core code lives in `src/`, with the executable entrypoint at `src/bin/epazote.rs`. CLI wiring is under `src/cli/`; service execution and fallback logic live in `src/cli/actions/`, configuration parsing in `src/cli/config.rs`, and argument parsing in `src/cli/commands/`. Integration tests are in `tests/integration.rs`. Packaging and service assets are in `contrib/`. Example configuration is `epazote.yml`, and release notes are kept in `CHANGELOG.md`.

## Build, Test, and Development Commands

Prefer running Rust commands inside the `epazote` DevPod:

```sh
devpod ssh epazote --command "bash -lc 'cd /workspaces/epazote && cargo test'"
```

Common commands:

```sh
cargo build --bins                         # build executable targets
cargo test                                 # run unit and integration tests
cargo fmt --all -- --check                 # verify formatting
cargo clippy --all-targets --all-features  # lint all targets
cargo llvm-cov --all-features --workspace  # coverage, if installed
just test                                  # build, clippy, fmt, then test
```

## Coding Style & Naming Conventions

Use standard Rust formatting with `cargo fmt`. The crate enforces strict lints in `Cargo.toml`, including Clippy `pedantic`, `unwrap_used`, `expect_used`, `panic`, and `indexing_slicing`. Keep fallible code returning `anyhow::Result` where existing modules do. Use snake_case for functions, modules, tests, and variables; use PascalCase for types and enums. Keep comments sparse and focused on non-obvious behavior.

## Testing Guidelines

Unit tests live near the code they exercise under `#[cfg(test)]`; integration behavior belongs in `tests/integration.rs`. Name tests after the behavior being verified, for example `test_handle_http_response_expect_body_not`. Add regression tests for config parsing and runtime behavior when changing `epazote.yml` semantics. Run `cargo test` and Clippy before handing off.

## Commit & Pull Request Guidelines

Recent history uses concise messages such as `3.4.0`, `Release 3.3.1: ...`, and short imperative summaries. Keep commits focused and mention the user-facing behavior when relevant. Pull requests should include a clear description, linked issues, config examples for new features, and the verification commands run. Update `CHANGELOG.md` and docs when changing public configuration or CLI behavior.

## Agent-Specific Instructions

Edit on the host checkout, but compile and test in DevPod. Do not install Rust tooling on the host. Keep generated artifacts such as `target/`, VitePress `dist/`, and host `node_modules/` out of commits.
