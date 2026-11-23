# Maintaining the dg_xch dependency

The app no longer vendors dg_xch. All five `dg_xch_*` workspace dependencies in the
root `Cargo.toml` are Git dependencies on one exact revision of the Chia-Network
fork, https://github.com/Chia-Network/dg_xch_utils. Keep all five on the same
`rev`; mixing revisions or sources duplicates `dg_xch_serialize`/`dg_xch_core` and
breaks trait and type identity across the graph. `upstream.toml` records the pins.

## Fork branches

The fork keeps changes in two layers so the first stays easy to offer upstream:

- `chia/timelord-app-patches`: upstream GalactechsLLC `ca437be` plus cameroncooper's
  HF2 timelord commits (`7b0faa7`, `eee74e9`, `3b50863`), then our upstreamable
  patches, one per logical change:
  - HF2 consensus field encoding: explicit proof header/commitment traversal and a
    shared packed-optional codec.
  - Validated `FilterSchedule` replacing the filter array plus count, frozen legacy
    defaults, and `timelord_iterations_with_context`.
  - `parse_u128` through serde_json numbers, so `arbitrary_precision` keeps working.
  - Recoverable unfinished-block admission with bounded deferral and per-peak retry.
- `chia/timelord-app-deps`: `chia/timelord-app-patches` plus dependency alignment
  with this workspace (currently num-bigint 0.5, and rsa 0.10 / x509-cert 0.3 /
  der 0.8 for certificate generation). The app pins this branch's head.

Changes that existed only because of vendoring (explicit package metadata, Git pins
inside crate manifests, removed upstream tests/benches, sqlx and prometheus removal,
change-notice headers) are not carried. sqlx stays out of `Cargo.lock` because the
dg_xch crates are no longer workspace members and nothing enables their optional
database features.

## Updating

1. Make the change in a local clone of the fork as signed commits: upstreamable fixes
   on `chia/timelord-app-patches`, dependency alignment on `chia/timelord-app-deps`
   (merge or rebuild it on the new patches head). Never force-push either branch.
2. In the fork run `cargo check --workspace --all-targets --locked`, the core and
   timelord tests with `--features dg_xch_core/chia-hf2`, and clippy for core and
   timelord, including the core feature matrix (`--no-default-features`, `bls`,
   `chia-hf2`).
3. Push the fork branch, then update the five `rev` values here together, plus
   `upstream.toml`, and refresh `Cargo.lock`.
4. Run `scripts/check.sh` and the Swift build. Run the independent Chia fixture suite
   before accepting any codec, hashing or iteration change. A deployment claiming
   testnet acceptance must separately demonstrate full-node acceptance of real PoS2
   infusion proofs; offline tests alone are insufficient.

To adopt newer upstream work, merge GalactechsLLC `main` into the patches branch in
the fork and review the result there; do not point the app at a moving branch.

## Scope

This is timelord compatibility. It does not qualify other dg_xch node roles for
current Chia consensus. Native ASIC proving and verification remain separate.
