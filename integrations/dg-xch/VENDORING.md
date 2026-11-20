# Maintaining the dg_xch vendor copies

The app owns two crates under `vendor/`: core and timelord. They are normal Cargo
workspace members, tested by the app CI. Root `[patch]` entries replace those
original Git packages throughout the dependency graph. `servers` is no longer
vendored; `dg_xch_servers` comes from the unchanged upstream revision
`ca437be562cec5f0514f6edd4c794adfe382dd8a`. No custom build-time patching,
sibling checkout or fork download is required.

`vendor-origin.json` records the imported HF2 snapshot, original upstream base,
source-file checksums and mechanical import adjustments. The initial import is
commit `69eeaa4`; later commits contain the maintained modifications. Licenses are
retained in each crate. The chain-definition fixture is copied under
`vendor/config`; this is test data, not an additional vendored crate. Upstream crate READMEs are retained as historical
source documentation; use the app README for supported builds and operation.

## Local compatibility changes

- PoS2 witness checks use the official `chia-pos2` crate. Consensus field writers
  distinguish witness serialization from quality commitments explicitly. Packed
  optional fields share one codec, with independent Chia golden vectors.
- Candidate admission owns hashing and returns the admitted identity. Candidate
  errors are recoverable and do not tear down the node session or other work.
- Filter schedules have private bounded storage, validated ordering and serde
  validation. Scalar filter bounds are checked in the consensus/scheduler layer.
  Defaults used to omit fields from legacy identity serialization are frozen
  independently of the evolving MAINNET constants.
- Unknown transaction context is retained. An invariant iteration target can be
  used immediately; ambiguous candidates enter a bounded queue and are retried on
  each new peak. Stale/reorged candidates are discarded through ordinary checks.
  Signage work continues while candidate context is missing.
- The RPC space-estimate parser uses serde_json's number representation so the
  app's arbitrary-precision feature does not break upstream floating-point inputs.
- Core's optional `sqlx` dependency, its `mysql`, `postgres` and `sqlite`
  features, and the sized-bytes database codecs are removed. The timelord stores
  nothing in SQL. Cargo locks sqlx's optional backends even when they are
  disabled, which kept RUSTSEC-2026-0459 (`libsqlite3-sys` 0.30.1) in the
  lockfile, and the fixed sqlx 0.9 line needs Rust 1.94. Do not restore these
  features when syncing upstream unless this app needs a database.

This is timelord compatibility. It does not qualify other dg_xch node roles for
current Chia consensus. Native ASIC proving and verification remain separate.

## Updating the snapshot

1. Fetch a proposed upstream revision into an explicit temporary checkout. Never
   edit Cargo's source cache or infer a nearby checkout.
2. Compare its core and timelord crate directories against the recorded source revision and
   the maintained vendor diff. Port relevant changes as reviewable commits; do
   not overwrite local fixes or replace all dependencies with a moving branch.
3. Preserve package names, versions, licenses and authorship. Keep explicit
   package metadata, the local core path and exact Git pins for other crates.
   Include any required test fixtures/helpers, and record their source hashes.
4. Update provenance deliberately when adopting a new baseline. Keep the prior
   origin record in Git history. Update the unchanged dependency pin only after
   checking its API and consensus compatibility with the maintained copies.
5. Run `python3 tools/check-dgx-vendor`, `cargo fmt --check`, workspace Clippy with
   warnings denied, workspace tests, core feature-matrix checks in
   `scripts/check.sh`, and the Swift build. Run the independent Chia fixture suite
   before accepting any codec, hashing or iteration change.
6. Verify an archive/fresh checkout builds without local overrides. A deployment
   claiming testnet acceptance must separately demonstrate full-node acceptance
   of real PoS2 infusion proofs; offline tests alone are insufficient.
