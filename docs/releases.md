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

The workflows use `GITHUB_TOKEN`; no GitHub personal access token is required.
For **Prepare Release**, enable **Allow GitHub Actions to create and approve pull
requests** in Settings → Actions → General → Workflow permissions (organization
policy must also permit this). Preparation requests `contents: write`,
`pull-requests: write`, and `actions: write` to push the version branch, open its
PR, and explicitly dispatch CI. It does not approve or merge PRs.
Missing credentials fail the release before signing. Apple team registration and
the certificate must permit Developer ID distribution and notarization.

## Cut a release

1. Merge the intended runtime and build changes into `main`.
2. In Actions → **Prepare Release** → **Run workflow**, select `main` and enter
   the new version without `v`, such as `0.1.1`, `0.2.0-beta.1`, or `0.2.0-rc.1`.
   Alternatively: `gh workflow run prepare-release.yml --ref main -f version=0.1.1`.
   The workflow updates `[workspace.package].version` in `Cargo.toml`, runs
   `cargo check --workspace` to refresh `Cargo.lock`, and opens a PR from
   `release/vVERSION` to `main`. It explicitly starts CI on that branch using
   `workflow_dispatch`, so checks do not depend on bot-created PR events.
   Review the two-file diff, wait for CI, and merge the PR. Preparation creates
   no tag or release and uses no Apple credentials.
3. Check out the merged version commit and push a tag matching that exact source
   version, such as `v0.1.1`. A mismatched
   version, branch ref, unsupported suffix or malformed tag is rejected before
   signing. To rerun manually, dispatch **Release** against that existing tag:
   `gh workflow run release.yml --ref v0.1.1`.
4. After CI succeeds, signing and notarization run automatically and create a draft.
5. Review and publish the resulting draft in GitHub Releases. Beta/RC tags create
   prerelease drafts. The workflow never publishes a release automatically.

Preparation refuses the current version, malformed versions, existing tags, and
existing preparation branches; it never force-pushes. For an existing preparation
PR, continue with that PR. If a run fails after pushing its branch but before
opening the PR, open a PR from that branch and run CI against it manually. For
failures before pushing, rerun preparation. To prepare locally instead, run
`bash scripts/prepare-release.sh VERSION` and commit `Cargo.toml` and `Cargo.lock`.
The already-configured source version can also be tagged directly for its first
release without a preparation PR.

For a stable release, after the version commit is merged and checked out (verify
`HEAD` is the intended release commit before tagging):

```sh
git tag -a v0.1.1 -m 'Timelord 0.1.1'
git push origin v0.1.1
```

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
