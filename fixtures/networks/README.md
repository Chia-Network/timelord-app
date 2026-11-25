# Exported network override fixtures

Each file is an exporter-shaped `network.json` (`schema_version`, provenance,
`constants`, `config_overrides`) whose `constants` map is the verbatim
`network_overrides.constants[<network>]` block from the named `chia-blockchain`
release. Later upstream *templates* may omit keys that earlier templates had:
`SOFT_FORK3_HEIGHT` is present in the 2.1.0 fixture but absent from the 2.3.0
fixture because the 2.3.0 template dropped it. Installed user configurations
are not rewritten on upgrade, so those historical keys can still appear in the
field. These fixtures preserve each template as published, including later keys
such as `SOFT_FORK4_HEIGHT` and `SOFT_FORK5_HEIGHT` where the named release has
them.

`chia-2.4-testnet11-fork.json` uses the `testnet11` override block from Chia
2.4.0 under a custom network ID, because `testnet11` itself is a built-in
Timelord network and cannot carry an imported definition. Everything else is a
real Chia network name.
