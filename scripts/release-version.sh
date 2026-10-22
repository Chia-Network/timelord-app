#!/usr/bin/env bash
# Validate release tags against the source version; no network or build required.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SOURCE_VERSION="$(sed -n 's/^version = "\([^"]*\)"$/\1/p' "$ROOT/Cargo.toml")"
NUMBER='(0|[1-9][0-9]*)'
VERSION_PATTERN="^${NUMBER}\\.${NUMBER}\\.${NUMBER}(-(beta|rc)\\.${NUMBER})?$"
[[ "$SOURCE_VERSION" =~ $VERSION_PATTERN ]] || { echo 'Invalid workspace version.' >&2; exit 1; }
if [[ $# == 0 ]]; then printf '%s\n' "$SOURCE_VERSION"; exit 0; fi
TAG="$1"
[[ "$TAG" == v* && "${TAG#v}" =~ $VERSION_PATTERN ]] || { echo 'Expected vMAJOR.MINOR.PATCH, optionally followed by -beta.N or -rc.N.' >&2; exit 1; }
[[ "${TAG#v}" == "$SOURCE_VERSION" ]] || { echo "Tag $TAG does not match workspace version $SOURCE_VERSION. Update Cargo.toml and Cargo.lock before tagging." >&2; exit 1; }
printf '%s\n' "${TAG#v}"
