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
remain in the pinned public chiavdf dependency, including its live-control channel;
no native source overlays are maintained here. The app has no Python, wallet, full node or software-VDF mode.

Within the service, `main.rs` handles CLI/configuration and `service.rs` owns
topology generations, listeners and task lifetimes. `status_api.rs` projects
that state into HTTP responses. Each runtime requires the existing
`operational::State` at construction: it owns the process health latch, progress
tracking and telemetry registry across topology replacements. HTTP endpoints
resolve that owner through the current runtime rather than carrying independent
process or registry handles. `hardware/board.rs` owns each board's
retry loop, `hardware/native.rs` its subprocess and telemetry, and
`hardware/tuning.rs` its optional Automatic policy. Native control belongs to the
session in `hardware/native.rs`. `automatic.rs` is
pure tuning policy; `automatic/profiles.rs` owns saved evidence and machine identity.
A frequency trial owns its rollback point and kind, so a retest cannot exist
without an active trial.

## Process ownership

The non-root boot supervisor owns durable Run/Pause intent, exclusive coordinator
ownership and crash replacement. The GUI is an optional same-user control client;
quitting it never sends Pause. Mutating commands use revisions to reject stale
intent updates. Pause is persisted before acknowledgment and survives reboot.
Reload retires the worker family without changing desired Run/Pause.

The coordinator has one process-lifetime operational owner for health, lifecycle
phase and progress supervision. Topology replacements retain that owner and its
failure latch. HTTP and Prometheus project the same state; the exporter does not
own service lifecycle decisions.

Each worker owns a process group. Normal replacement terminates descendants before
reaping the leader; a dedicated lifetime-pipe thread terminates a worker whose
supervisor disappears and hard-kills its group after 30 seconds even if its async
runtime is wedged. Neither mechanism establishes USB hardware quiescence under a
wedged driver; native and physical fault qualification are separate requirements.
Exclusive worker locks prevent a second coordinator from starting. Installation
refuses an existing legacy listener instead of terminating arbitrary processes.
Required local listeners are bound before work starts, and board initialization is
sequential. Worker presence, configured hardware and sockets are not productivity.

Discovery calls only D2XX inventory APIs in a disposable helper process; it sends
no engine, voltage or clock commands. The proof service never loads the USB driver.
A ten-second timeout kills and reaps a stuck helper instead of leaving a blocking
thread running. Helper crashes, invalid JSON and bounded-output failures are
inventory errors, not an empty inventory or a reason to stop healthy boards.
The driver stays loaded until its helper exits, avoiding unload while its USB
background threads may still be running. Serial identity survives USB-port changes. Managed board owners reserve three physically identified engine sockets. Automatic
layouts route chains over current healthy capacity (3 / 2+1 / 1+1+1 engines),
without replacing healthy native processes or the node session. USB discovery runs
every five seconds to find new/returning serials; failed discovery does not remove
live owners. Known disconnected boards retain independent recovery supervisors.
Routing observes lifecycle changes every 500 ms. Only a changed chain route is
cancelled; generation checks reject stale results. Explicit mappings do not
redistribute. Route changes start a fresh chart scope so old chain samples cannot
become observations for another board. Configuration edits still require restart.

## Proof path

`runtime.rs` uses the pinned dg_xch regular scheduler over an authenticated Chia
WebSocket session. `asic.rs` maps scheduler work to persistent native chain sessions,
derives discriminants, and verifies complete checkpointed proofs before handing
them to the scheduler. The process owner's typed arithmetic resources bound real
blocking work to one job per resource, including a job whose async caller has been
cancelled. Production and isolated-admission resources remain independent. The
actual job retains its permit and progress guard until it finishes; replacing a
backend or USB topology does not reset that process capacity. The scheduler independently validates request identity,
generation and consensus validity before encoding a node message. Primer ICC work
keeps that chain running; primer proofs are never counted or submitted.

Successive requests retain work within a generation/challenge/input. Resets cancel
pending work, invalidate the generation and drain STOP/ACK before closing the old
session. Interrupted partial-frame I/O retires its connection rather than reusing
it. Retired sockets half-close and drain with a deadline to accommodate the native
client's final STOP write. Node outages cancel work; invalid ASIC results latch and persist a
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

## Durable ownership and continuous recovery

`hardware/lifecycle.rs` owns readiness, faults and generation-fenced proof evidence.
`BoardSupervisor` owns indefinite retry policy. `PreparedAttempt` retains the serial
lease through initialization and retirement. Managed production owners initialize
in place; they do not run a disposable owner and require its successful shutdown
before reopening USB for production. Manual owners have no tuning policy.

The private journal records the owner epoch before USB opens. Confirmed shutdown
and reaping clear it; an interrupted or unconfirmed owner requests conservative
recovery. Restrictive updates remain in memory if persistence fails; relaxation
requires a durable epoch-fenced checkpoint. Invalid private journal contents are
archived and replaced conservatively; insecure paths are rejected. Run/Pause intent
is separate and an explicit Pause remains durable.

Transport and unknown-owner faults retry after 1, 5, then at most 30 seconds.
Thermal faults retain a 30-second cooldown, then probe at 200 MHz / 0.7 V and
require fresh temperature below both the configured limit minus 5°C and 50°C.
Invalid proofs retain a 10-second cooldown and must pass a private canonical
360-iteration proof through the same live owner before production routing resumes.
The private check has no node task, production metrics or public listener. All
production proofs independently require native verification. No fault exhausts a
retry budget, including Manual invalid proofs. Repeated real thermal/verification
failures lower recovery frequency by 50 MHz per failed point, down to 200 MHz;
recovery can operate below optimization preferences without editing them.

`hardware/continuous.rs` owns per-board supervisors/listeners and updates routes
among healthy boards. `asic/pool.rs` parks unused physical engines. Route and owner
generation fencing prevents stale proofs from entering a new chain assignment.
Missing temperature or failed control still retires only the affected owner.
Native retirement keeps stdout/stderr readers and partial records alive across
soft deadlines, accepts only complete positive evidence, and separately reaps the
process. Missing confirmation remains unknown and triggers fresh conservative
initialization. It never certifies physical quiescence of an unresponsive device.

Exploratory tuning still uses the shared experiment lease. Optional persistence
cannot terminate a healthy owner; critical journal writes retain independent
watchdogs and epoch fencing. Status exposes the recovery reason, retry deadline
and actual native PID, separately from verified productivity.

USB serial leases use the service account's private persistent `~/.timelord-usb-locks` directory, resolved through the account database. Deployment accounts need an owned writable home; `HOME` and temporary-directory cleanup do not select or rotate the namespace.

Critical point changes carry the native session's opaque journal owner token.
Beginning a native owner advances the epoch once; changing a point cannot create
an owner, revive a retired one or mutate a successor. The safety writer publishes
restrictions in memory before disk I/O and commits relaxations only after a durable
checkpoint. Active point updates run off the async executor while the owner keeps
draining thermal logs and observing shutdown. Only the actual critical writer owns
the persistence watchdog, so a queued caller cannot replace its deadline.

Optional learned profiles have one bounded process-wide writer permit retained by
the actual blocking job across generation replacement. Their wait is bounded and
still drains native logs; a retest requires confirmed persistence of its retry
reservation before changing the live point. A failed optional checkpoint retains
the current owner. Successful healthy profiles enqueue epoch-fenced evidence in
memory; the next required safety transaction can durably clear the safe restriction.
They never acquire the critical journal writer. Journal and status JSON schemas
remain unchanged.
