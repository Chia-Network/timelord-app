#!/usr/bin/env bash
# Release input failures must happen before signing or producing an artifact.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VERSION="$(bash "$ROOT/scripts/release-version.sh")"
[[ "$(bash "$ROOT/scripts/release-version.sh" "v$VERSION")" == "$VERSION" ]]
for tag in main v999999.0.0 'v0.1.0;exit 0' v01.1.0 v0.1.0+local v0.1.0-unknown.1; do
  if bash "$ROOT/scripts/release-version.sh" "$tag" >/dev/null 2>&1; then
    echo "Unexpectedly accepted release tag: $tag" >&2; exit 1
  fi
done
LOG="$(mktemp)"
trap 'rm -f "$LOG"' EXIT
if env -u SIGNING_IDENTITY bash "$ROOT/scripts/sign-macos.sh" /missing/Timelord.app >"$LOG" 2>&1; then
  echo 'Signing without an identity succeeded.' >&2; exit 1
fi
grep -q 'Missing Developer ID Application identity' "$LOG"
echo 'Release tag and missing-credential checks passed.'
