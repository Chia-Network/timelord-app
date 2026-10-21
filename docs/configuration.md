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
max_frequency_mhz = 1600
min_voltage_v = 0.9
healthy_seconds = 120
cooldown_seconds = 900
voltage_after_seconds = 3600
minimum_proofs_per_chain = 3
```

The policy uses active verified work and fresh temperature readings, rather than
elapsed idle time. It tests 10 MHz increases, retaining measured improvements;
recoveries restore the previous observed trial setting or reduce the baseline by
20 MHz. At lower voltage, a recovery can first restore requested 1.0 V. Frequency
exploration respects thermal headroom and cooldown. Once frequency settles, it can
test 6.25 mV reductions after an hour of active work. These are live heuristics,
not a benchmark or a guarantee of long-term stability.

Learned settings are bound to serial, engine count, Mac model and native binary
SHA256. `automatic-profiles.json` is stored next to the config; override it with
`hardware.automatic_state`. Confirmed rollback settings are saved with zero clean
seconds; experimental increases are not saved. Pause tuning rolls back an ongoing
trial, then holds the previous observed setting while production continues.

## CLI and endpoints

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
