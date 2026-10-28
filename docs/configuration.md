# Configuration and administration

The app and `timelord-service` use the same version-1 TOML configuration. Unknown
fields and unsupported profiles are rejected; this initial schema has no migration
paths for earlier prototypes. Defaults apply to omitted supported settings. Edits
are validated and saved atomically with owner-only permissions; service changes
apply on restart. The app's startup preference needs no service restart.

```toml
schema_version = 1

[node]
host = "localhost"
port = 8444
network = "mainnet" # or testnet11
server_name = "chia.net"
certificate = "identity/client.crt"
private_key = "identity/client.key"
ca_certificate = "identity/ca.crt"

[service]
status_listen = "127.0.0.1:9257"
# Optional read-only deployment endpoints; disabled when omitted.
# observability_listen = "0.0.0.0:9258"
# Persistent supervisor listener; re-register after changing it.
supervisor_observability_listen = "127.0.0.1:9259"
# Legacy app-launch preference; durable supervisor Run/Pause is authoritative.
start_automatically = true
tuning_paused = false

[hardware]
# Populated by the app for its bundled runtime; optional for an unmanaged client.
client_binary = "/path/to/hw_vdf_client"
driver_library = "/path/to/libftd2xx.dylib"

[[hardware.boards]]
id = "asic-a"
name = "ASIC A"
serial = "BOARD_SERIAL"
device_location = 12305
client_port = 18110
profile = "automatic" # or manual
frequency_mhz = 1450 # Manual only
voltage_v = 1.0 # Manual only
temperature_limit_c = 55
```

For multiple boards, give each a distinct id and client port (18110–18112). USB
locations are discovered afresh; serials preserve settings across port changes.
`remembered_boards` retains disconnected board settings. The number of detected
boards determines capacity: one board runs three engines, two boards run two plus
one, and three boards run one each. Automatic layouts rebalance while the service
runs, after three agreeing five-second discovery polls. This briefly stops proof
work and reconnects the node with the new layout; it does not restart the app or
status endpoint. No boards leaves the service waiting for hardware. USB discovery
errors do not trigger redistribution. Idle chains with supervised native owners
remain in the layout even when open FTDI metadata and proof sockets are absent;
the native owner's sensor/exit watchdog determines when that evidence expires.
A changed layout uses its separately learned
engine-count/mask profile, or its configured Automatic starting frequency/voltage
if no matching profile exists. A high single-engine clock is never reused as a
three-engine learned setting. Thermal and invalid-proof faults stay latched.
Changed assignments start fresh chart scopes and run counters, preserving correct
board attribution; ordinary same-layout client reconnects retain the charts.

### Chain assignments

With three boards, the service ranks comparable accepted Automatic profiles at
startup: fastest to CC, next to RC, third to ICC. This is a provisional heuristic,
not a measured optimum. Ranking uses median running raw engine-1 IPS captured at
an accepted operating point, not delivered IPS from different chain workloads.
Ordinary recoveries remain permitted by the tuning policy; their observed rate is
saved with the ranking evidence. Ranking does not introduce a zero-recovery rule.

All three boards need qualifying evidence: at least 12 distinct raw samples
spanning 60 seconds, no sample gap above 15 seconds, and a profile accepted after
its configured healthy-work interval. Evidence must match the serial, physical
engine mask, native binary and Mac model, remain within configured operating
bounds, and be no more than 30 days old. Missing, provisional, manual or old profiles
preserve configuration order. Speeds within 2% retain configuration order too.
Accepted healthy profiles refresh their evidence at most every five minutes.
These observation and tie thresholds are engineering defaults.

The resolved plan stays fixed while the hardware topology is unchanged. Tuning
alone does not shuffle chain ownership. Improved ranking evidence applies on the
next service start or stable topology change. One- and two-board layouts retain
their existing assignment rules.

Administrators can override the three-board ranking by adding this table to the
config (values are **serial numbers**, not board names or USB locations):

```toml
[hardware.chain_assignments]
challenge = "CC_BOARD_SERIAL"
reward = "RC_BOARD_SERIAL"
infused_challenge = "ICC_BOARD_SERIAL"
```

Each must identify a distinct configured board; unknown or repeated serials are
rejected. If a mapped board is unplugged, its owner retries discovery while the
other boards retain their chains, including across service starts. Remove the
table to restore Automatic assignment. The app preserves this override when saving settings. The status API
reports the actual resolved chains on each board, and `plan --config FILE` previews
the saved configuration's plan without accessing USB. Live discovery can change
the available topology, so the running status remains authoritative.

Completed proof bundles log each chain's verified-ready timestamp and milliseconds
spent waiting for the other required chains. These local timings diagnose the
bottleneck; they do not claim network wins or trigger automatic swaps.

Manual settings accept
200–2200 MHz and requested 0.7–1.0 V; Automatic exploration stays within its bounds.

A fresh app installation needs no USB driver paths in its config. Discovery finds
the packaged D2XX library relative to the helper inside the app bundle. Explicit
`driver_library` or `client_binary` paths remain administrator overrides.

