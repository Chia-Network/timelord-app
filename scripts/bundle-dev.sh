#!/usr/bin/env bash
# Assemble a complete ad-hoc-signed developer app without starting any service.
set -euo pipefail
export MACOSX_DEPLOYMENT_TARGET=14.0
PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$PROJECT_ROOT"
source "$PROJECT_ROOT/scripts/macho-load-commands.sh"
[[ "$(uname -s)" == Darwin ]] || { echo 'The Mac app bundle must be built on macOS.' >&2; exit 1; }
CLIENT="${TIMELORD_ASIC_CLIENT:-$PROJECT_ROOT/build/controlled-native/hw_vdf_client}"
[[ -x "$CLIENT" ]] || { echo 'Build the native client first: bash scripts/build-native-control.sh' >&2; exit 1; }
DRIVERS="${CHIAVDF_DRIVERS:-$(dirname "$CLIENT")/hw/libft4222}"
for name in libft4222.dylib libftd2xx.dylib; do
  [[ -f "$DRIVERS/$name" ]] || { echo "Missing FTDI library: $DRIVERS/$name" >&2; exit 1; }
done
cargo build --release --locked -p timelord-service -p timelord-supervisor
swift build --configuration release --package-path apps/macos --scratch-path build/swift
SWIFT_BIN_DIR="$(swift build --configuration release --package-path apps/macos --scratch-path build/swift --show-bin-path)"
APP="${TIMELORD_APP_BUNDLE:-$PROJECT_ROOT/build/Timelord.app}"
# Never replace an existing app's mapped helper or libraries during a live run.
[[ ! -e "$APP" ]] || { echo "Bundle already exists: $APP. Choose a fresh TIMELORD_APP_BUNDLE path." >&2; exit 1; }
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Helpers" "$APP/Contents/Frameworks" "$APP/Contents/Resources/ThirdParty"
cp "$SWIFT_BIN_DIR/TimelordMac" "$APP/Contents/MacOS/TimelordMac"
cp build/rust/release/timelord-service "$APP/Contents/Helpers/timelord-service"
cp build/rust/release/timelord-supervisor "$APP/Contents/Helpers/timelord-supervisor"
cp scripts/install-service.sh "$APP/Contents/Resources/install-service.sh"
cp "$CLIENT" "$APP/Contents/Helpers/hw_vdf_client"
cp "$DRIVERS/libft4222.dylib" "$DRIVERS/libftd2xx.dylib" "$APP/Contents/Frameworks/"
# GMP is dynamically linked by chiavdf. Include the actual linked versions.
dependencies() { otool -L "$1" | macho_linked_libraries; }
while IFS= read -r dependency; do
  case "$(basename "$dependency")" in
    libgmp*.dylib)
      [[ -f "$dependency" ]] || { echo "Cannot resolve GMP dependency: $dependency" >&2; exit 1; }
      cp -L "$dependency" "$APP/Contents/Frameworks/"
      GMP_PREFIX="$(dirname "$(dirname "$dependency")")"
      cp "$GMP_PREFIX/COPYING" "$APP/Contents/Resources/ThirdParty/GMP-COPYING"
      cp "$GMP_PREFIX/COPYING.LESSERv3" "$APP/Contents/Resources/ThirdParty/GMP-COPYING.LESSERv3"
      ;;
  esac
