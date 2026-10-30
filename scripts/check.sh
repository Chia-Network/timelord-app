#!/usr/bin/env bash
set -euo pipefail
PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$PROJECT_ROOT"
bash scripts/bundle-inputs-test.sh
bash scripts/bundle-icon-test.sh
bash scripts/release-inputs-test.sh
mkdir -p build
c++ -std=c++17 -Wall -Wextra -Werror integrations/chiavdf/tests/live_control_test.cpp -o build/native-control-test
build/native-control-test
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
if [[ "$(uname -s)" == Darwin ]]; then
  cargo build --locked -p timelord-service
  swift build --package-path apps/macos --scratch-path build/swift
  swift test --package-path apps/macos --scratch-path build/swift
fi
