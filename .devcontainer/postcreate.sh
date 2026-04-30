#!/usr/bin/env bash
set -euo pipefail

# Fix mounted Cargo volumes upfront.
sudo chown -R "$(id -u):$(id -g)" /home/vscode/.cargo

sudo apt-get update
sudo apt-get install -y \
    pkg-config libssl-dev \
    curl ca-certificates git

sudo chsh -s /usr/bin/zsh vscode

export PATH="${HOME}/.local/bin:${PATH}"
export MISE_YES=1

if ! command -v mise >/dev/null 2>&1; then
    curl -fsSL https://mise.run | sh
fi

mise trust "${PWD}/mise.toml"
mise install

rustup component add rustfmt clippy rust-analyzer

bash .devcontainer/setup-zsh.sh
