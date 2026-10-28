#!/bin/bash
# Validate generated registration only; never install or load a launchd job.
set -euo pipefail
[[ "$(uname -s)" == Darwin ]] || exit 0
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
STAGE="$(mktemp -d /private/tmp/timelord-installer-test.XXXXXX)"
trap 'rm -rf "$STAGE"' EXIT
APP="$STAGE/My & Timelord.app"
mkdir -p "$APP/Contents/Helpers"
ln -s "$ROOT/build/rust/debug/timelord-supervisor" "$APP/Contents/Helpers/timelord-supervisor"
ln -s "$ROOT/build/rust/debug/timelord-service" "$APP/Contents/Helpers/timelord-service"
printf 'schema_version=1\n' > "$STAGE/config & settings.toml"
bash "$ROOT/scripts/install-service.sh" plan --app "$APP" --config "$STAGE/config & settings.toml" --user "$(id -un)" > "$STAGE/job.plist"
plutil -lint "$STAGE/job.plist"
[[ "$(plutil -extract UserName raw -o - "$STAGE/job.plist")" == "$(id -un)" ]]
[[ "$(plutil -extract ProgramArguments.0 raw -o - "$STAGE/job.plist")" == "$APP/Contents/Helpers/timelord-supervisor" ]]
[[ "$(plutil -extract ProgramArguments.3 raw -o - "$STAGE/job.plist")" == "$STAGE/config & settings.toml" ]]
[[ "$(plutil -extract KeepAlive raw -o - "$STAGE/job.plist")" == true ]]
if bash "$ROOT/scripts/install-service.sh" plan --app "$APP" --config "$STAGE/config & settings.toml" --user root >/dev/null 2>&1; then exit 1; fi
ln -s "$STAGE/config & settings.toml" "$STAGE/link.toml"
if bash "$ROOT/scripts/install-service.sh" plan --app "$APP" --config "$STAGE/link.toml" --user "$(id -un)" >/dev/null 2>&1; then exit 1; fi
[[ ! -e "$STAGE/.config & settings.toml-supervisor" ]]
echo 'Installer plan inputs passed without loading a system job.'
