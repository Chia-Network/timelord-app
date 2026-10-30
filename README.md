# Timelord

A native macOS app for running Chia VDF ASICs against an existing local or remote
full node. SwiftUI provides the dashboard and menu bar, a Rust service coordinates
the three VDF chains, and the existing C++ chiavdf client handles USB and proofs.
The application has no Python runtime and does not install or manage a full node.

This is the standalone **timelord-app** repository. It can be cloned anywhere;
no parent, sibling, or private chiavdf checkout is required.

## Use

Connect one to three ASIC boards and select your node's Chia folder or import an
administrator-provided connection bundle. Install the app in `/Applications`, then
choose **Enable background service…** in Settings and approve the macOS prompt.
Timelord then starts at boot without opening the interface. One board runs three engines; two boards run two plus one; three boards
run one engine each. A board running a single engine uses physical engine 1
(zero-based); two-engine boards use engines 0 and 1, and three-engine boards use
all three. Only detected boards appear.

New boards use **Automatic** tuning, starting at 1450 MHz / requested 1.0 V or a
learned setting for that board, engine count, Mac model and native client. Manual
frequency and voltage settings remain available. Requested voltage is limited to
1.0 V and the temperature stop to at most 55°C. **Pause tuning** holds the observed
setting while proofs continue; **Pause** stops proof production. Automatic permits
recoveries while delivered performance remains acceptable, holds stable voltage,
and backs off for sustained recovery-related IPS loss or thermal pressure. The
recovery evaluation thresholds are configurable in the [configuration guide](docs/configuration.md).

Done saves settings. Changes to a supervised service use **Save & Restart**.
**Pause** is durable: neither reopening the interface nor rebooting resumes it.
Press **Run** to resume. Quitting the interface, closing its window, screen locking
and logging out do not request Pause or stop the background supervisor. The old
`start_automatically` app-launch preference has no effect on supervised intent.
An older manually started coordinator can be observed but must be paused before
installing background supervision; the installer refuses competing ownership.

The supervisor runs under the installing user's non-root account, owns durable
Run/Pause intent and replaces crashed workers with capped retries. It has no USB,
node or proof-arithmetic dependencies. See [deployment](docs/deployment.md) for
installation, SSH control, migration and remaining physical qualification gates.

USB disconnects and unreadable sensor data stop only the affected client. The
service retries after 1, 5, then at most every 30 seconds without exhausting a
retry budget, and finds a returning board by serial even if its USB location
changes. Other boards keep running. Confirmed overtemperature and invalid proofs
remain stopped for attention; missing telemetry never permits bypassing the
native control checks. Automatic layouts rebalance after a stable USB change:
one board runs three engines, two run two plus one, and three run one each.
This briefly restarts the proof session and native clients, while the status API
stays available. Each layout uses its own learned tuning profile. Explicit
serial-pinned chain assignments retain their mapping instead.

The persistent supervisor prevents idle system sleep while Run is requested,
including worker recovery, and releases the assertion after Pause cleanup or
supervisor shutdown. Display sleep and screen locking remain permitted. Explicit
system sleep is not prevented.

The dashboard shows node sync/height, per-chain proof history, temperature and
Automatic tuning status. The headline IPS is the last-hour arithmetic mean of
30-second samples of the latest verified output from any chain, matching the
`chia_timelord_estimated_ips` gauge semantics. Each output uses cumulative iterations
divided by elapsed time from client assignment to proof receipt (before verification
and node submission), truncated to one decimal place. Only validated proofs update
the gauge; internal one-iteration primers do not. Board tiles show five-minute means for their individual chains, alongside one-hour
charts using the same samples. Independent chain speeds are never added together.

Slow recovery samples remain included. Values at least 90 seconds old, known work
interruptions and missing capture are omitted rather than held as current speed or
filled with zeros. Grafana may hold stale gauges longer; compare the same windows,
scrape alignment and availability for exact comparisons. A successful send does not
establish that the node used the proof. Existing delivery-timing history is retained
on disk but is not mixed into the new IPS charts. Automatic tuning continues using
its separate delivered-proof timings, including verification and submission delays.
The native clients retain recent ASIC values for off-grid proof endpoints
(`--retain-asic-values`), matching the optimized benchmark configuration.

## Build

