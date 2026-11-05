#!/usr/bin/env bash
# Exercise release preparation in a standalone, offline Cargo workspace.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
FIXTURE="$(mktemp -d)"
TEST_GPG_HOME="$FIXTURE/gnupg"
trap 'if [[ -d "$TEST_GPG_HOME" ]]; then GNUPGHOME="$TEST_GPG_HOME" gpgconf --kill gpg-agent; fi; rm -rf "$FIXTURE"' EXIT
mkdir -p "$FIXTURE/scripts" "$FIXTURE/crates/example/src" "$FIXTURE/crates/shared/src"
cp "$ROOT/scripts/prepare-release.sh" "$ROOT/scripts/release-version.sh" \
  "$ROOT/scripts/push-release.sh" "$FIXTURE/scripts/"
cat > "$FIXTURE/Cargo.toml" <<'EOF'
[workspace]
members = ["crates/example", "crates/shared"]
resolver = "3"
[workspace.package]
version = "0.1.0"
EOF
cat > "$FIXTURE/crates/example/Cargo.toml" <<'EOF'
[package]
name = "release-fixture"
version.workspace = true
edition = "2024"
[dependencies]
release-shared = { path = "../shared" }
EOF
cat > "$FIXTURE/crates/shared/Cargo.toml" <<'EOF'
[package]
name = "release-shared"
version.workspace = true
edition = "2024"
EOF
echo 'pub fn example() {}' > "$FIXTURE/crates/example/src/lib.rs"
echo 'pub fn shared() {}' > "$FIXTURE/crates/shared/src/lib.rs"
export CARGO_NET_OFFLINE=true
cargo check --manifest-path "$FIXTURE/Cargo.toml"
cp "$FIXTURE/Cargo.toml" "$FIXTURE/original.toml"
cp "$FIXTURE/Cargo.lock" "$FIXTURE/original.lock"
for version in 0.1.0 v0.2.0 01.2.3 0.2.0-alpha.1 0.2.0+local '0.2.0;exit 0' $'0.2.0\n' ''; do
  if bash "$FIXTURE/scripts/prepare-release.sh" "$version" > "$FIXTURE/log" 2>&1; then
    echo "Unexpectedly accepted version: $version" >&2; exit 1
  fi
  cmp "$FIXTURE/original.toml" "$FIXTURE/Cargo.toml"
  cmp "$FIXTURE/original.lock" "$FIXTURE/Cargo.lock"
done
for version in 0.2.0-beta.1 0.2.0-rc.1 0.2.0; do
  bash "$FIXTURE/scripts/prepare-release.sh" "$version"
  [[ "$(bash "$FIXTURE/scripts/release-version.sh" "v$version")" == "$version" ]]
  sed "s/0\.1\.0/$version/g" "$FIXTURE/original.lock" > "$FIXTURE/expected.lock"
  cmp "$FIXTURE/expected.lock" "$FIXTURE/Cargo.lock"
  cargo check --manifest-path "$FIXTURE/Cargo.toml" --locked
done
# Exercise remote updates against a local bare repository, never GitHub.
export GIT_AUTHOR_NAME='Release fixture' GIT_COMMITTER_NAME='Release fixture'
export GIT_AUTHOR_EMAIL='fixture@example.invalid' GIT_COMMITTER_EMAIL='fixture@example.invalid'
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
export GNUPGHOME="$TEST_GPG_HOME"
mkdir -m 700 "$GNUPGHOME"
git init -b main "$FIXTURE"
git -C "$FIXTURE" add Cargo.toml Cargo.lock crates scripts
git -C "$FIXTURE" commit -m 'Initial fixture'
git init --bare --initial-branch=main "$FIXTURE/remote.git"
git -C "$FIXTURE" remote add origin "$FIXTURE/remote.git"
git -C "$FIXTURE" push origin main
bash "$FIXTURE/scripts/prepare-release.sh" 0.3.0
REMOTE_HEAD="$(git --git-dir="$FIXTURE/remote.git" rev-parse refs/heads/main)"
if bash "$FIXTURE/scripts/push-release.sh" 0.3.0 > "$FIXTURE/log" 2>&1; then
  echo 'Unexpectedly created a release without a signing key.' >&2; exit 1
