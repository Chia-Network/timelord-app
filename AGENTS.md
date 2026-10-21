# Timelord Mac

This is the standalone timelord-app repository. All source, configuration examples,
fixtures, build scripts and dependency pins must work from an arbitrary checkout.
Never infer a sibling or parent chiavdf checkout. Do not create a GitHub repository
or publish without a user request.

Build this as a maintained product: native SwiftUI app, Rust background service,
existing native chiavdf ASIC clients. Keep configuration, protocol, node scheduling,
and presentation separate. Do not add a Python runtime or fork the optimized C++
prover into this repository.

Use `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
`cargo test --workspace`, and `swift build --package-path apps/macos` for relevant
changes. Protocol and lifecycle changes need meaningful regression coverage.

Never use the live timelord, its ports, certificates, node, or USB boards as an
implicit test fixture. The development ports are 9257 (status) and 18110–18112
(client connections). Hardware tests, operating-point changes and mainnet service
replacement require explicit user authorization for that experiment.

Treat received proofs as unverified until the native verifier accepts them. Never
show a configured board as detected hardware, a socket connection as verified work,
or a held/stale IPS gauge as current productivity. Never add the IPS of independent
chains. Status JSON is versioned and must not expose credentials.

The pinned upstream Rust scheduler is linked through our native-client adapter.
Offline fixtures do not establish hardware/testnet acceptance or production
compatibility. Keep exact provenance in integrations/dg-xch and preserve the
validation boundary in README as implementation progresses.
