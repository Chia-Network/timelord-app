#!/usr/bin/env bash
# Sign from the inside out; no --deep signing or relaxed library validation.
set -euo pipefail
APP="${1:?Usage: sign-macos.sh APP}"
: "${SIGNING_IDENTITY:?Missing Developer ID Application identity hash}"
: "${APPLE_TEAM_ID:?Missing Apple team ID}"
: "${RELEASE_VERSION:?Missing release version}"
: "${SOURCE_REVISION:?Missing source revision}"
[[ "$RELEASE_VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-(beta|rc)\.[0-9]+)?$ && "$SOURCE_REVISION" =~ ^[0-9a-f]{40}$ ]] || { echo 'Invalid release metadata.' >&2; exit 1; }
PLIST="$APP/Contents/Info.plist"
BASE_VERSION="${RELEASE_VERSION%%-*}"
/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $BASE_VERSION" "$PLIST"
/usr/libexec/PlistBuddy -c "Set :CFBundleVersion $BASE_VERSION" "$PLIST"
/usr/libexec/PlistBuddy -c "Add :TimelordReleaseVersion string $RELEASE_VERSION" "$PLIST"
/usr/libexec/PlistBuddy -c "Add :TimelordSourceRevision string $SOURCE_REVISION" "$PLIST"
# Use the same identity for FTDI/GMP, native client, coordinator, and UI.
for binary in "$APP/Contents/Frameworks/"*.dylib "$APP/Contents/Helpers/"* "$APP/Contents/MacOS/TimelordMac"; do
  codesign --force --sign "$SIGNING_IDENTITY" --timestamp --options runtime "$binary"
done
codesign --force --sign "$SIGNING_IDENTITY" --timestamp --options runtime "$APP"
codesign --verify --deep --strict "$APP"
for binary in "$APP/Contents/Frameworks/"*.dylib "$APP/Contents/Helpers/"* "$APP"; do
  details="$(codesign --display --verbose=4 "$binary" 2>&1)"
  grep -Fq "TeamIdentifier=$APPLE_TEAM_ID" <<< "$details"
  grep -Fq 'Authority=Developer ID Application:' <<< "$details"
  grep -Fq 'runtime' <<< "$details"
done
