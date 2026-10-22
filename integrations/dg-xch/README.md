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
peak/slot state arrives (including while signage plans omit ICC), prepares the canonical
discriminant while prior sessions retire or native clients reconnect, and converts
complete checkpointed native proofs without flattening.
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

## Completed-proof path measurement

`proof-path-trial.json` records the October 22 six-minute diagnostic capture with
three boards, one engine per board, at the existing 1600 MHz / 1.0 V operating
points. All 100 regular chain proofs were matched from native target observation
through adapter verification; 44 node messages were matched separately. The
normal six-worker overlap app and original configuration were restored afterward.
No diagnostic instrumentation is included in the product source.

The request median after target observation was 248 ms, but the 26 requests at
the 61-checkpoint limit averaged 3.76 seconds, mostly queued behind checkpoint
construction. The native planner searches queued checkpoints for reuse; completed
checkpoints have already left that queue. The capture found 240 identical
checkpoint segments recomputed after prior completion in the same native session.
Completed-checkpoint reuse and a bounded checkpoint layout merit controlled tests
before further startup micro-optimization. This is a latency profile, not a
controlled IPS gain or long-term stability result; three node-state resyncs are
recorded separately from zero ASIC recoveries and zero invalid proofs.

`optimization-status.json` keeps startup and completed-proof work as separate
tracks. Hardware SHA, the six-worker discriminant trial and preparation/retirement
overlap remain retained. Startup measurements remain open; live challenge sets
vary, so changes in their medians require a controlled replay before attribution.

The checkpoint-reuse proposal is isolated in
[chiavdf PR #450](https://github.com/Chia-Network/chiavdf/pull/450), with controlled
M6 and live validation in `checkpoint-reuse-trial.json`. It reuses owned checkpoint
objects and accounts for available proof-worker parallelism when choosing a
prefix. The app's normal native pin and integration patch are unchanged. The
original app/configuration were restored after both live captures; sustained
paired IPS measurement and long-chain coarsening remain follow-up work.
