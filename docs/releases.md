# macOS builds and releases

The app targets Apple Silicon. Install by opening the DMG and dragging Timelord
into Applications. A full node is configured separately; no Chia identity,
connection bundle, user configuration, chart history or signing key is shipped.

## Developer builds

Pull requests, `main`, and manual CI runs produce the `Timelord-macos-arm64`
Actions artifact. It contains an ad-hoc-signed app ZIP, DMG, checksums and pinned
native build provenance. The DMG is mounted read-only and its helpers are probed
with `--version` / `--control-version`; neither command opens USB or a node socket.
These artifacts expire after 14 days and are not notarized distribution builds.
The bundle job uses macOS 26 so Xcode supports the layered icon when it lands.

The existing **macOS checks** job runs the source tests on macOS 15. The bundle
job and signing job use isolated Apple Silicon runners and never use live ASICs.
Hardware acceptance and testing on the oldest supported macOS remain separate.

## One-time repository setup

Grant this repository access to the following organization secrets, using the same names as
[chia-blockchain's macOS workflow](https://github.com/Chia-Network/chia-blockchain/blob/main/.github/workflows/build-macos-installers.yml):

| Secret | Value |
| --- | --- |
| `APPLE_DEV_ID_APP` | Base64-encoded PKCS#12 containing a Developer ID Application certificate and private key |
| `APPLE_DEV_ID_APP_PASS` | PKCS#12 password |
| `APPLE_TEAM_ID` | Apple Developer team ID |
| `APPLE_NOTARIZE_USERNAME` | Apple account used for notarization |
| `APPLE_NOTARIZE_PASSWORD` | Its app-specific password |

No GitHub Actions environment or environment approval is required. Repository
secrets can also supply these values. Do not commit certificates, private keys
or account passwords. The pinned Apple signing
action imports the certificate into a temporary keychain and removes it in its
post-job cleanup. Exactly one imported Developer ID Application identity must
match the team. In the Release workflow, only the draft-release job receives
`contents: write`; build and signing jobs have read-only repository permissions.

Release uses `GITHUB_TOKEN` and needs no personal access token.
**Prepare Release** requests `contents: write` and `actions: write` to commit
both Cargo version files directly to `main`, create the version tag, and dispatch
Release. Repository and organization rules must permit the workflow token to
push to `main` and create tags. Preparation respects those rules and does not
bypass branch protection. It does not need pull-request permissions.

This repository currently requires reviewed PRs on `main` and permits the
`ChiaAutomation` account to bypass that requirement. For direct preparation,
provide a repository or organization secret named `RELEASE_PREPARE_TOKEN` with
that account's credential and repository Contents read/write access. Organization
policy must permit use of that credential. Preparation uses it for Git access;
its tag push starts Release normally, so the workflow skips explicit dispatch to
avoid duplicate builds. The credential does not need access to Apple secrets.
Without this secret, preparation falls back to `GITHUB_TOKEN`; that works only
where branch rules already permit direct pushes by that token. With the current
rules and no automation credential, the push is rejected before any remote
commit or tag changes. No repository protection is relaxed by these workflows.
Missing credentials fail the release before signing. Apple team registration and
the certificate must permit Developer ID distribution and notarization.

## Cut a release

1. Merge the intended runtime and build changes into `main`.
2. In Actions → **Prepare Release** → **Run workflow**, select `main` and enter
   the new version without `v`, such as `0.1.1`, `0.2.0-beta.1`, or `0.2.0-rc.1`.
   Alternatively: `gh workflow run prepare-release.yml --ref main -f version=0.1.1`.
   Preparation updates `Cargo.toml`, runs `cargo check --workspace` to refresh
   `Cargo.lock`, commits both files, and atomically pushes the version commit to
   `main` together with its matching tag. The automation token's tag push starts
   **Release**. When using the default `GITHUB_TOKEN`, preparation explicitly
   dispatches Release instead, because that token's pushes do not trigger other
   workflows.
3. Follow **Actions → Release**. That workflow runs CI, builds, signs, notarizes,
   and creates the draft release. A successful preparation run means Release was
   started; follow the separate Release run for build and signing results.
4. Review and publish the resulting draft in GitHub Releases. Beta/RC tags create
   prerelease drafts. Publishing remains manual.

There is no version-bump PR to merge and no separate tag or release to create in
the GitHub UI. Preparation itself uses no Apple credentials; Release uses them
after CI passes.

Preparation rejects malformed/current versions and existing tags. It never moves
a tag, force-pushes, or modifies an existing published release. Older preparation
branches and PRs do not block it and are not merged automatically. Existing
mismatched tags, including tags created before their version bump landed, still
require a new version or explicit maintainer-directed recovery.

If `main` advances during preparation, a tag is created concurrently, or branch
protection rejects the commit, the atomic push changes neither remote ref. Rerun
preparation from the updated `main` after resolving the cause. If the push succeeds
but dispatch fails, the workflow summary contains the commit and tag; run
`gh workflow run release.yml --ref vVERSION` to retry Release against that existing
tag. For a later build or notarization failure, rerun Release against the same
tag. Do not rerun preparation for an existing tag.

To prepare locally instead, run `bash scripts/prepare-release.sh VERSION`, review
the two Cargo files, then run `bash scripts/push-release.sh VERSION` from a clean
checkout of `main` with Git author identity configured. This commits and pushes
`main` and the tag. A tag pushed with your own credentials triggers Release;
explicitly dispatch Release if using the workflow token. An already-configured
source version may also be tagged directly for its first release.

The release workflow reuses CI's app from the **same workflow run**, verifies its
checksum, stamps the release version/source commit and signs FTDI/GMP, the native
client, coordinator and UI from the inside out with the same Developer ID. It
enables hardened runtime and secure timestamps without disabling library
validation. The numeric base version goes into `CFBundleShortVersionString` and
`CFBundleVersion`; the exact version (including beta/RC suffix) and source commit
are recorded in `TimelordReleaseVersion` and `TimelordSourceRevision`.

It creates a signed DMG containing Timelord.app and an Applications link, submits
the DMG with `notarytool`, requires Apple's `Accepted` status, staples and validates
the ticket, and assesses the final DMG with Gatekeeper. This follows the container
notarization approach used in Chia's installer workflow; it uses no Electron or
Python runtime in the shipped app.

The draft contains:

- `Timelord-VERSION-macos-arm64.dmg` and `SHA256SUMS` for the final stapled DMG.
- `release-manifest.json` with app source commit, version, architecture and run URL.
- `native-provenance.txt` with pinned chiavdf revision and native input hashes.
- `notarization.json` with Apple's accepted submission receipt.

A retry can replace assets on an existing draft. It refuses to modify a published
release. Move no existing release tags; use a new version for different source.
Notarization failures retain their submission JSON in a diagnostic Actions
artifact, and do not create a draft release. Cargo audit remains a separate
Security check. Its reviewed upstream RSA advisory exception is documented in
`.cargo/audit.toml`; signing does not remove that dependency.

## Local packaging check

Build the pinned native client and developer bundle as documented in README,
then choose a fresh output path:

```sh
bash scripts/package-macos.sh build/Timelord.app build/Timelord-test.dmg \
  "$(bash scripts/release-version.sh)"
```

This verifies signatures, ARM64 slices, the Applications link, DMG integrity, and
helper loading from the mounted image with a system-only PATH. It does not sign
with Developer ID or call Apple's notarization service. A successful local check
is not evidence that Apple has accepted a release.
