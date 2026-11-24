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
tracking and telemetry registry for the process lifetime. HTTP endpoints
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
phase and progress supervision. Node-profile switches retain that owner and its
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
uptime with the engine-session age beneath it. Node-profile switches still reset
session proof counters and chart scope, but never the displayed service uptime.

## Durable ownership and continuous recovery

There is one hardware path: each configured serial has a persistent
`BoardSupervisor` (`hardware/board.rs`) and at most one native owner. The
service never replaces its process, node session or healthy owners because a
board changed; USB inventory updates only adjust board records and routes.

`hardware/lifecycle.rs` is the board state machine (idle → starting → checking
→ connected, with reconnecting, thermal-stop and invalid-proof faults). Routing
and status read it; only the owner supervisor and fenced proof reports change it.
A board serves only after its native owner acknowledged its setting, every
engine socket connected and fresh telemetry is cool.

`safety.rs` is the private per-serial journal (schema 2, upgrading schema 1):

- **Owner epoch.** Recorded before USB opens. Confirmed shutdown and reaping
  close it; an owner left active at startup means a cautious next start.
- **Restriction.** Only `thermal` and `invalid_proof`. They keep their cooldowns
  and recovery checks (200 MHz / 0.7 V probe and fresh cool telemetry; a
  private canonical 360-iteration proof through the live owner) and lower the
  recovery clock limit 50 MHz below the failing point. Six hours of fault-free
  serving raise the limit again.
- **Cautious.** An unconfirmed stop, a crash or an unknown board starts at the
  recovery point and returns to its best-known point after verified work.
- **Health.** Failure streak, recent failures, last cause, probation deadline
  and operator disable state (`hardware/health.rs`). Absence is not a failure:
  a transport failure followed within 120 s by the board missing at restart
  restores the health from before it (`failure_forgiven` in history).

Restrictive updates publish in memory before disk I/O; relaxations publish only
after a durable checkpoint. Invalid journal contents are archived and replaced
conservatively; insecure paths are rejected. Run/Pause intent is separate.

`hardware/routing.rs` is pure routing policy. Every serving, enabled board
carries work; reliability only orders which board carries which chain (the most
reliable takes the challenge chain, a board on probation the infused-challenge
chain). Only a non-serving owner or an operator disable leaves a board without
chains.
It keeps chains on their boards while those stay eligible, uses a fixed board
order and defers optional moves to the next peak. `hardware/continuous.rs` runs
it every 500 ms, records route changes and holds Automatic tuning on every board
for 20 seconds after any route change, owner start or retirement. Live
operating-point changes are serialized across boards (`LIVE_CONTROL`).

Faults are handled by the smallest part first. A dropped proof session retries
on the same owner; a second lane error within 120 s, a routed engine that never
connects, 15 s of native silence or failed control retires only that owner.
Native retirement keeps stdout/stderr readers alive across soft deadlines,
accepts only complete positive evidence, bounds every wait including the final
reap after SIGKILL, and never certifies physical quiescence of an unresponsive
device. Restart delays follow the saved failure streak (1, 5, 30 s).

Exploratory tuning uses the shared experiment lease. Optional persistence cannot
terminate a healthy owner; critical journal writes keep independent watchdogs
and epoch fencing. Learned profiles keep a separate best-known stable point; a
cautious start never saves one. Status (API v2) exposes per board the recovery
reason, retry deadline, routing state, failure streak, probation, cautious start
and recent saved board events (`board-history.json`, best-effort), separately
from verified productivity.

USB serial leases use the service account's private persistent
`~/.timelord-usb-locks` directory, resolved through the account database.
Deployment accounts need an owned writable home; `HOME` and temporary-directory
cleanup do not select or rotate the namespace. Another process holding a lease
makes the board "busy", not failed.

Critical point changes carry the native session's opaque journal owner token.
Beginning a native owner advances the epoch once; changing a point cannot create
an owner, revive a retired one or mutate a successor.

A full-node profile change is applied by the running service
(`timelord-service reload-node`): it stops the node session, retargets chart
history to the new node and starts a new session. Native owners, routes, tuning
and board health are untouched; the metrics `network` label follows the switch.
Other configuration edits still require a supervised restart.

Fault scenarios are exercised offline by `hardware::testing::Rig`: scripted fake
native clients with a fixture USB inventory, the real supervisors and the real
routing loop, including a regression for the 2026-11-18 m6mini cascade. A
randomized routing simulation checks the routing invariants. Physical behaviour
needs the experiments in `docs/hardware-qualification.md`.
