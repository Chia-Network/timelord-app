# Connecting to a remote full node

Create the bundle on the computer running the full node, then transfer it to the
Mac running Timelord. The app connects directly to the full node's peer port; it
runs its own timelord service. No Python installation is needed on the Timelord Mac.

For the complete dashboard, prepare **both** connections before importing: the
timelord peer identity for proof production and the private RPC identity for sync
status and height. The importer configures both from the same `connection.toml`.
Follow all sections below in order. A peer-only bundle is made by removing
`[node.rpc]` from an exported manifest: it can produce proofs but cannot report
whether the full node is synced.

## Allow the remote timelord on the node

Chia's full node accepts localhost timelords by default. For a direct connection
from another computer, add that computer's address to the full node's
`config/config.yaml`, preserving any existing exemptions:

```yaml
full_node:
  exempt_peer_networks:
    - 192.168.1.209/32
```

Replace this example with the Timelord Mac's address, preferably a DHCP reservation.
Restart the full node after editing. Use a single-host `/32` exemption rather than
exempting the entire LAN; this setting also exempts the host from other peer limits.
TLS client authentication is still required. If the node logs
`of type TIMELORD. Inbound limit reached.`, check this setting even if no other
timelord is connected. A TLS handshake can succeed before this admission check.

Alternatively, forward the peer port through SSH to the node's loopback interface
and point `[node]` at that tunnel. This avoids changing the node's exemptions.

## Create a dedicated connection identity

The repository includes `tools/export-fullnode-connection`. Run it on the full-node
computer with the Python environment from that node's existing Chia installation:

```sh
export CHIA_ROOT="$HOME/.chia/mainnet"
/path/to/chia/venv/bin/python tools/export-fullnode-connection
```

Supply the Chia root with `--chia-root PATH` or `$CHIA_ROOT`; if neither is set,
the tool exits with an error. The remaining options have defaults:

- `--node-host HOST` overrides the suggested local routing address. The tool prints
  the suggestion; check that the Timelord Mac can reach it, especially with a VPN
  or multiple interfaces. If detection fails, supply this option explicitly.
- `--rpc-host HOST` overrides the RPC address, which defaults to the same host as
  the peer connection.
- `--rpc-port PORT` overrides the full node's configured RPC port.
- `--output PATH` overrides `./fullnode-NETWORK-connection`. Existing destinations
  are never overwritten.

The exporter reads the configured peer/RPC ports and CA paths, generates separate
peer and RPC leaf identities, and writes an owner-only bundle without changing
the node configuration. No CA signing keys are exported. `network.json` is always
written; it records `schema_version`, the network ID, genesis challenge, exported
constants and `config_overrides`, Chia version and export time.
The root itself is not needed on the receiving Mac.

RPC defaults to the same node address as the peer connection and the node's
configured RPC port. The full node must serve authenticated RPC on that reachable
private-network address. The exporter does not change its listener settings.
For an explicit SSH tunnel endpoint, use `--rpc-host 127.0.0.1 --rpc-port 18555`
and establish the tunnel on the Timelord Mac as described below.

Every import creates and selects a new **connection profile**. On the first import,
the app also preserves your existing configured connection (for example Mainnet)
as its own profile. Repeated imports create separate entries with distinct names.

Use **Full node → Remove selected profile…** to remove obsolete profiles. Removal
keeps certificate files on disk, since other profiles may reference them. Removing
the last profile returns to connection setup. Apply the change with **Done** or
**Save & Restart** (a paused service stays paused).

The **Sync status and height** section lets you edit the selected profile's RPC
host, port, TLS server name and certificate paths directly. Correct an imported
endpoint there without creating another profile. Changes apply when saved;
running Timelord uses **Save & Restart**.

Use **Full node → Connection profile** to switch. Each profile restores its network
definition, peer host/port, TLS server name, peer identity and optional RPC connection
together. Changes are applied through **Done** or **Save & Restart**, preserving the
existing Run/Pause intent. Edits made to one profile are retained when selecting
another. Selecting a Chia root also creates a profile; its certificate paths remain
references to that local installation.

For a custom network, import the entire folder including `network.json`. Its genesis
challenge and exporting Chia version are shown with the selected profile. Remote
bundle credentials are copied into private app storage, so the transfer folder is
no longer required. Switching back to Mainnet does not remove the custom profile.