## Node identities

Select a local Chia root in the app, or run `local-node --chia-root FOLDER`. This
uses its public timelord leaf identity for the full-node peer port and imports the
private RPC leaf identity for read-only sync status/height. It neither edits Chia's
configuration nor imports CA signing keys. Peer connectivity does not establish
sync status. RPC is polled every 15 seconds; stale observations show no height.

For a remote node, provide a folder containing `connection.toml`, a dedicated
client certificate/key and the node CA certificate. See
[Create and import a connection bundle](connection-bundles.md) for commands,
the complete manifest, and optional RPC through SSH. The manifest has
`schema_version = 1` and the `[node]` section above. Credential paths must be
relative, remain inside the bundle, and not escape through symlinks. Private keys
must have mode 0600; CA certificates cannot act as client identities. An optional
status connection uses a separate identity:

```toml
[node.rpc]
host = "node.example.net"
port = 8555
server_name = "chia.net"
certificate = "rpc-client.crt"
private_key = "rpc-client.key"
ca_certificate = "private-ca.crt"
```

## Automatic tuning

Optional per-board `[hardware.boards.automatic]` defaults:

```toml
start_frequency_mhz = 1450
start_voltage_v = 1.0
min_frequency_mhz = 1000
max_frequency_mhz = 2200
min_voltage_v = 0.9
healthy_seconds = 120
cooldown_seconds = 900
retest_after_seconds = 7200 # 0 disables periodic retries
recovery_window_seconds = 300
recovery_loss_percent = 5.0
recovery_bad_windows = 2
minimum_proofs_per_chain = 3
```

The objective is delivered proof performance. Automatic never reduces voltage
because a board is stable. It explores supported PLL settings within the configured
frequency ceiling, holds otherwise stable trials even without a small measured
IPS gain, and permits recoveries while their delivered IPS cost stays acceptable.
There is no fixed recoveries-per-hour rejection limit for ordinary recoveries.

A separate fast guard rejects severe recovery storms: **20 restarts within a
rolling 10-second window**. It reduces frequency without waiting for a delivered
IPS score, fresh proofs, or the normal consecutive-window test. Five seconds of
settling after a clock change exclude residual logs from the old point. The guard
uses at most 20 event timestamps and works during cooldown, but respects Pause
tuning. It returns to the previous trial point or a supported lower clock,
preserves voltage, and saves the normal cooldown and reduced search ceiling.
These are provisional engineering thresholds, not characterized silicon limits.
Live clock changes continue draining native logs while awaiting acknowledgement,
so the recovery flood cannot fill stderr and block the USB control owner.

After at least `healthy_seconds` of active verified work and three qualifying
proofs per assigned chain (configurable above), a setting can be retained. Idle
time and stale telemetry do not advance that active-work timer. For ordinary recoveries,
backoff requires delivered IPS more than `recovery_loss_percent` below the
accepted baseline in `recovery_bad_windows` consecutive windows of
`recovery_window_seconds` each. Every bad window must contain recoveries and
sufficient fresh proof observations; slow recovery observations remain in the
score. One good complete window resets the consecutive count. These defaults
(two five-minute windows and a 5% drop) are provisional heuristics, not limits
established by the earlier hardware experiments. Workload changes can affect IPS
comparisons, so a correlated decline does not prove recoveries caused it.

A qualifying recovery problem first raises lower voltage toward requested 1.0 V,
or restores the previous trial frequency / backs off to a supported lower step.
Protective backoff starts the configured cooldown before upward exploration
resumes. Temperature within 1°C of the configured stop triggers frequency backoff;
upward exploration pauses within 3°C. The hard temperature/missing-telemetry stops,
failed-readback handling, and invalid-proof fault behavior remain enforced. At a
performance floor the policy holds and reports the problem rather than repeatedly
writing the same setting. Normal recoveries alone never stop proof production.

A rejected frequency is periodically reconsidered so changes in cooling or
operating conditions do not leave a board permanently capped. By default, a retry
becomes eligible after **two hours of healthy verified work**, with at least 5°C
of temperature headroom. Idle time, paused tuning, missing scores/telemetry and
performance-degraded windows do not advance this timer. Ordinary recoveries are
still permitted. Only the next supported frequency is tested, within the configured
maximum and without changing voltage or physical engine selection.

Retries use a longer acceptance period: at least `healthy_seconds` and
`recovery_window_seconds * recovery_bad_windows` of healthy work (ten minutes with
defaults). Storm, thermal and sustained-loss protections stay active. A rejected
retry returns to the previous accepted point and doubles the next healthy-work
wait: 4, 8, 16, then at most 24 hours with defaults. A successful retry clears the
learned ceiling and permits normal upward exploration again. Set
`retest_after_seconds = 0` to disable retries; otherwise it must be at least the
configured cooldown and no more than one day.

