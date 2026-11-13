# Native client dependency

The build consumes public Chia-Network/chiavdf at the exact commit in
[upstream.env](upstream.env), without source overlays or copied prover/control
headers. That revision includes upstream performance, live-control and telemetry
work plus [opened-handle serial validation](https://github.com/Chia-Network/chiavdf/pull/474).
The pin is `906ca599901472599ebbec5fcae8f6ed0275cce5`, the upstream branch for
[PR 497](https://github.com/Chia-Network/chiavdf/pull/497), based on main
`47774682a02319c3dfd0f216981ce1eb65623515`. It includes the merged serial and
initialization changes, quantized operating-point readback validation, checked
shutdown acknowledgements and opt-in physical engine socket identities.
Requested operating points retain strict limits; measured values use the existing
bounded readback tolerance, including the 0.69375 V regulator reading for a
0.700 V shutdown target. This prevents false shutdown failures and rejected
initial readback on the next startup. Offline regression coverage does not by
itself establish hardware/testnet acceptance.

Managed owners send `TLSTOP 1 ID` and retain complete positive `TLSTOPACK` or
legacy shutdown evidence through soft deadlines and TERM fallback. Process reaping
is independently required. The supervisor allows 25 seconds for worker cleanup,
covering the native 5-second soft deadline, 10-second TERM allowance and bounded
pipe drains. Unknown evidence forces conservative reinitialization, not a
permanent stop. `--engine-identity` sends `TLENGINE 1 N` on every engine socket
connection; the adapter consumes it before the usual VDF protocol.

```sh
bash scripts/build-native.sh
```

Downloads are cached per revision under `build/upstream/`. The native source is
copied to an ignored staging directory to hold the FTDI SDK, then built unchanged.
`CHIAVDF_SOURCE` and `CHIAVDF_DRIVERS` are explicit development overrides; source
never defaults to a sibling or parent checkout. Provenance records source revision,
tracked source modifications, native binary hash and every staged source hash.

The upstream FTDI installer supplies universal macOS libraries. Staging follows
its library symlinks into regular files; `stage_ftdi_drivers` has an offline input
fixture. The app bundle verifies linkage, signs helpers and includes the libraries.
The version/control-version probes return before USB initialization.

The app selects a USB location by discovered serial, then passes
`--expected-serial`. The native client confirms the actual opened SPI/GPIO handles
before applying board settings; enumeration alone cannot protect against location
reuse during hotplug. Unreadable or mismatched identities fail closed.

Native proof, operating-point transaction, EOF, framing and raw-speed regression
tests live upstream. App tests retain its own subprocess, native-protocol parsing,
verification, telemetry freshness, recovery and tuning integration boundaries.
The app does not maintain a C++ prover or native-control fork.

The pin also includes [PR 475](https://github.com/Chia-Network/chiavdf/pull/475), which pauses all engines before the new owner configures its operating point.
`LICENSE` is chiavdf's Apache-2.0 license; `NOTICE` identifies the external runtime
libraries. FTDI binaries and GMP are obtained as build dependencies, never checked
into this repository. The app bundle includes the linked dynamic libraries with
relative loader paths and dependency notices/licenses.

`TLRAW 2 engine ips state` reports recent hardware counter deltas about once a second,
using values the USB owner loop already reads. Idle, rollback and new challenge
transitions publish `ips = -1` with an explicit `idle` or `sampling` state.
The most recent raw rate can bridge a handoff for at most six seconds; its
measurement timestamp never advances during the handoff. Idle shows no active
rate. Status exposes these
per-engine samples separately from verified delivered IPS; the dashboard refreshes
every two seconds and expires numeric samples six seconds old. No chain speeds are summed.
The displayed delivered mean and median use the same five-minute window of
30-second exporter-equivalent gauge samples. These historical statistics remain
visible between proofs and during temporary idle or node interruptions; stale
gauge samples and interruption intervals remain excluded from the calculation. The raw/delivered gap includes startup,
USB and proof work; it is not a direct measurement of proof-construction time.

### USB initialization evidence

The service's initialization mutex protects `init_hw`, not network connection
readiness. In the pinned `hw_vdf_client.cpp`, `init_hw` must return successfully
before `init_vdf_client` and `event_loop` run. `init_hw` opens FTDI and configures
PLL, voltage and PVT alarms (`hw/hw_interface.cpp`). A live-control ACK is emitted
from `event_loop` after its checked readback. Manual temperature telemetry is
emitted only by `read_hw_status`, also called by that event loop after setup.
The service may therefore release the USB initialization mutex on a matching
ACK or a finite, positive, below-limit temperature sample. The later
`Connected to timelord, waiting for challenge` line reports a separate socket
connection; it is neither necessary to finish USB setup nor proof readiness.
Fixture tests assert that both profiles release the mutex before that line,
while an owner with no initialization evidence remains bounded and is reaped.

### Recovery log line

`Recovery::parse` in the service counts a recovery only for the exact line
emitted by `event_loop` in the pinned `hw_vdf_client.cpp`:
`LOG_INFO("VDF %d: Restarting VDF at %llu iters", vdf->idx, ...)`. `vdf_do_log`
(`hw/hw_util.cpp`) prefixes it with a `%FT%T.mmm ` local timestamp and writes it
to stderr. The bundled patches do not change this line.
`crates/timelord-service/tests/fixtures/native-recovery-stderr.txt` is a trimmed excerpt
of captured hardware client stderr (2026-10-19 standalone run, plus 2026-10-21
lines unwrapped from the service log) with nine recoveries among interleaved
noise; the parser test expects exactly those. Recheck the fixture whenever
`upstream.env` moves.
