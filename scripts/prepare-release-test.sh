#!/usr/bin/env bash
# Exercise release preparation in a standalone, offline Cargo workspace.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
FIXTURE="$(mktemp -d)"
trap 'rm -rf "$FIXTURE"' EXIT
mkdir -p "$FIXTURE/scripts" "$FIXTURE/crates/example/src" "$FIXTURE/crates/shared/src"
cp "$ROOT/scripts/prepare-release.sh" "$ROOT/scripts/release-version.sh" "$FIXTURE/scripts/"
cat > "$FIXTURE/Cargo.toml" <<'EOF'
[workspace]
members = ["crates/example", "crates/shared"]
resolver = "3"
[workspace.package]
version = "0.1.0"
EOF
cat > "$FIXTURE/crates/example/Cargo.toml" <<'EOF'
[package]
name = "release-fixture"
version.workspace = true
edition = "2024"
[dependencies]
release-shared = { path = "../shared" }
EOF
cat > "$FIXTURE/crates/shared/Cargo.toml" <<'EOF'
[package]
name = "release-shared"
version.workspace = true
edition = "2024"
EOF
echo 'pub fn example() {}' > "$FIXTURE/crates/example/src/lib.rs"
echo 'pub fn shared() {}' > "$FIXTURE/crates/shared/src/lib.rs"
export CARGO_NET_OFFLINE=true
cargo check --manifest-path "$FIXTURE/Cargo.toml"
cp "$FIXTURE/Cargo.toml" "$FIXTURE/original.toml"
cp "$FIXTURE/Cargo.lock" "$FIXTURE/original.lock"
for version in 0.1.0 v0.2.0 01.2.3 0.2.0-alpha.1 0.2.0+local '0.2.0;exit 0' $'0.2.0\n' ''; do
  if bash "$FIXTURE/scripts/prepare-release.sh" "$version" > "$FIXTURE/log" 2>&1; then
    echo "Unexpectedly accepted version: $version" >&2; exit 1
  fi
  cmp "$FIXTURE/original.toml" "$FIXTURE/Cargo.toml"
  cmp "$FIXTURE/original.lock" "$FIXTURE/Cargo.lock"
done
for version in 0.2.0-beta.1 0.2.0-rc.1 0.2.0; do
  bash "$FIXTURE/scripts/prepare-release.sh" "$version"
  [[ "$(bash "$FIXTURE/scripts/release-version.sh" "v$version")" == "$version" ]]
  sed "s/0\.1\.0/$version/g" "$FIXTURE/original.lock" > "$FIXTURE/expected.lock"
  cmp "$FIXTURE/expected.lock" "$FIXTURE/Cargo.lock"
  cargo check --manifest-path "$FIXTURE/Cargo.toml" --locked
done
echo 'Release preparation checks passed.'
