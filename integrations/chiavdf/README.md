# Native client integration

The build uses the public Chia-Network/chiavdf repository at the commit in
[upstream.env](upstream.env). It downloads that checkout into `build/upstream/`
and copies source into an ignored staging directory before applying
`live-control.patch`. The original checkout is not patched. No prover or USB
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
engines. `TLACK 1 ID MHz V\n` confirms actual settings. IDs increase and frames
are bounded. Live commands accept 200–1600 MHz / requested 0.7–1.0 V. A 55°C
pre-write guard supplements the coordinator's sampled guard. Failed commands,
readback mismatch or controller EOF quiesce engines and terminate the child.
This mode excludes the upstream client's independent auto-frequency loop.

Native fixtures use fake registers and anonymous pipes. Rust fixtures also test
child-process acknowledgements and owner-only control IPC. Physical PLL/voltage
settling and proof continuity during live changes still require hardware testing.

`LICENSE` is chiavdf's Apache-2.0 license; `NOTICE` identifies the external runtime
libraries. FTDI binaries and GMP are obtained as build dependencies, never checked
into this repository. The app bundle includes the linked dynamic libraries with
relative loader paths and dependency notices/licenses.
