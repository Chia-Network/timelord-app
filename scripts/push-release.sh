#!/usr/bin/env bash
# Run after prepare-release.sh in a clean checkout of main.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
VERSION="${1:?Usage: push-release.sh VERSION}"
[[ $# == 1 ]]
bash scripts/release-version.sh "v$VERSION"
git add Cargo.toml Cargo.lock
git commit -m "Prepare release v$VERSION"
git tag -a "v$VERSION" -m "Timelord $VERSION"
# Reject concurrent main updates or tag creation without changing either ref.
# Branch protection failures also leave both remote refs untouched.
git push --atomic origin HEAD:refs/heads/main "refs/tags/v$VERSION"
