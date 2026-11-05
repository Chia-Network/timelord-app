#!/usr/bin/env bash
# Pure fixtures: never invokes Xcode or compiles artwork.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
source "$ROOT/scripts/bundle-icon.sh"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/icon/AppIcon.icon"
printf 'flat-icon\n' > "$work/icon/AppIcon.icns"
fixture=missing
compiled=0
xcrun() {
  if [[ "$1" == --find ]]; then [[ "$fixture" != missing ]]; return; fi
  if [[ "$2" == --version ]]; then
    case "$fixture" in
      broken-probe) return 1 ;;
      unknown) printf '<plist/>' ;;
      old) printf '<key>short-bundle-version</key>\n<string>16.4</string>\n' ;;
      *) printf '<key>short-bundle-version</key>\n<string>26.3</string>\n' ;;
    esac
    return
  fi
  compiled=1
  [[ "$fixture" != compiler-error ]] || { echo 'invalid artwork' >&2; return 1; }
  while [[ "$1" != --compile ]]; do shift; done
  printf 'layered\n' > "$2/Assets.car"
  printf 'compiled-icon\n' > "$2/AppIcon.icns"
}
for fixture in missing old current broken-probe unknown compiler-error; do
  resources="$work/$fixture"
  mkdir -p "$resources"
  compiled=0
  if bundle_app_icon "$work/icon" "$resources" 14.0 >"$work/log" 2>&1; then
    case "$fixture" in
      missing|old)
        [[ "$compiled" -eq 0 ]]
        cmp "$work/icon/AppIcon.icns" "$resources/AppIcon.icns" ;;
      current)
        [[ "$compiled" -eq 1 && -f "$resources/Assets.car" ]]
        [[ "$(cat "$resources/AppIcon.icns")" == compiled-icon ]] ;;
      *) echo "Unexpected fallback/success for $fixture" >&2; exit 1 ;;
    esac
  else
    case "$fixture" in
      broken-probe|unknown|compiler-error) [[ ! -f "$resources/AppIcon.icns" ]] ;;
      *) cat "$work/log" >&2; exit 1 ;;
    esac
  fi
done
printf 'Icon capability and fallback fixtures passed.\n'
