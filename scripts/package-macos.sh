#!/usr/bin/env bash
# Package and inspect a DMG without starting the app, a node, or a USB connection.
set -euo pipefail
APP="${1:?Usage: package-macos.sh APP OUTPUT.dmg VERSION}"
OUTPUT="${2:?Missing output DMG}"
VERSION="${3:?Missing version}"
[[ -d "$APP/Contents" && ! -e "$OUTPUT" ]] || { echo 'App missing or output already exists.' >&2; exit 1; }
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-(beta|rc)\.[0-9]+)?$ ]] || { echo 'Invalid version.' >&2; exit 1; }
mkdir -p "$(dirname "$OUTPUT")"
OUTPUT="$(cd "$(dirname "$OUTPUT")" && pwd)/$(basename "$OUTPUT")"
STAGE="$(mktemp -d)"
MOUNT="$(mktemp -d)"
MOUNT="$(cd "$MOUNT" && pwd -P)"
ATTACHED=false
cleanup() {
  if $ATTACHED; then
    # Never remove a directory until the disk image is detached.
    hdiutil detach "$MOUNT" || return
  fi
  rm -rf "$STAGE" "$MOUNT"
}
trap cleanup EXIT
codesign --verify --deep --strict "$APP"
ditto "$APP" "$STAGE/Timelord.app"
ln -s /Applications "$STAGE/Applications"
hdiutil create -volname "Timelord $VERSION" -srcfolder "$STAGE" -format UDZO "$OUTPUT"
hdiutil verify "$OUTPUT"
hdiutil attach "$OUTPUT" -readonly -nobrowse -mountpoint "$MOUNT"
ATTACHED=true
[[ "$(readlink "$MOUNT/Applications")" == /Applications ]]
codesign --verify --deep --strict "$MOUNT/Timelord.app"
for binary in "$MOUNT/Timelord.app/Contents/MacOS/TimelordMac" "$MOUNT/Timelord.app/Contents/Helpers/"* "$MOUNT/Timelord.app/Contents/Frameworks/"*.dylib; do
  lipo "$binary" -verify_arch arm64
done
# These commands return before node, status-listener or USB initialization.
env -i HOME="$HOME" PATH=/usr/bin:/bin:/usr/sbin:/sbin "$MOUNT/Timelord.app/Contents/Helpers/timelord-service" --version
env -i HOME="$HOME" PATH=/usr/bin:/bin:/usr/sbin:/sbin "$MOUNT/Timelord.app/Contents/Helpers/hw_vdf_client" --control-version
hdiutil detach "$MOUNT"
ATTACHED=false
