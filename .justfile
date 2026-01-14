test: build-bin clippy fmt
  cargo test

build-bin:
  cargo build --bins

clippy:
  cargo clippy --all-targets --all-features

fmt:
  cargo fmt --all -- --check

coverage:
  cargo llvm-cov --all-features --workspace
