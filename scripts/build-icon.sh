#!/usr/bin/env bash
# Regenerate apps/macos/AppIcon/AppIcon.icns from its SVG sources. Only needed after
# editing the artwork; the app bundle uses the committed .icns. Requires rsvg-convert.
set -euo pipefail
PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ICON_DIR="$PROJECT_ROOT/apps/macos/AppIcon"
command -v rsvg-convert >/dev/null || { echo 'rsvg-convert is required (librsvg).' >&2; exit 1; }
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
ICONSET="$WORK/AppIcon.iconset"
mkdir "$ICONSET"
render() { rsvg-convert -w "$2" -h "$2" "$ICON_DIR/$1" -o "$ICONSET/$3"; }
# The simplified artwork stays legible at 16 and 32 px.
render AppIcon-small.svg 16 icon_16x16.png
render AppIcon-small.svg 32 icon_16x16@2x.png
render AppIcon-small.svg 32 icon_32x32.png
render AppIcon.svg 64 icon_32x32@2x.png
render AppIcon.svg 128 icon_128x128.png
render AppIcon.svg 256 icon_128x128@2x.png
render AppIcon.svg 256 icon_256x256.png
render AppIcon.svg 512 icon_256x256@2x.png
render AppIcon.svg 512 icon_512x512.png
render AppIcon.svg 1024 icon_512x512@2x.png
iconutil -c icns "$ICONSET" -o "$ICON_DIR/AppIcon.icns"
printf 'Built %s\n' "$ICON_DIR/AppIcon.icns"
