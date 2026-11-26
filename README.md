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

A board's trouble stays on that board. Unplugging it, a stuck USB write or a
failed proof session stops only its own native client; the other boards and the
full-node session keep running, and a returning board is found by serial even
if its USB location changes. Only overheating and invalid proofs are saved as
safety restrictions (cooldown, low-power probe or private verified proof, then a
recovery clock limit that relaxes after hours of fault-free running). An
unconfirmed stop only means a cautious start, after which the board returns to
its saved setting once it has produced verified work. Retries never exhaust.

Availability comes first: a board that is working always carries work. Only
hardware evidence (overheating, an invalid proof, a stuck or dead native client)
or an operator's **Keep out of rotation** takes a board out. Pulling a cable is
not a failure: after a native client dies the service checks USB, and a board
that is no longer attached is simply looked for again. USB can still list a
pulled board for a moment, so a stop counted as a failure is taken back when
the board turns out to be missing within two minutes. Each board's reliability
is saved per serial and survives restarts and network switches; it only decides
which chain a board carries. After a real failure the board is on probation
(1 min to 2 h, growing with repeated failures) and prefers the infused-challenge
chain while more reliable boards carry the challenge and reward chains. The
dashboard shows each board's state, failure streak and recent history, with
**Retry now** (clears the streak), **Keep out of rotation** and **Enable**.

Automatic layouts route chains among available boards: one board runs three
engines, two run two plus one, and three run one each. Each native owner keeps
three identified engine sockets available; unused engines remain idle. The
challenge and reward chains go to the most reliable boards. A chain stays on
its board while that board is healthy; chains move back to a returning board
only at the next peak. Route changes cancel only affected chain sessions, never
replace healthy native owners or the node connection, and briefly hold
Automatic tuning on every board so a shared-bus disturbance is not mistaken for
instability. Explicit serial-pinned chain assignments retain their mapping instead.

Switching the full-node profile applies without restarting ASIC boards.

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
bash scripts/build-native.sh
bash scripts/bundle-dev.sh
open build/Timelord.app
```

The native build downloads a pinned public chiavdf revision and its FTDI SDK into
ignored `build/` directories and builds the unchanged native source without
touching hardware. Dependency pins, notices and integration
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

The November 15 HF2 development build supports current Chia PoS2 proofs and active
header/sub-epoch MMR roots. Its m6mini testnet deployment produced infusion proofs
that the full node accepted into PoS2 blocks. See
[integration evidence](integrations/dg-xch/hf2-testnet-deployment.json).
dg_xch comes from one pinned revision of the
[Chia-Network dg_xch_utils fork](integrations/dg-xch/VENDORING.md), which carries
our HF2 timelord patches as signed commits.
Normal Cargo checks and builds require no fork or local overrides. Offline
fixtures and this post-fork trial do not establish activation replay, sustained
hardware stability or production compatibility.
Switching the full-node profile applies live; other configuration edits still
need a supervised restart. Start-at-login service registration is not implemented. Chart history is checkpointed to disk for up to
24 hours and restored after service restarts, with downtime shown as gaps. Current
uptime and proof counts start fresh for each service run, and proof counts also
for each node-profile switch. Board reliability (failure streaks, probation and
safety restrictions) is saved per serial and survives restarts. The 1.0.3 to 1.0.4
upgrade clears legacy streaks whose last failure is the Automatic prepare-state
message; see [deployment](docs/deployment.md#upgrade-and-rollback-procedure).

See [architecture](docs/architecture.md) for process ownership, proof verification
and tuning behavior. Licensed under [Apache-2.0](LICENSE); dependencies retain their
own licenses and notices.

Deployment monitoring: [health, readiness and Prometheus metrics](docs/observability.md).