Retry waits and attempt counts are saved with the serial/engine-bound profile.
The next wait is reserved before issuing a retry command, so a command failure or
process restart cannot immediately repeat the attempt. Offline time does not
advance it. Older profiles without retry state begin a fresh wait. Pausing tuning
during a retry holds the clock; resuming restarts its full acceptance window and
retains its previous rollback point. The board detail message and service status
report the remaining healthy-work wait. These retry intervals are engineering
defaults, not a guarantee that a cooler board can sustain a higher clock.

The old `voltage_after_seconds` config key is accepted when reading existing
configs, ignored, and omitted from newly saved configs. Voltage optimization has
been removed from the policy and app settings model.

Learned settings are bound to serial, engine count, Mac model and native binary
SHA256. `automatic-profiles.json` is stored next to the config; override it with
`hardware.automatic_state`. Cache schema 2 removes voltage-search state. Existing
schema-1 observed frequencies are retained; old zero-recovery ceilings/cooldowns
are discarded and previously reduced voltage is raised to at least the configured
starting voltage. Saves remain atomic and owner-only. Unconfirmed trial settings
are not saved. Pause tuning holds the current operating point while production
continues; resume starts a fresh observation period.

## CLI and endpoints

### Saved chart history

The service saves validated receipt-speed observations, separate delivered-proof
timings for tuning, recovery/profile events and
capture intervals beside its config: `config.toml` uses `config.history.json`.
No credentials or proof contents are stored. The owner-only JSON checkpoint is
replaced atomically every five seconds and flushed after a clean shutdown. A
crash can lose data since the last successful checkpoint (normally up to five
seconds); previously saved history survives. Retention is 24 hours, bounded to
86,400 records per observation stream and events.

Restarting restores chart data with a gap between capture sessions. It does not
restore current proof counters, uptime, hardware state or current IPS. Changing
the node address/network or board-to-chain assignments starts a new history
context to avoid misattribution; frequency/profile and app updates retain history.
Unreadable or corrupt checkpoints are left untouched and persistence is disabled
with a warning in `service.log`; proof production can continue. Back up or remove
that checkpoint while the service is stopped to start a fresh history file.

Status remains `/v1/status`; chart history uses `/v3/history` with explicit capture
intervals so the app never draws a continuous line across restart downtime.

### Commands

```sh
# build/rust/debug/timelord-service or the app's Contents/Helpers/timelord-service
timelord-service init --config FILE
timelord-service validate --config FILE
timelord-service plan --config FILE
timelord-service devices --config FILE
timelord-service local-node --config FILE --chia-root FOLDER
timelord-service import-node --config FILE --bundle connection.toml
timelord-service test-node --config FILE
timelord-service node-state --config FILE
timelord-service serve --config FILE
timelord-service status --config FILE
timelord-service tuning --config FILE --paused true
```

`configuration` prints JSON; `configure --input JSON_FILE` validates before
replacement. `check-hardware --config FILE` explicitly starts the configured
clients for a bounded synthetic three-chain proof check without a full node.
Never point it at boards in use. It applies the configured operating point and
thermal guard, verifies retained sessions and a reset, then stops its own clients.
It does not train Automatic tuning or change saved settings.

The status/history HTTP API is read-only and bound to loopback. Same-user,
mode-0600 Unix IPC handles Pause/Resume tuning. `service.control_socket` optionally
overrides its derived path. Fault and thermal stops require an explicit restart;
startup never becomes an automatic restart loop.

App administrator overrides: `TIMELORD_CONFIG`, `TIMELORD_SERVICE_BINARY`, and
`TIMELORD_STATUS_URL`. Run only one owner of a board. The app can observe an external
service; it will not stop or replace one it did not launch.

Automatic tuning can explore supported PLL points up to 2200 MHz. This is a search
ceiling, not the starting clock or a guarantee of stability. Requested voltage stays
at or below 1.0 V and the temperature guard stays at or below 55°C.

## Board recovery

Native readback retries are separate from client recovery: three reads per stage,
20 ms apart, with engines paused and no repeated voltage/PLL writes. Persistent
unexpected state still stops that native client.

The service supervises each physical board independently. After a transient
client/control/socket failure it restarts only that board, with delays of 1, 5,
and 30 seconds. Initialization must reapply and verify the saved operating point;
verified proofs are required before the board appears productive again. Restart
attempts never raise its saved voltage or frequency. An unaccepted Automatic
trial returns to the last saved point. The retry budget can renew after at least
ten minutes of operation, three verified outputs for each assigned chain from
that run, and fresh proof/temperature telemetry.

Exhausted retries, failed proof verification, thermal stops and missing temperature
telemetry require attention; these are not retried indefinitely. Pause/Run starts
a new supervision session. Pausing or quitting cancels pending restarts.

Other boards keep their processes, settings and chain sessions during a board
outage. The full-node connection remains open to receive progress from other
timelords. The service never submits an incomplete combined proof or treats an
unavailable board as productive. History gaps apply to the affected board; healthy
boards' verified samples remain available.

See [deployment health and metrics](observability.md) for optional Prometheus,
liveness/readiness endpoints and deployment examples.
