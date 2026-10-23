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
boards determines chain assignments; there is no separate layout override.
A topology change requires restarting the service. Manual settings accept
200–2200 MHz and requested 0.7–1.0 V; Automatic exploration stays within its bounds.

## Node identities

Select a local Chia root in the app, or run `local-node --chia-root FOLDER`. This
uses its public timelord leaf identity for the full-node peer port and imports the
private RPC leaf identity for read-only sync status/height. It neither edits Chia's
configuration nor imports CA signing keys. Peer connectivity does not establish
sync status. RPC is polled every 15 seconds; stale observations show no height.

For a remote node, provide a folder containing `connection.toml`, a dedicated
client certificate/key and the node CA certificate. The manifest has
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
recovery_window_seconds = 300
recovery_loss_percent = 5.0
recovery_bad_windows = 2
minimum_proofs_per_chain = 3
```

The objective is delivered proof performance. Automatic never reduces voltage
because a board is stable. It explores supported PLL settings within the configured
frequency ceiling, holds otherwise stable trials even without a small measured
IPS gain, and permits recoveries while their delivered IPS cost stays acceptable.
There is no fixed recoveries-per-hour rejection limit.

After at least `healthy_seconds` of active verified work and three qualifying
proofs per assigned chain (configurable above), a setting can be retained. Idle
time and stale telemetry do not advance that active-work timer. Recovery-related
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
