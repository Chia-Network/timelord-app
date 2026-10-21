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
