#!/usr/bin/env bash
# Update the source version and let Cargo refresh inherited lockfile versions.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
VERSION="${1:?Usage: prepare-release.sh VERSION}"
NUMBER='(0|[1-9][0-9]*)'
[[ $# == 1 && "$VERSION" =~ ^${NUMBER}\.${NUMBER}\.${NUMBER}(-(beta|rc)\.${NUMBER})?$ ]] || {
  echo 'Expected MAJOR.MINOR.PATCH, optionally followed by -beta.N or -rc.N.' >&2
  exit 1
}
CURRENT="$(bash scripts/release-version.sh)"
[[ "$VERSION" != "$CURRENT" ]] || { echo "Source version is already $VERSION." >&2; exit 1; }
TEMP="$(mktemp)"
trap 'rm -f "$TEMP"' EXIT
sed "s/^version = \"$CURRENT\"$/version = \"$VERSION\"/" Cargo.toml > "$TEMP"
cat "$TEMP" > Cargo.toml
bash scripts/release-version.sh "v$VERSION"
cargo check --workspace