Developer prerequisites: macOS 14 or later, Xcode command-line tools, Swift 5.10 or
later, Rust/Cargo (minimum version in Cargo.toml), CMake, GMP and Boost headers.
Xcode 26 or newer compiles the layered icon; older selected tools use the committed
flat icon. Errors from a supported icon compiler still fail the bundle.
For Homebrew dependencies:

```sh
brew install cmake gmp boost
bash scripts/check.sh
bash scripts/build-native-control.sh
bash scripts/bundle-dev.sh
open build/Timelord.app
```

The native build downloads a pinned public chiavdf revision and its FTDI SDK into
ignored `build/` directories, stages the small live-control integration, and builds
the client without touching hardware. Dependency pins, notices and integration
details are under [integrations/](integrations/chiavdf/README.md). FTDI libraries
retain their vendor distribution terms.

The developer bundle includes the Rust helper, native client, FTDI and dynamically
linked GMP libraries with relative load paths and third-party notices. It is
ad-hoc signed, not a notarized release. The bundle script refuses to overwrite an
existing app; use a fresh output path for subsequent builds:

```sh
TIMELORD_APP_BUNDLE="$PWD/build/review/Timelord.app" bash scripts/bundle-dev.sh
```

`CHIAVDF_SOURCE` selects an explicit external development checkout;
`CHIAVDF_DRIVERS` selects an existing macOS FTDI SDK; `CMAKE_PREFIX_PATH` locates
non-Homebrew build dependencies. No source checkout is inferred from this repo's
location. `TIMELORD_ASIC_CLIENT` can select a separately built live-control client
and its adjacent FTDI libraries.

## Administration

Configuration is human-editable TOML at
`~/Library/Application Support/Timelord/config.toml`. Relative paths resolve from
the configuration directory. See [configuration](docs/configuration.md) for node
identities, remote bundles, board settings, tuning and CLI commands.
For a node on another computer, follow
[Create and import a connection bundle](docs/connection-bundles.md).

Run `bash scripts/check.sh` for formatting, clippy, Rust/Swift tests and the native
control fixtures. Tests use generated identities and disposable local peers and
processes. They never use a live node or USB board. Dashboard sample data exists
only in the test target; it is not part of the app.

## Continuous integration

Pull requests and pushes to `main` run macOS formatting, lint, native fixtures,
Rust/Swift tests, and an independent Apple Silicon developer app build. The build
uploads a zipped, ad-hoc-signed app, a drag-to-Applications DMG, checksums and
native source provenance for 14 days. The DMG is mounted and its bundled helpers
are smoke-tested without a node or USB board. It is not a notarized release. CI never connects to a live node or ASIC.

Security checks run CodeQL and a Cargo vulnerability audit on pull requests,
`main`, and weekly. Dependabot proposes weekly Cargo and GitHub Actions updates.
The audit has one reviewed exception for the upstream `rsa` dependency advisory
[RUSTSEC-2023-0071](https://rustsec.org/advisories/RUSTSEC-2023-0071/): the app does
not use its affected operations. The rationale is recorded in `.cargo/audit.toml`.
Require **macOS checks** and **Apple Silicon app bundle** in the repository's
branch rules when CI is verified. CodeQL needs `actions: read` to inspect workflow
runs in this private repository. Hardware benchmarks remain separate.

## Releases

Version tags run the same CI jobs, then sign and notarize an Apple Silicon DMG
and upload it to a **draft** GitHub release. No release credentials are used in
pull requests. Releases fail if credentials or notarization are missing; they
never fall back to developer signing. See [the release guide](docs/releases.md)
for Apple credentials, version tags, download artifacts, and publishing.

## Development status

Initial hardware/node smoke tests have verified real proofs across all three
chains. Sustained consensus acceptance, physical clock/voltage transitions,
USB unplug/replug, clean-machine installation and notarized distribution still
need validation. This repository is development source, not a production release.

The pinned Rust scheduler needs a narrow decoder for current Chia unfinished-block
messages. Absent MMR roots are supported; active header MMR roots fail closed.
Live configuration reload and start-at-login service registration are not implemented. Chart history is checkpointed to disk for up to
24 hours and restored after service restarts, with downtime shown as gaps. Current
uptime, proof counts and health still start fresh for each service run.

See [architecture](docs/architecture.md) for process ownership, proof verification
and tuning behavior. Licensed under [Apache-2.0](LICENSE); dependencies retain their
own licenses and notices.
