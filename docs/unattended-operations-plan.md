# Unattended Timelord operation

The deployment contract is **keep useful proof work running whenever requested
Run and available hardware/node capacity permit it**. A running GUI, coordinator,
socket, or held IPS gauge does not satisfy that contract. Explicit Pause is durable;
Quit, logout, screen lock, recovery, and reboot must not turn it into Pause.

## Ownership and data flow

```text
macOS launchd (system domain, selected non-root service account)
  └─ supervisor: durable Run/Pause, one worker owner, independent work watchdog
       ├─ private same-user control socket ← GUI or SSH CLI
       ├─ independent health/readiness/metrics endpoint → external monitoring
       └─ Rust worker: coordinator, discovery, assignments, recovery, node session
            ├─ isolated inventory helper → D2XX inventory (bounded execution)
            ├─ board owners: one C++ native client per admitted ASIC serial
            │    └─ USB driver → one or more independent ASIC VDF engines
            ├─ native proof response → cryptographic verification → scheduler
            │    → authenticated full-node peer connection → proof message send
            └─ current status + bounded historical charts + work metrics
```

The app is a controller and dashboard. It must not own production lifetime or
sleep prevention. The node may be local or remote. No GUI login or unlocked
interactive keychain may be necessary to read deployment configuration and peer
credentials. The node is an external dependency, not a child to restart whenever
our worker fails.

## Health contract

Report these independently, including their last successful time and reason when
blocked:

1. **Desired state:** Run or explicit Pause, persisted before acknowledgement.
2. **Supervisor liveness:** its event loop is responsive and owns the deployment.
3. **Worker liveness:** the current identity reports fresh progress.
4. **Capacity:** configured, physically detected, initialized, admitted, connected,
   and productive boards/engines; these counts have different meanings.
5. **Node readiness:** authenticated connection, useful current work, freshness of
   node updates, and any not-before eligibility delay.
6. **Proof progress:** request → native computation/checkpoint response → verified
   result → scheduler-ready result → completed node send, by independent chain.

A node send is not evidence of network acceptance or winning. ICC can legitimately
be idle. No short fixed proof deadline should penalize a long iteration target.
Do not sum chain IPS. Preserve slow observations and recoveries in delivered IPS;
expose freshness and last-submission age separately from the compatible gauge.

`/healthz` answers whether that process can supervise/respond. `/readyz` answers
whether its actual current capabilities permit useful work. Neither means every
board is healthy; partial capacity and chain-level readiness must remain visible.
Monitoring the independent supervisor remains possible during worker replacement
or explicit Pause.

## Recovery policy

| Fault | First repair | Escalation and containment |
|---|---|---|
| GUI quit, logout, screen lock | None; background work continues | External monitor detects supervisor loss |
| Worker crash or essential task exit | Reap its process family, replace worker | Capped retry delay, no exhausted retry budget |
| Hung async loop or proof arithmetic | Independent watchdog observes missing/work-stalled reports | Retire the execution boundary; never merely abandon a blocking task |
| Node/network unavailable | Cancel obsolete work, reconnect with capped backoff | Keep control, metrics and discovery alive; do not restart the full node |
| USB unplug/replug or stale location | Rediscover by serial and retry the affected owner | Require serial confirmation on the opened handle before settings writes |
| One board repeatedly unavailable | Continue healthy owners, reconcile usable capacity | 3 boards: 1+1+1; 2: 2+1; 1: 3; strict admin assignments remain strict |
| Overtemperature or invalid proof | Stop affected owner, persist quarantine | Cooling/readback plus isolated verified re-admission; never erase fault on worker restart |
| Failed native shutdown/readback | Mark owner unclean; retain exclusive ownership until retired | Pause all engines before replacement settings; physical USB hangs need independent hardware protection |
| Bad edited config | Reject with useful error; retain validated running config | Stage and validate replacement before retiring useful work |
| Full disk/log destination unavailable | Bound/drop diagnostic output; report degraded persistence | Never block proof/control on logging or silently discard durable Pause/safety state |

Normal recoveries are tolerated according to measured delivered performance; one
recovery is not a reason to permanently stop or declare an unsafe board. Temperature
and invalid-proof guards are independent of tuning. A software watchdog cannot
promise physical shutdown of a board whose USB driver or host has failed.

## Delivery sequence and current review stack

1. **Independent lifetime:** supervisor, non-root boot registration, durable intent,
   exclusive ownership and optional GUI. Review: app PR 9, based on the existing
   service lifecycle branch. Offline lifecycle coverage is implemented; physical
   install/boot qualification remains outstanding.
2. **Work-aware supervision:** identity-fenced stage reports, independent watchdog,
   essential task failure detection and persistent monitoring. Review: app PR 8,
   stacked on PR 9. The combined native compute/checkpoint stage is observed today;
   finer checkpoint progress is a possible later native protocol addition.
3. **Board containment and capacity recovery:** serial leases, durable safety
   journal, isolated admission checks, native EOF quiescence and usable-capacity
   reconciliation. Current work is in the isolated reliability checkout and is not
   yet a finished app PR. Native support PRs 474 and 475 cover opened-handle serial
   validation and pausing every engine before new-owner settings. Finish fault
   fixtures, documentation and full checks before
   treating this stage as complete.
4. **Operational hardening:** persistent sleep assertion, bounded non-blocking logs
   and native output, validated last-good configuration, headless credentials,
   bounded diagnostics and upgrade/rollback procedure. This stage is pending; do
   not infer these guarantees from the presence of the supervisor.

Keep each stage reviewable with explicit dependency pins and focused tests. Native
support changes remain in chiavdf rather than copied app overlays. No live M6
configuration, clocks, launchd registration or node process is changed by these
offline implementation gates.

## Qualification before datacenter rollout

Use fixture faults first: crash/hang every essential task, interrupt parent/child
ownership, persist Pause during backoff, corrupt state/config, disconnect node,
remove/re-add boards, fail verification/readback/temperature, flood output and fail
persistence. Assert actual cleanup, bounded resource use, stale-generation rejection
and continued healthy-board work, not only successful HTTP responses.

Then explicitly authorize physical tests on a dedicated host: boot without login,
GUI quit/logout/lock, USB and board power cycles, node/network outages, safe thermal
fault injection, upgrade and rollback, and power restoration. Check macOS energy
settings, FileVault/volume availability, network reachability and credential access
at boot. Use a 72-hour fault-injection soak and a seven-day monitored canary before
fleet rollout. Record time to recover, healthy-capacity availability, verified/send
progress by chain, resource growth and maximum unproductive Run interval. External
monitoring must page on persistent loss of useful work even if every process is up.
