# Architecture

```text
macOS launchd → independent Rust supervisor → Rust coordinator ↔ Chia full node
                       ↑                            ↕ local TCP
                SwiftUI / SSH CLI           native chiavdf clients
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

Within the service, `main.rs` handles CLI/configuration and `service.rs` owns
topology generations, listeners and task lifetimes. `status_api.rs` projects
that state into HTTP responses. Each runtime requires the existing
`operational::State` at construction: it owns the process health latch, progress
tracking and telemetry registry across topology replacements. HTTP endpoints
resolve that owner through the current runtime rather than carrying independent
process or registry handles. `hardware/board.rs` owns each board's
retry loop, `hardware/native.rs` its subprocess and telemetry, and
`hardware/tuning.rs` the acknowledged native control session. `automatic.rs` is
pure tuning policy; `automatic/profiles.rs` owns saved evidence and machine identity.
A frequency trial owns its rollback point and kind, so a retest cannot exist
without an active trial.


## Process ownership

The non-root boot supervisor owns durable Run/Pause intent, exclusive coordinator
ownership and crash replacement. The GUI is an optional same-user control client;
quitting it never sends Pause. Mutating commands use revisions to reject stale
intent updates. Pause is persisted before acknowledgment and survives reboot.
Reload retires the worker family without changing desired Run/Pause.

Each worker owns a process group. Normal replacement terminates descendants before
reaping the leader; a dedicated lifetime-pipe thread terminates a worker whose
supervisor disappears and hard-kills its group after 30 seconds even if its async
runtime is wedged. Neither mechanism establishes USB hardware quiescence under a
wedged driver; native and physical fault qualification are separate requirements.
Exclusive worker locks prevent a second coordinator from starting. Installation
refuses an existing legacy listener instead of terminating arbitrary processes.
All configured listeners are bound before work starts, and board initialization is
sequential. Worker presence, configured hardware and sockets are not productivity.

Discovery calls only D2XX inventory APIs in a disposable helper process; it sends
no engine, voltage or clock commands. The proof service never loads the USB driver.
A ten-second timeout kills and reaps a stuck helper instead of leaving a blocking
thread running. Helper crashes, invalid JSON and bounded-output failures are
inventory errors, not an empty inventory or a reason to stop healthy boards.
The driver stays loaded until its helper exits, avoiding unload while its USB
background threads may still be running. Serial identity survives USB-port changes. The active plan includes only
detected boards; one/two/three boards receive 3 / 2+1 / 1+1+1 engines. The service
polls USB every five seconds and requires three consistent inventories before
replacing the layout. Enumeration errors leave the existing layout intact; fresh
supervised owners cover metadata omitted by open FTDI interfaces. An idle chain
needs no leased proof socket. Client health and USB presence are separate: a
known-present board stays in the layout during transport or sensor recovery. Only
a successful inventory taken after its native owner has closed can confirm that
its serial is absent. The 60-second sensor watchdog still stops and retries the
affected client; latched safety faults remain excluded from presence fallback. Old scheduler,
client listeners and native owners stop before new ones start, while the status
HTTP listener stays available. Zero boards waits for reconnection. Each layout
selects its own engine-count/mask tuning profile, and safety faults remain latched
across transitions. Explicit serial-pinned mappings do not redistribute. A changed
assignment starts a new chart scope and run counters; old chain samples never
become observations for another board. The service and UI keep disconnected
board settings by serial. Configuration edits still require a manual restart.

## Proof path

`runtime.rs` uses the pinned dg_xch regular scheduler over an authenticated Chia
WebSocket session. `asic.rs` maps scheduler work to persistent native chain sessions,
derives discriminants, and verifies complete checkpointed proofs before handing
them to the scheduler. A process-wide arithmetic permit bounds real blocking
work to one job per chain, including a job whose async caller has been cancelled.
The job retains the permit until it finishes; replacing a backend or USB topology
does not reset that capacity. Different chains retain independent capacity. The scheduler independently validates request identity,
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
Recoveries are permitted. Only a sustained, configured loss of delivered IPS in
consecutive recovery-containing windows triggers recovery backoff. The policy
holds stable settings and performs no downward voltage search. The independent
hard verification and hardware guards remain authoritative.

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

The status API reports `service_started_unix` once per coordinator process and
`started_unix` once per engine-assignment session. The dashboard shows Service
uptime with the engine-session age beneath it. Topology replacements still reset
session proof counters and chart scope, but never the displayed service uptime.
