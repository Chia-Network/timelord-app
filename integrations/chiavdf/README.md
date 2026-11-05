# Native client integration

The build uses the public Chia-Network/chiavdf repository at the commit in
[upstream.env](upstream.env). It downloads that checkout into `build/upstream/`
and copies source into an ignored staging directory before applying
`live-control.patch` and `raw-speed.patch`. The original checkout is not patched. No prover or USB
driver is maintained in this repository.

```sh
bash scripts/build-native-control.sh
```

The default build fetches the pinned upstream FTDI installer into the same build
cache. That installer points `src/hw/libft4222` at the download directory, whose
libraries are universal; staging copies them into a real directory. An existing
FTDI SDK can be selected explicitly with `CHIAVDF_DRIVERS`.
`CHIAVDF_SOURCE=/path/to/chiavdf` permits an explicit development checkout; it
never defaults to a parent directory. Build provenance records the revision,
patch/header hashes, native binary hash and staged source hashes. The final
`--control-version` probe returns before USB initialization.

The integration adds an inherited-pipe command channel to the existing native
client. `TLSET 1 ID MHz V\n` is applied on its USB owner thread, quiescing active
engines, ordering voltage/clock writes, checking readback and resuming those
engines. `TLACK 1 ID MHz V\n` confirms actual settings.
A rejected hardware operation returns bounded `TLERR 1 ID reason\n` before the
client stops, preserving the native failure even while the coordinator awaits
its control reply. IDs increase and frames
are bounded. Live commands accept 200–2200 MHz / requested 0.7–1.0 V. A 55°C
pre-write guard supplements the coordinator's sampled guard. Failed commands,
persistent readback mismatch or controller EOF quiesce engines and terminate the child.
Initial and final readback each allow at most three reads, 20 ms apart, with
engines paused throughout. Retries never repeat voltage/PLL writes or widen
accepted ranges/tolerances. Rejected samples log the stage, attempt, frequency,
voltage and a separate read-only snapshot of clock control/status/divider
registers (including individual USB read errors); recovery is also logged.
Persistent failures include the last diagnostic snapshot in the control error.
`--control-frequencies` reports eligible 10 MHz-grid requests directly from the
pinned PLL table, before USB initialization. Automatic tuning chooses from that
list for trials and backoff; unsupported gaps such as 1510 MHz are skipped.
Readback tolerance remains 1 MHz and configured ceilings still apply.
This mode excludes the upstream client's independent auto-frequency loop.

Temperature sampling continues when no engine has work, including idle infused
chains. A monotonic five-second deadline supplements the existing active polling
cadence. Temperature-only reads do not start engines or deserialize proof values.
The coordinator's 60-second missing-telemetry stop and configured temperature
limit remain enforced.

Native fixtures use fake registers and anonymous pipes. Rust fixtures also test
child-process acknowledgements and owner-only control IPC. Physical PLL/voltage
settling and proof continuity during live changes still require hardware testing.

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
