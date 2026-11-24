# Exported network override fixtures

Each file is an exporter-shaped `network.json` (`schema_version`, provenance,
`constants`, `config_overrides`) whose `constants` map is the verbatim
`network_overrides.constants[<network>]` block from the named `chia-blockchain`
release. Chia never prunes keys from that map, so these fixtures pin the
historical keys (including `SOFT_FORK4_HEIGHT` and `SOFT_FORK5_HEIGHT`) that
custom-network configurations carry after upgrades.

`chia-2.4-testnet11-fork.json` uses the `testnet11` override block from Chia
2.4.0 under a custom network ID, because `testnet11` itself is a built-in
Timelord network and cannot carry an imported definition. Everything else is a
real Chia network name.