fi
[[ "$(git --git-dir="$FIXTURE/remote.git" rev-parse refs/heads/main)" == "$REMOTE_HEAD" ]]
if git --git-dir="$FIXTURE/remote.git" show-ref --verify --quiet refs/tags/v0.3.0; then
  echo 'Unsigned release attempt left a tag behind.' >&2; exit 1
fi
gpg --batch --pinentry-mode loopback --passphrase '' \
  --quick-generate-key 'Release fixture <fixture@example.invalid>' ed25519 sign 0
bash "$FIXTURE/scripts/push-release.sh" 0.3.0
REMOTE_HEAD="$(git --git-dir="$FIXTURE/remote.git" rev-parse refs/heads/main)"
[[ "$(git --git-dir="$FIXTURE/remote.git" rev-parse 'refs/tags/v0.3.0^{commit}')" == "$REMOTE_HEAD" ]]
git --git-dir="$FIXTURE/remote.git" verify-commit "$REMOTE_HEAD"
git --git-dir="$FIXTURE/remote.git" verify-tag v0.3.0
git --git-dir="$FIXTURE/remote.git" show main:Cargo.lock > "$FIXTURE/pushed.lock"
cmp "$FIXTURE/Cargo.lock" "$FIXTURE/pushed.lock"

# A concurrent main commit must reject the version commit AND its new tag.
git clone "$FIXTURE/remote.git" "$FIXTURE/stale"
git -C "$FIXTURE" commit --allow-empty -m 'Concurrent main change'
git -C "$FIXTURE" push origin main
REMOTE_HEAD="$(git --git-dir="$FIXTURE/remote.git" rev-parse refs/heads/main)"
bash "$FIXTURE/stale/scripts/prepare-release.sh" 0.4.0
if bash "$FIXTURE/stale/scripts/push-release.sh" 0.4.0 > "$FIXTURE/log" 2>&1; then
  echo 'Unexpectedly overwrote concurrent main update.' >&2; exit 1
fi
[[ "$(git --git-dir="$FIXTURE/remote.git" rev-parse refs/heads/main)" == "$REMOTE_HEAD" ]]
if git --git-dir="$FIXTURE/remote.git" show-ref --verify --quiet refs/tags/v0.4.0; then
  echo 'Rejected push left a release tag behind.' >&2; exit 1
fi

# A tag created after validation must reject BOTH updates, preserving that tag.
git clone "$FIXTURE/remote.git" "$FIXTURE/collision"
git -C "$FIXTURE" tag -a v0.5.0 -m 'Concurrent tag'
git -C "$FIXTURE" push origin refs/tags/v0.5.0
REMOTE_TAG="$(git --git-dir="$FIXTURE/remote.git" rev-parse refs/tags/v0.5.0)"
bash "$FIXTURE/collision/scripts/prepare-release.sh" 0.5.0
if bash "$FIXTURE/collision/scripts/push-release.sh" 0.5.0 > "$FIXTURE/log" 2>&1; then
  echo 'Unexpectedly overwrote an existing tag.' >&2; exit 1
fi
[[ "$(git --git-dir="$FIXTURE/remote.git" rev-parse refs/heads/main)" == "$REMOTE_HEAD" ]]
[[ "$(git --git-dir="$FIXTURE/remote.git" rev-parse refs/tags/v0.5.0)" == "$REMOTE_TAG" ]]

# A branch policy rejection must not leave a release tag behind.
cat > "$FIXTURE/remote.git/hooks/update" <<'EOF'
#!/bin/sh
[ "$1" != refs/heads/main ]
EOF
chmod +x "$FIXTURE/remote.git/hooks/update"
git clone "$FIXTURE/remote.git" "$FIXTURE/protected"
bash "$FIXTURE/protected/scripts/prepare-release.sh" 0.6.0
if bash "$FIXTURE/protected/scripts/push-release.sh" 0.6.0 > "$FIXTURE/log" 2>&1; then
  echo 'Unexpectedly bypassed branch protection.' >&2; exit 1
fi
[[ "$(git --git-dir="$FIXTURE/remote.git" rev-parse refs/heads/main)" == "$REMOTE_HEAD" ]]
if git --git-dir="$FIXTURE/remote.git" show-ref --verify --quiet refs/tags/v0.6.0; then
  echo 'Rejected push left a release tag behind.' >&2; exit 1
fi
echo 'Release preparation checks passed.'
