#!/bin/bash
# Install an explicitly approved system job; never run the proof worker as root.
set -euo pipefail
export PATH=/usr/bin:/bin:/usr/sbin:/sbin
MODE="${1:-}"; shift || true
APP=""; CONFIG=""; SERVICE_USER=""
while (($#)); do
  case "$1" in
    --app) APP="${2:?Missing app}"; shift 2 ;;
    --config) CONFIG="${2:?Missing configuration}"; shift 2 ;;
    --user) SERVICE_USER="${2:?Missing deployment account}"; shift 2 ;;
    *) echo 'Usage: install-service.sh plan|install|remove --app APP --config CONFIG --user USER' >&2; exit 2 ;;
  esac
done
[[ "$(uname -s)" == Darwin && "$MODE" =~ ^(plan|install|remove)$ ]] || { echo 'Expected macOS and plan, install or remove.' >&2; exit 2; }
[[ -n "$APP" && -n "$CONFIG" && "$SERVICE_USER" =~ ^[A-Za-z_][A-Za-z0-9_.-]*$ ]] || { echo 'Invalid installation arguments.' >&2; exit 2; }
SERVICE_UID="$(id -u "$SERVICE_USER")"
[[ "$SERVICE_UID" != 0 ]] || { echo 'Select a non-root deployment account.' >&2; exit 1; }
[[ -d "$APP/Contents" && ! -L "$APP" && -f "$CONFIG" && ! -L "$CONFIG" ]] || { echo 'App and configuration must exist and must not be links.' >&2; exit 1; }
APP="$(cd "$APP" && pwd -P)"
CONFIG="$(cd "$(dirname "$CONFIG")" && pwd -P)/$(basename "$CONFIG")"
[[ "$(stat -f %u "$CONFIG")" == "$SERVICE_UID" ]] || { echo 'Configuration must belong to the service account.' >&2; exit 1; }
HELPER="$APP/Contents/Helpers/timelord-supervisor"
[[ -x "$HELPER" && -x "$APP/Contents/Helpers/timelord-service" ]] || { echo 'Required helpers are missing.' >&2; exit 1; }
run_as_owner() {
  if [[ "$EUID" == "$SERVICE_UID" ]]; then "$@"; else sudo -H -u "$SERVICE_USER" "$@"; fi
}
CONFIG_JSON="$(run_as_owner "$APP/Contents/Helpers/timelord-service" configuration --config "$CONFIG")" || exit 1
MONITOR="$(printf '%s' "$CONFIG_JSON" | plutil -extract service.supervisor_observability_listen raw -o - -)" || exit 1
NETWORK="$(printf '%s' "$CONFIG_JSON" | plutil -extract node.network raw -o - -)" || exit 1
if [[ "$MODE" == plan ]]; then
  run_as_owner "$HELPER" launchd-plist --app "$APP" --config "$CONFIG" --user "$SERVICE_USER" --monitor-listen "$MONITOR" --network "$NETWORK" || exit $?
  exit 0
fi
[[ "$EUID" == 0 && "$APP" == /Applications/Timelord.app ]] || { echo 'Installation requires administrator approval and /Applications/Timelord.app.' >&2; exit 1; }
codesign --verify --deep --strict "$APP" || exit 1
LABEL=net.chia.timelord.supervisor
PLIST="/Library/LaunchDaemons/$LABEL.plist"
if [[ -e "$PLIST" || -L "$PLIST" ]]; then
  [[ -f "$PLIST" && ! -L "$PLIST" && "$(stat -f %u "$PLIST")" == 0 && "$(stat -f %l "$PLIST")" == 1 ]] || { echo 'Unsafe registration path.' >&2; exit 1; }
fi
if [[ "$MODE" == remove ]]; then
  # Removal is an explicit administrative action, not a rewrite of Run/Pause.
  launchctl bootout "system/$LABEL" 2>/dev/null || true
  rm -f "$PLIST"
  exit 0
fi
# Validate everything possible before interrupting an existing registered job.
STAGE="$(mktemp -d /private/tmp/timelord-registration.XXXXXX)" || exit 1
WAS_LOADED=false; STOPPED=false; SUCCEEDED=false; HAD_PLIST=false
if launchctl print "system/$LABEL" >/dev/null 2>&1; then WAS_LOADED=true; fi
if [[ -f "$PLIST" ]]; then cp -p "$PLIST" "$STAGE/previous.plist" || exit 1; HAD_PLIST=true; fi
cleanup() {
  result=$?
  if ! $SUCCEEDED && $STOPPED; then
    launchctl bootout "system/$LABEL" 2>/dev/null || true
    if $HAD_PLIST; then
      cp -p "$STAGE/previous.plist" "$PLIST"
      if $WAS_LOADED; then launchctl bootstrap system "$PLIST" || echo 'Previous registration could not be restored.' >&2; fi
    else
      rm -f "$PLIST"
    fi
  fi
  rm -rf "$STAGE"
  exit "$result"
}
trap cleanup EXIT
run_as_owner "$HELPER" launchd-plist --app "$APP" --config "$CONFIG" --user "$SERVICE_USER" --monitor-listen "$MONITOR" --network "$NETWORK" > "$STAGE/new.plist" || exit 1
plutil -lint "$STAGE/new.plist" || exit 1
LISTEN="$(run_as_owner "$APP/Contents/Helpers/timelord-service" configuration --config "$CONFIG" | plutil -extract service.status_listen raw -o - -)"
PORT="${LISTEN##*:}"
[[ "$PORT" =~ ^[0-9]+$ && "$PORT" -gt 0 && "$PORT" -le 65535 ]] || { echo 'Invalid status port.' >&2; exit 1; }
LOG="$(dirname "$CONFIG")/supervisor.log"
if [[ -e "$LOG" || -L "$LOG" ]]; then
  [[ -f "$LOG" && ! -L "$LOG" && "$(stat -f %u "$LOG")" == "$SERVICE_UID" && "$(stat -f %l "$LOG")" == 1 ]] || { echo 'Unsafe log path.' >&2; exit 1; }
else
  install -o "$SERVICE_USER" -m 600 /dev/null "$LOG" || exit 1
fi
if $WAS_LOADED; then launchctl bootout "system/$LABEL" || exit 1; fi
STOPPED=true
run_as_owner "$HELPER" check-owner --config "$CONFIG" || exit 1
# Pre-supervisor GUI versions have no ownership lock. Never kill arbitrary
# processes by name or assume an occupied port belongs to this installation.
if lsof -nP -iTCP:"$PORT" -sTCP:LISTEN >/dev/null 2>&1; then
  echo 'Pause the existing foreground Timelord before installing background operation.' >&2
  exit 1
fi
install -o root -g wheel -m 644 "$STAGE/new.plist" "$STAGE/installed.plist" || exit 1
mv "$STAGE/installed.plist" "$PLIST" || exit 1
launchctl enable "system/$LABEL" || exit 1
launchctl bootstrap system "$PLIST" || exit 1
SUCCEEDED=true
echo 'Installed background supervision. Process presence does not establish proof readiness.'
