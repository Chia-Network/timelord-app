# Architecture

```text
SwiftUI app + menu bar → Rust coordinator ↔ existing Chia full node
                                 ↕ local TCP
                         native chiavdf clients
                                 ↕ USB
                             ASIC boards
```

`apps/macos` contains the interface and pure presentation model. `timelord-core`
contains configuration validation and chain assignment without I/O.
`vdf-client-protocol` bounds and decodes native frames without treating them as
verified proofs. `timelord-service` owns node scheduling, verification, process
supervision, telemetry and Automatic tuning. The optimized prover and USB driver
remain in the pinned public chiavdf dependency; only the live-control integration
is maintained here. The app has no Python, wallet, full node or software-VDF mode.

## Process ownership

One coordinator owns its native clients. The UI starts and stops only its own
coordinator, and matches status/history by service PID. It may observe an external
service without taking ownership. All configured listeners are bound before work
starts, and board initialization is sequential. A configured board or connected
socket is not evidence of functioning hardware.

App startup waits for configuration, physical enumeration and a confirmed absent
status listener. Invalid/stale/timed-out responses do not establish absence.
It makes one startup attempt per app launch and respects Pause, unsaved settings
and shutdown. Starting at macOS login and crash-independent launch-agent ownership
are outside the current implementation.

Discovery resolves D2XX enumeration exports only: it never opens or initializes
an ASIC. Serial identity survives USB-port changes. The active plan includes only
detected boards; one/two/three boards receive 3 / 2+1 / 1+1+1 engines. Reassignment
requires restart. The UI keeps a separate registry of disconnected board settings.

## Proof path

`runtime.rs` uses the pinned dg_xch regular scheduler over an authenticated Chia
WebSocket session. `asic.rs` maps scheduler work to persistent native chain sessions,
derives discriminants, and verifies complete checkpointed proofs before handing
them to the scheduler. The scheduler independently validates request identity,
generation and consensus validity before encoding a node message. Primer ICC work
keeps that chain running; primer proofs are never counted or submitted.

Successive requests retain work within a generation/challenge/input. Resets cancel
pending work, invalidate the generation and drain STOP/ACK before closing the old
session. Interrupted partial-frame I/O retires its connection rather than reusing
it. Retired sockets half-close and drain with a deadline to accommodate the native
client's final STOP write. Node outages cancel work; invalid ASIC results latch a
hardware fault. No parsed proof is accepted without cryptographic verification.

The pinned scheduler's unfinished-block type predates the optional header MMR root.
A bounded exact decoder handles absent roots, rejecting malformed extensions and
active roots until the scheduler supports them. This node-protocol adapter is
necessary for current Chia interoperability; it is not app configuration migration.

## Tuning and health

Automatic and Manual use four auxiliary threads, three proof threads and 250,000
iteration checkpoints. Automatic policy decisions use proof observations whose
work started after the new setting epoch and settling allowance. Every assigned
chain must have enough observations; the score is the minimum independent-chain
mean. Idle time, old in-flight work and missing temperature do not justify a raise.

The native client remains the sole USB owner. Its event loop applies bounded,
sequenced commands through inherited pipes, pauses active engines, orders voltage
and clock writes, verifies readback and resumes. Invalid commands, failed readback
or controller disappearance quiesce engines and terminate the child. Rust adds
sampled temperature/missing-telemetry guards and fault reporting.

Learning state is keyed by board serial, engine count, host model and client hash.
Only observed settings or confirmed recovery fallbacks are persisted, atomically
and with owner-only permissions. A private same-user Unix socket can hold/resume
tuning; it cannot change operating points or control arbitrary processes.

## Metrics and validation

Proof timings include native session startup, verification, scheduler gating and
node send. The UI plots verified sent-proof events by independent chain, with
recovery observations retained and capture gaps explicit. A send is not proof of
node acceptance. History is bounded to 24 hours/86,400 observations, checkpointed
atomically beside the config, and restored with explicit capture gaps. API
responses still match the current service owner; restored records never populate
current health, proof counters or hardware state. Node/network/chain-assignment
changes reset the history context. A stale gauge is not current productivity.

Tests use canonical C++ proof fixtures, generated TLS identities, local peers and
owned fake processes. Presentation scenarios compile only in the test target.
Hardware/consensus acceptance, physical live transitions and clean-machine
installation remain separate validation work; see README for current limits.