done < <(dependencies "$CLIENT")
# Remove all build-machine load paths. Each non-system dependency must be bundled.
for binary in "$APP/Contents/Frameworks/"*.dylib "$APP/Contents/Helpers/hw_vdf_client"; do
  chmod u+w "$binary"
  while IFS= read -r dependency; do
    case "$dependency" in /usr/lib/*|/System/Library/*) continue ;; esac
    name="$(basename "$dependency")"
    [[ -f "$APP/Contents/Frameworks/$name" ]] || { echo "Unbundled dependency: $dependency" >&2; exit 1; }
    if [[ "$binary" == "$APP/Contents/Frameworks/"* ]]; then
      [[ "$name" == "$(basename "$binary")" ]] && continue
      replacement="@loader_path/$name"
    else
      replacement="@executable_path/../Frameworks/$name"
    fi
    install_name_tool -change "$dependency" "$replacement" "$binary"
  done < <(dependencies "$binary")
  # No staged source paths or Homebrew search paths remain in the shipped files.
  while IFS= read -r rpath; do install_name_tool -delete_rpath "$rpath" "$binary"; done < <(otool -l "$binary" | macho_rpaths)
  if [[ "$binary" == "$APP/Contents/Frameworks/"* ]]; then install_name_tool -id "@rpath/$(basename "$binary")" "$binary"; fi
  codesign --force --sign - "$binary"
done
# The layered icon needs Xcode 26's actool: Assets.car carries the macOS 26 appearances and
# actool also writes a fallback AppIcon.icns. With only the command line tools, ship the flat icon.
if xcrun --find actool >/dev/null 2>&1; then
  ICON_LOG="$(mktemp)"
  # actool resolves relative input paths against the output folder, so pass absolute paths.
  if ! xcrun actool "$PROJECT_ROOT/apps/macos/AppIcon/AppIcon.icon" --compile "$APP/Contents/Resources" \
      --platform macosx --minimum-deployment-target "$MACOSX_DEPLOYMENT_TARGET" --app-icon AppIcon \
      --output-partial-info-plist "$ICON_LOG.plist" --output-format human-readable-text --errors --warnings >"$ICON_LOG" 2>&1 \
    || [[ ! -f "$APP/Contents/Resources/Assets.car" || ! -f "$APP/Contents/Resources/AppIcon.icns" ]]; then
    cat "$ICON_LOG" >&2
    echo 'Could not compile apps/macos/AppIcon/AppIcon.icon (requires Xcode 26 or newer).' >&2
    exit 1
  fi
  rm -f "$ICON_LOG" "$ICON_LOG.plist"
else
  echo 'actool not found (it ships with Xcode, not the command line tools); bundling the flat AppIcon.icns.' >&2
  cp apps/macos/AppIcon/AppIcon.icns "$APP/Contents/Resources/AppIcon.icns"
fi
cp LICENSE "$APP/Contents/Resources/LICENSE"
cp integrations/dg-xch/LICENSE "$APP/Contents/Resources/ThirdParty/dg-xch-LICENSE"
cp integrations/chiavdf/LICENSE "$APP/Contents/Resources/ThirdParty/chiavdf-LICENSE"
cp integrations/chiavdf/NOTICE "$APP/Contents/Resources/ThirdParty/chiavdf-NOTICE"
cp integrations/dg-xch/NOTICE "$APP/Contents/Resources/ThirdParty/dg-xch-NOTICE"
cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleExecutable</key><string>TimelordMac</string>
  <key>CFBundleIdentifier</key><string>net.chia.timelord.mac</string>
  <key>CFBundleName</key><string>Timelord</string>
  <key>CFBundleDisplayName</key><string>Timelord</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundleIconName</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>CFBundleVersion</key><string>1</string>
  <key>LSMinimumSystemVersion</key><string>14.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict></plist>
PLIST
for binary in "$APP/Contents/MacOS/TimelordMac" "$APP/Contents/Helpers/timelord-service" "$APP/Contents/Helpers/timelord-supervisor"; do
  while IFS= read -r dependency; do
    case "$dependency" in /usr/lib/*|/System/Library/*) ;; *) echo "Unbundled application dependency: $dependency" >&2; exit 1 ;; esac
  done < <(dependencies "$binary")
done
codesign --force --sign - "$APP/Contents/Helpers/timelord-service"
codesign --force --sign - "$APP/Contents/Helpers/timelord-supervisor"
codesign --force --sign - "$APP"
codesign --verify --deep --strict "$APP"
"$APP/Contents/Helpers/hw_vdf_client" --control-version
printf 'Built %s\n' "$APP"
