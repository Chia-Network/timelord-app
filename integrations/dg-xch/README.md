# Rust timelord integration

Pinned dependency: [dg_xch_utils](https://github.com/GalactechsLLC/dg_xch_utils/tree/ca437be562cec5f0514f6edd4c794adfe382dd8a),
revision `ca437be562cec5f0514f6edd4c794adfe382dd8a`, Apache-2.0.
See upstream.toml, LICENSE, NOTICE and Cargo.lock. No upstream source is patched.

The app links the public regular Scheduler, protocol/consensus types, TLS/framing
helpers and VDF verifier. Our `runtime.rs` owns the node session and injects our
`asic.rs` backend directly when executing scheduler plans. Upstream's regular
service/CLI is not used: it constructs a CpuBackend and recomputes each request.

Custom Chia network imports resolve the exact exported network ID and supported
constant overrides in our `network.rs` adapter, using pinned Mainnet constants as
the baseline. They do not use upstream's DGX seed-derived chain format. Newer
constant fields are retained for provenance without changing upstream consensus
capabilities. The scheduler, PoS2 verifier and message codecs remain at the pin
above; offline custom import/handshake tests do not establish transition support
or live consensus acceptance.

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
Invalid ASIC results stop the affected owner's work, not other owners or the
node session. Managed owners recover indefinitely with cooldowns, fresh hardware
initialization and private verification after invalid proofs. Unknown retirement
evidence requests conservative recovery rather than a permanent admission gate.
Every production proof still requires native verification and scheduler checks.

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

The initial user-requested PR #450 deployment at 1600 MHz is recorded in
`pr450-deployment.json`. The subsequent live build is recorded in
`live-ips-ceiling-deployment.json`: it retains checkpoint reuse, hardware SHA,
six-worker discriminant search, preparation/retirement overlap and GMP/FTDI
libraries, and adds recent raw IPS telemetry and a 2200 MHz Automatic ceiling.
Boards resume at their prior points and explore supported PLL steps under the
requested 1.0 V ceiling and 55°C guard. A short deployment health check does not
establish stability at the maximum clock or a sustained performance gain.
This remains an explicit runtime development build; normal dependency pins are
unchanged.

The follow-up display fix is recorded in `ips-dropout-fix-deployment.json`. It
keeps valid five-minute statistics visible between proofs and reports explicit
raw sampling/idle states while preserving the proof and tuning implementations.

`recovery-storm-deployment.json` records the subsequent service-only fast-backoff
update. The normal recovery-cost policy remains in place; severe bursts bypass
proof-score gating and live control continues draining native logs. The recorded
1920 MHz trace requests backoff at five seconds in the offline regression.

`readback-retry-deployment.json` records the native control-only update: initial
and final readback each allow three attempts, 20 ms apart, while engines remain
paused. Rejected samples include separate clock-register snapshots. PLL/voltage
writes are never retried, and persistent unexpected state still stops the client.
The service, prover, UI machine code and libraries remain unchanged. The explicit
development deployment preserves learned points, ceilings and cooldowns for the
new native hash; this is not automatic cache migration in normal app builds.
The 60-second live health check did not inject or reproduce a hardware fault.

`board-recovery-deployment.json` records per-board supervision and bounded client
restarts, with healthy native sessions and the full-node peer connection retained
during an isolated hardware outage. Offline socket/process/TLS fixtures exercise
recovery, exhaustion and safety latches. The live check verifies deployment health
only; it does not inject a physical board fault or establish an IPS improvement.

`continuous-recovery-deployment.json` records the user-authorized November 13
custom-testnet deployment. The injected native SIGTERM recovered automatically
while retaining the supervisor, worker, healthy owner PIDs and the exact node TCP
connection through the recovery check. Other chains delivered verified proofs
during recovery. A separate ASIC 2 native timeout also recurred and recovered;
a process sample locates the stall in FTDI `BulkOutRequest` during engine enable.
The other two owners delivered 88 additional proofs in a subsequent 50-second
capture, with zero verification failures. The USB stall remains unresolved; this
is not an all-board stability result or a PoS transition test.
