#!/bin/bash
# Exercise dispatch with fixture commands and paths; never touch launchd/root files.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
STAGE="$(mktemp -d /private/tmp/timelord-remove-test.XXXXXX)"
trap 'rm -rf "$STAGE"' EXIT
mkdir "$STAGE/bin"
# Only destinations, sanitized PATH and privilege check differ in this test copy.
sed -e 's|export PATH=/usr/bin:/bin:/usr/sbin:/sbin|export PATH="$TEST_BIN:/usr/bin:/bin"|' \
    -e "s|/Library/LaunchDaemons/|$STAGE/|" \
    -e 's|"$EUID"|"0"|g' "$ROOT/scripts/install-service.sh" > "$STAGE/installer"
cat > "$STAGE/bin/uname" <<'SH'
#!/bin/bash
echo Darwin
SH
cat > "$STAGE/bin/stat" <<'SH'
#!/bin/bash
case "$2" in %u) echo 0 ;; %l) echo 1 ;; *) exit 1 ;; esac
SH
cat > "$STAGE/bin/codesign" <<'SH'
#!/bin/bash
echo 'codesign must not run during removal' >&2
exit 99
SH
cat > "$STAGE/bin/launchctl" <<'SH'
#!/bin/bash
printf '%s\n' "$1" >> "$TEST_ACTIONS"
case "$1" in
  print) exit "${TEST_PRINT_STATUS:-0}" ;;
  bootout) exit "${TEST_BOOTOUT_STATUS:-0}" ;;
  *) exit 99 ;;
esac
SH
chmod +x "$STAGE/bin/"*
export TEST_BIN="$STAGE/bin" TEST_ACTIONS="$STAGE/actions"
PLIST="$STAGE/net.chia.timelord.supervisor.plist"
for scenario in omitted missing damaged; do
  touch "$PLIST"; : > "$TEST_ACTIONS"
  case "$scenario" in
    omitted) bash "$STAGE/installer" remove ;;
    missing) bash "$STAGE/installer" remove --app "$STAGE/missing.app" --config "$STAGE/missing.toml" --user missing ;;
    damaged) mkdir -p "$STAGE/broken.app/Contents"; bash "$STAGE/installer" remove --app "$STAGE/broken.app" --config "$STAGE/missing.toml" --user missing ;;
  esac
  [[ ! -e "$PLIST" && "$(cat "$TEST_ACTIONS")" == $'print\nbootout' ]]
done
touch "$PLIST"
if TEST_BOOTOUT_STATUS=1 bash "$STAGE/installer" remove; then exit 1; fi
[[ -f "$PLIST" ]]
TEST_PRINT_STATUS=1 bash "$STAGE/installer" remove
[[ ! -e "$PLIST" ]]
ln -s "$STAGE/missing" "$PLIST"
if bash "$STAGE/installer" remove; then exit 1; fi
[[ -L "$PLIST" ]]
echo 'Installer removal fixtures passed without system changes.'
