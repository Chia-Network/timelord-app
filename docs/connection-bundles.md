# Connecting to a remote full node

Create the bundle on the computer running the full node, then transfer it to the
Mac running Timelord. The app connects directly to the full node's peer port; it
runs its own timelord service. No Python installation is needed on the Timelord Mac.

For the complete dashboard, prepare **both** connections before importing: the
timelord peer identity for proof production and the private RPC identity for sync
status and height. The importer configures both from the same `connection.toml`.
Follow all sections below in order. A peer-only bundle is a deliberate reduced
setup: it can produce proofs but cannot report whether the full node is synced.

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
export CHIA_ROOT="/Volumes/LaCie/chia/test-2026-11-13-2-chia"
/path/to/chia/venv/bin/python tools/export-fullnode-connection
```

All three options are optional:

- `--chia-root PATH` overrides `$CHIA_ROOT`. One of these must be supplied.
- `--node-host HOST` overrides the suggested local routing address. The tool prints
  the suggestion; check that the Timelord Mac can reach it, especially with a VPN
  or multiple interfaces. If detection fails, supply this option explicitly.
- `--output PATH` overrides `./fullnode-NETWORK-connection`. Existing destinations
  are never overwritten.

The exporter reads the configured peer/RPC ports and CA paths, generates separate
peer and RPC leaf identities, and writes an owner-only bundle without changing
the node configuration. No CA signing keys are exported. `network.json` preserves
the selected network's consensus/config overrides, Chia version and export time.
The root itself is not needed on the receiving Mac.

RPC defaults to an SSH tunnel at `127.0.0.1:18555`; the generated `README.txt`
contains the command with the node's actual RPC port. Replace `NODE_SSH_LOGIN`
and keep the tunnel running on the Timelord Mac. For direct private-network RPC,
edit `[node.rpc]` before importing, as described below.

For a custom network, import the entire folder including `network.json`. The app
adds the imported network to the Network selector and shows its genesis challenge
and exporting Chia version. It saves the definition with its private configuration;
the transfer folder is no longer required after import. Switching to a built-in
network clears the custom selection; import its bundle again to return to it.

The service uses the exact network ID in its peer handshake and applies exported
constants represented by the pinned scheduler, with the pinned Chia Mainnet
constants as the baseline for omitted values. Additional exported constants remain
saved for provenance; importing them does not implement new consensus features.
PoS2 verification, transition behavior and active-MMR support are unchanged. Import
or handshake success does not establish consensus or hardware acceptance. The
exporter does not test connections or run hardware.

For a manual bundle instead, follow the steps below.

Use the Python environment from the node's Chia installation. Replace the first
two values below with that environment's Python executable and the full node's
address as reachable from the Timelord Mac. This example is for mainnet, port 8444.
For testnet11, use `network = "testnet11"` and the node's configured peer port.

```sh
chia_python="/path/to/chia-blockchain/venv/bin/python"
node_host="192.168.1.49"
bundle_dir="$HOME/Desktop/Timelord-mainnet-connection"

"$chia_python" - "$bundle_dir" "$node_host" <<'PY'
import json
import os
import sys
from pathlib import Path
from chia.ssl.create_ssl import get_chia_ca_crt_key, generate_ca_signed_cert

os.umask(0o077)
bundle = Path(sys.argv[1])
bundle.mkdir(mode=0o700, parents=True, exist_ok=False)
ca_crt, ca_key = get_chia_ca_crt_key()
generate_ca_signed_cert(ca_crt, ca_key, bundle / "client.crt", bundle / "client.key")
(bundle / "client.key").chmod(0o600)
(bundle / "ca.crt").write_bytes(ca_crt)
(bundle / "connection.toml").write_text(
    'schema_version = 1\n\n[node]\n'
    f'host = {json.dumps(sys.argv[2])}\n'
    'port = 8444\nnetwork = "mainnet"\nserver_name = "chia.net"\n'
    'certificate = "client.crt"\nprivate_key = "client.key"\n'
    'ca_certificate = "ca.crt"\n'
)
print(f"Created {bundle}")
PY
```

This creates a fresh client identity using Chia's public peer CA, without changing
the full node's certificates. The node's private CA is used separately for RPC.
The output folder contains:

```text
Timelord-mainnet-connection/
  connection.toml
  client.crt
  client.key
  ca.crt
```

For reference, the manifest is:

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
```

Credential paths must be relative and stay inside the folder, including through
symlinks. Keep private keys mode 0600. Transfer the folder privately, for example
with `scp -r`, and never include a CA signing key (`chia_ca.key` or `private_ca.key`).
Create a separate client identity for each Timelord installation.

## Add sync status and height

Proof production uses the peer connection above. The dashboard needs a separate,
authenticated private full-node RPC connection to display sync status and height.
Without it, proofs can run but node sync status is unavailable.
Include it for the normal app setup.

On the node computer, generate a dedicated RPC leaf identity using its private
CA. Set `node_root` to the Chia root and keep the `chia_python` and `bundle_dir`
variables from above. These paths assume the standard Chia SSL layout; use the
paths in the node's `config.yaml` if customized.

```sh
node_root="/path/to/chia/mainnet"
"$chia_python" - "$node_root" "$bundle_dir" <<'PY'
import os
import sys
from pathlib import Path
from chia.ssl.create_ssl import generate_ca_signed_cert

os.umask(0o077)
root, bundle = map(Path, sys.argv[1:])
ca = root / "config/ssl/ca"
ca_crt = (ca / "private_ca.crt").read_bytes()
generate_ca_signed_cert(ca_crt, (ca / "private_ca.key").read_bytes(),
                        bundle / "rpc-client.crt", bundle / "rpc-client.key")
(bundle / "rpc-client.key").chmod(0o600)
(bundle / "private-ca.crt").write_bytes(ca_crt)
PY
```

Full-node RPC normally listens on loopback. Keep it there and forward it through
SSH. Run this on the Timelord Mac, substituting the node's SSH login/address and
configured RPC port (8555 for this example):

```sh
ssh -N -o ExitOnForwardFailure=yes \
  -L 127.0.0.1:18555:127.0.0.1:8555 NODE_SSH_LOGIN@NODE_HOST
```

Keep that session running while using RPC. The app currently does not create or
supervise SSH tunnels. Before transferring/importing the bundle, append this to
`connection.toml`:

```toml
[node.rpc]
host = "127.0.0.1"
port = 18555
server_name = "chia.net"
certificate = "rpc-client.crt"
private_key = "rpc-client.key"
ca_certificate = "private-ca.crt"
```

For an already-imported bundle, import the updated folder again. The tunnel's
18555 port is local to the Timelord Mac; the peer connection still uses the full
node's network address and peer port. No CA signing key leaves the node computer.

If the node already serves authenticated RPC on a reachable private network,
use that address and its RPC port in `[node.rpc]` instead of the loopback tunnel
address. A bundle supplies credentials and endpoints; it does not establish a
tunnel or change the full node's listener settings.

## Import on the Timelord Mac

Transfer the completed folder, including both sets of credentials, to the Timelord
Mac. It should contain `connection.toml`, `client.crt`, `client.key`, `ca.crt`,
`rpc-client.crt`, `rpc-client.key`, and `private-ca.crt`.

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
