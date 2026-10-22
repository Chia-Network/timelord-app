# Rust timelord integration

Pinned dependency: [dg_xch_utils](https://github.com/GalactechsLLC/dg_xch_utils/tree/ca437be562cec5f0514f6edd4c794adfe382dd8a),
revision `ca437be562cec5f0514f6edd4c794adfe382dd8a`, Apache-2.0.
See upstream.toml, LICENSE, NOTICE and Cargo.lock. No upstream source is patched.

The app links the public regular Scheduler, protocol/consensus types, TLS/framing
helpers and VDF verifier. Our `runtime.rs` owns the node session and injects our
`asic.rs` backend directly when executing scheduler plans. Upstream's regular
service/CLI is not used: it constructs a CpuBackend and recomputes each request.

The adapter keeps one logical-chain session per assigned board, retains squaring
across requests with identical generation/challenge/input, primes the validated infused-chain input as soon as
peak/slot state arrives (including while signage plans omit ICC), derives the canonical
discriminant, and converts complete checkpointed native proofs without flattening.
The ASIC adapter verifies the proof; Scheduler.finish also checks generation,
request identity and consensus proof validity before sending a node message.

Normal resets drain proofs through STOP/ACK and close the old connection; timeout
or cancellation closes the lease. Cancellation cannot leave a partial native frame
in a reused socket. Node disconnects cancel workers and drain retained sessions.
Invalid ASIC results latch a hardware fault and stop owned clients.

The offline checkpoint fixture is generated and independently verified by canonical
C++ chiavdf. The adapter has tests for persistent successive requests, partial-frame
cancellation, cryptographic corruption, strict TLS identity and native log guards.
Initial local ASIC/mainnet smoke testing verified real proofs and a stored signage
proof pair. Wider consensus and timing still require replay and sustained acceptance.
Current Chia adds an optional header MMR root to unfinished-block messages; our
adapter accepts only the absent-root extension or legacy framing, with captured
message regression coverage. Active MMR roots require upstream consensus support.
The existing live Python coordinator has not been replaced or used as a test fixture.

## ARM SHA and six-worker trial

ARM app builds enable RustCrypto `sha2/asm` through the service manifest. This
unifies with the verifier's SHA dependency and uses its runtime-detected ARM SHA
backend with its existing software fallback. There is no custom app SHA kernel.

`six-worker-discriminant.patch` is an experimental upstream proposal against the
revision above. It is **not applied by the normal app build**. It uses one shared
six-thread pool for discriminant searches, preserves the existing probable-prime
predicate and memo, and waits for earlier candidate indices before selecting a
winner. Proof-form hash searches stay serial. Tests cover serial agreement,
seed counter carries and a deliberately delayed earlier passing candidate.

The October22 M6 trial was built from the pinned source plus this patch in an
isolated checkout; no Cargo cache sources were modified. Its provenance and
measurements are in `six-worker-trial.json`. The trial service is separate from
the default reproducible build until an upstream revision can be pinned. The
upstream repository currently provides read access to this account, so publishing
the proposal requires a writable fork or help from an upstream maintainer.
