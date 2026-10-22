# Timelord

A native macOS app for running Chia VDF ASICs against an existing local or remote
full node. SwiftUI provides the dashboard and menu bar, a Rust service coordinates
the three VDF chains, and the existing C++ chiavdf client handles USB and proofs.
The application has no Python runtime and does not install or manage a full node.

This is the standalone **timelord-app** repository. It can be cloned anywhere;
no parent, sibling, or private chiavdf checkout is required.

## Use

Connect one to three ASIC boards and select your node's Chia folder or import an
administrator-provided connection bundle. Timelord starts automatically when setup
is ready. One board runs three engines; two boards run two plus one; three boards
run one engine each. Only detected boards appear.

New boards use **Automatic** tuning, starting at 1450 MHz / requested 1.0 V or a
learned setting for that board, engine count, Mac model and native client. Manual
frequency and voltage settings remain available. Requested voltage is limited to
1.0 V and the temperature stop to at most 55°C. **Pause tuning** holds the observed
setting while proofs continue; **Pause** stops proof production.

Done saves settings. Changes to a running service use **Save & Restart**. Settings
includes **Run Timelord when the app opens**, enabled by default. Pause holds until
Run is selected or the app is reopened. Closing the window keeps the menu bar and
service running; Stop and Quit stops the app's owned processes. An externally
started service is observed without taking ownership.

While its owned service runs, the app prevents idle system sleep and App Nap;
the display can still sleep and the screen can lock. Pause/exit releases the
activity after the service exits. Explicit system sleep is not prevented.

The dashboard shows node sync/height, per-chain proof history, temperature and
Automatic tuning status. IPS measures verified proofs sent to the node, including
slow recovery observations. Independent chain speeds are never added together.
A successful send does not establish that the node used the proof.

## Build

Developer prerequisites: macOS 14 or later, Xcode command-line tools, Swift 5.10 or
later, Rust/Cargo (minimum version in Cargo.toml), CMake, GMP and Boost headers.
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

Run `bash scripts/check.sh` for formatting, clippy, Rust/Swift tests and the native
control fixtures. Tests use generated identities and disposable local peers and
processes. They never use a live node or USB board. Dashboard sample data exists
only in the test target; it is not part of the app.

## Development status

Initial hardware/node smoke tests have verified real proofs across all three
chains. Sustained consensus acceptance, physical clock/voltage transitions,
USB unplug/replug, clean-machine installation and notarized distribution still
need validation. This repository is development source, not a production release.

The pinned Rust scheduler needs a narrow decoder for current Chia unfinished-block
messages. Absent MMR roots are supported; active header MMR roots fail closed.
Automatic hotplug failover, live configuration reload, durable history and
start-at-login service registration are not implemented. In-memory history covers
up to 24 hours and is cleared on service restart.

See [architecture](docs/architecture.md) for process ownership, proof verification
and tuning behavior. Licensed under [Apache-2.0](LICENSE); dependencies retain their
own licenses and notices.
