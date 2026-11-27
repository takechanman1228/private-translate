#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
scripts/local-dev.sh fmt --all --check
scripts/local-dev.sh clippy --workspace --all-targets --locked -- -D warnings
scripts/local-dev.sh clippy --workspace --all-targets --release --locked -- -D warnings
scripts/local-dev.sh test --workspace --release --locked
