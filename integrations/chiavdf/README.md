# Native client dependency

The build consumes public Chia-Network/chiavdf at the exact commit in
[upstream.env](upstream.env), without source overlays or copied prover/control
headers. That revision includes upstream performance, live-control and telemetry
work plus [opened-handle serial validation](https://github.com/Chia-Network/chiavdf/pull/474).
The serial PR is pending; the pin refers to its immutable public branch commit.

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