The service uses the exact network ID in its peer handshake and applies exported
constants represented by the pinned scheduler, with the pinned Chia Mainnet
constants as the baseline for omitted values. Additional exported constants remain
saved for provenance; importing them does not implement new consensus features.
PoS2 verification, transition behavior and active-MMR support are unchanged. Import
or handshake success does not establish consensus or hardware acceptance. The
exporter does not test connections or run hardware.

## Bundle layout

The exporter writes:

```text
fullnode-NETWORK-connection/
  connection.toml
  client.crt
  client.key
  ca.crt
  rpc-client.crt
  rpc-client.key
  private-ca.crt
  network.json
  README.txt
```

`connection.toml` is the manifest the importer reads:

```toml
schema_version = 1

[node]
host = "192.168.1.49"
port = 8444
network = "mainnet"
server_name = "chia.net"
certificate = "client.crt"
private_key = "client.key"
ca_certificate = "ca.crt"

[node.rpc]
host = "192.168.1.49"
port = 8555
server_name = "chia.net"
certificate = "rpc-client.crt"
private_key = "rpc-client.key"
ca_certificate = "private-ca.crt"
```

Remove `[node.rpc]` from the manifest to make a peer-only bundle. `network.json`
is always written and is used for custom networks: it records `schema_version`,
the network ID, genesis challenge, exported constants and `config_overrides`,
Chia version and export time; the importer reads it. Credential paths
must be relative and stay inside the folder, including through symlinks. Keep
private keys mode 0600, transfer the folder privately, for example with `scp -r`,
and never include a CA signing key (`chia_ca.key` or `private_ca.key`). Create a
separate client identity for each Timelord installation.

## Add sync status and height

Proof production uses the peer connection above. The dashboard needs a separate,
authenticated private full-node RPC connection to display sync status and height.
Without it, proofs can run but node sync status is unavailable.
Include it for the normal app setup.

If the full node serves authenticated RPC on a reachable private-network address,
export with `--rpc-host` and `--rpc-port` set to that address and port. If RPC
stays on loopback, export a tunnel endpoint instead:

```sh
/path/to/chia/venv/bin/python tools/export-fullnode-connection \
  --chia-root "$HOME/.chia/mainnet" \
  --node-host 192.168.1.49 \
  --rpc-host 127.0.0.1 --rpc-port 18555 \
  --output "$HOME/Desktop/Timelord-mainnet-connection"
```

Run this on the Timelord Mac, substituting the node's SSH login/address and
configured RPC port (8555 for this example):

```sh
ssh -N -o ExitOnForwardFailure=yes \
  -L 127.0.0.1:18555:127.0.0.1:8555 NODE_SSH_LOGIN@NODE_HOST
```

Keep that session running while using RPC. The app does not create or supervise
SSH tunnels. The exported `[node.rpc]` section already points at the tunnel; to
change it later, re-export and import the folder again, or edit the selected
profile's RPC settings in the app. The tunnel's 18555 port is local to the
Timelord Mac; the peer connection still uses the full node's network address and
peer port. No CA signing key leaves the node computer.

A bundle supplies credentials and endpoints; it does not establish a tunnel or
change the full node's listener settings.

## Import on the Timelord Mac

Transfer the completed folder, including both sets of credentials, to the Timelord
Mac. It should contain `connection.toml`, `client.crt`, `client.key`, `ca.crt`,
`rpc-client.crt`, `rpc-client.key`, `private-ca.crt`, `network.json`, and
`README.txt`.

1. Open the Full node connection settings and choose **Another computer**.
2. Choose **Import connection bundle…** and select the folder or `connection.toml`.
3. Confirm the **Sync status and height** section shows a status connection.
4. While Timelord is paused, choose **Test connection** to test the peer connection.
5. Choose **Done**, or **Save & Restart** if applying to a running service. Confirm
   the dashboard reports sync status and height from RPC.

The importer copies both identities into the app's private configuration folder,
so the original transfer folder can be moved afterward. Keep a protected copy if
it is needed for reinstallation. A successful peer connection test does not
establish node sync or consensus acceptance of proofs.
