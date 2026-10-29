#!/usr/bin/env bash
set -euo pipefail
PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$PROJECT_ROOT"
bash scripts/bundle-inputs-test.sh
bash scripts/release-inputs-test.sh
bash scripts/installer-remove-test.sh
mkdir -p build
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
TIMELORD_TEST_EXPOSITION="$PROJECT_ROOT/build/observability.prom" cargo test --workspace --locked
promtool check metrics < build/observability.prom
promtool check rules monitoring/alerts.yml
promtool test rules monitoring/alerts.test.yml
if [[ "$(uname -s)" == Darwin ]]; then
  cargo build --locked -p timelord-service -p timelord-supervisor
  bash scripts/installer-inputs-test.sh
  swift build --package-path apps/macos --scratch-path build/swift
  swift test --package-path apps/macos --scratch-path build/swift
fi
