#!/bin/bash
# Validate generated registration only; never install or load a system job.
set -euo pipefail
[[ "$(uname -s)" == Darwin ]] || exit 0
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
STAGE="$(mktemp -d /private/tmp/timelord-installer-test.XXXXXX)"
trap 'rm -rf "$STAGE"' EXIT
USER_NAME="$(id -un)"
APP="$STAGE/My & Timelord.app"
mkdir -p "$APP/Contents/Helpers"
ln -s "$ROOT/build/rust/debug/timelord-supervisor" "$APP/Contents/Helpers/timelord-supervisor"
ln -s "$ROOT/build/rust/debug/timelord-service" "$APP/Contents/Helpers/timelord-service"

plan() {
  bash "$ROOT/scripts/install-service.sh" plan --app "$APP" --config "$1" --user "$USER_NAME"
}

assert_job() {
  local plist="$1" config="$2" network="$3"
  plutil -lint "$plist"
  [[ "$(plutil -extract UserName raw -o - "$plist")" == "$USER_NAME" ]]
  [[ "$(plutil -extract ProgramArguments.0 raw -o - "$plist")" == "$APP/Contents/Helpers/timelord-supervisor" ]]
  [[ "$(plutil -extract ProgramArguments.3 raw -o - "$plist")" == "$config" ]]
  [[ "$(plutil -extract ProgramArguments.7 raw -o - "$plist")" == "$network" ]]
  [[ "$(plutil -extract KeepAlive raw -o - "$plist")" == true ]]
}

# Legacy single-node files keep a top-level [node] table.
printf 'schema_version=1\n' > "$STAGE/config & settings.toml"
plan "$STAGE/config & settings.toml" > "$STAGE/job.plist"
assert_job "$STAGE/job.plist" "$STAGE/config & settings.toml" mainnet

# The active profile's network is used when [[connections.profiles]] exist.
cat > "$STAGE/profiles.toml" <<'TOML'
schema_version = 1
[connections]
active_id = "testnet"
[[connections.profiles]]
id = "mainnet"
name = "Mainnet"
node = { host = "127.0.0.1", port = 8444, network = "mainnet" }
[[connections.profiles]]
id = "testnet"
name = "Testnet11"
node = { host = "127.0.0.1", port = 58444, network = "testnet11" }
TOML
plan "$STAGE/profiles.toml" > "$STAGE/profiles.plist"
assert_job "$STAGE/profiles.plist" "$STAGE/profiles.toml" testnet11

# A credentialed [node] is moved into connections.profiles on load, so the
# configuration JSON has no top-level node.network.
cat > "$STAGE/legacy-node.toml" <<'TOML'
schema_version = 1
[node]
host = "10.75.1.228"
port = 8444
network = "testnet11"
certificate = "client.crt"
private_key = "client.key"
ca_certificate = "ca.crt"
TOML
plan "$STAGE/legacy-node.toml" > "$STAGE/legacy.plist"
assert_job "$STAGE/legacy.plist" "$STAGE/legacy-node.toml" testnet11

# Missing network: configuration output has an active profile but no network.
STUB="$STAGE/Stub.app"
mkdir -p "$STUB/Contents/Helpers"
ln -s "$ROOT/build/rust/debug/timelord-supervisor" "$STUB/Contents/Helpers/timelord-supervisor"
cat > "$STUB/Contents/Helpers/timelord-service" <<'SH'
#!/bin/bash
printf '%s' '{"schema_version":1,"connections":{"active_id":"connection-1","profiles":[{"id":"connection-1","name":"Mainnet","node":{"host":"localhost","port":8444}}]},"service":{"status_listen":"127.0.0.1:9257","supervisor_observability_listen":"127.0.0.1:9259"},"hardware":{}}'
SH
chmod +x "$STUB/Contents/Helpers/timelord-service"
printf 'schema_version=1\n' > "$STAGE/missing.toml"
if bash "$ROOT/scripts/install-service.sh" plan --app "$STUB" --config "$STAGE/missing.toml" --user "$USER_NAME" >"$STAGE/missing.plist" 2>"$STAGE/missing.err"; then
  exit 1
fi
grep -q 'no network on the active connection profile or at node.network' "$STAGE/missing.err"
[[ ! -s "$STAGE/missing.plist" ]]

if bash "$ROOT/scripts/install-service.sh" plan --app "$APP" --config "$STAGE/config & settings.toml" --user root >/dev/null 2>&1; then exit 1; fi
ln -s "$STAGE/config & settings.toml" "$STAGE/link.toml"
if bash "$ROOT/scripts/install-service.sh" plan --app "$APP" --config "$STAGE/link.toml" --user "$USER_NAME" >/dev/null 2>&1; then exit 1; fi
[[ ! -e "$STAGE/.config & settings.toml-supervisor" ]]
echo 'Installer plan inputs passed without loading a system job.'
