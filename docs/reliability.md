# Unattended reliability implementation

The requested deliverable is four focused PRs. These requirements are the scope,
not claims that current code already satisfies them. Physical qualification and
canary deployment are separate from the offline implementation gates.

## 1. Independent service lifetime and durable intent

- A small Rust supervisor owns durable Run/Pause state, child replacement and
  same-user control. It performs no node, proof arithmetic or USB operations.
- A system LaunchDaemon starts at boot without a GUI login, under an explicitly
  selected non-root deployment account. Packaging installs the supervisor alongside
  the worker and provides an approval-based installation/removal flow.
- The GUI controls the installed supervisor rather than owning the worker. Quitting
  the interface leaves desired intent and background operation unchanged.
- Pause persists before acknowledgment, quiesces the worker family, survives reboot
  and suppresses delayed/retrying starts. Run and automatic-tuning pause are separate.
- Atomic owner-only state, control-peer checks, revisions and exclusive process
  locks prevent silent intent loss, stale control updates and competing owners.
- Worker exit replacement uses capped retries without exhausting them. A process
  group is retired before replacement; a lifetime pipe stops a worker if its
  supervisor disappears. Reboot and abrupt-parent cleanup must be tested explicitly.
- Status describes process state honestly; worker existence is not productivity.

Current foundation: the Rust supervisor and worker lifetime/ownership changes are
implemented. Boot-registration generation, the non-root installer and bundle
integration are implemented with a fixture-only plan check; no system job has
been installed in development. GUI migration and abrupt-parent coverage remain
required before PR 1 is ready for review.

## 2. Work-aware supervision and observability

- Reuse the existing deployment-observability PR (#8), preserving metric semantics.
- Observe every essential task exit immediately; do not keep a nominally healthy
  HTTP server around a stopped scheduler or board-supervision task.
- Carry per-run and per-request identities through bounded stage snapshots.
- Track discovery, native ownership, iteration progress, checkpoint construction,
  verification, scheduler eligibility and actual node submission independently.
- Use current work and iteration targets for deadlines; legitimate ICC idleness,
  cancellations, node-not-before gates and long work must not cause restart storms.
- Timeouts must terminate/reap their execution boundary, not abandon blocking work.
- A watchdog independent of proof computation replaces a hung worker. Repair the
  smallest failed component first; escalate only after bounded attempts.
- Expose health, readiness, metrics and bounded recovery records while degraded or
  paused. A submitted message is not evidence of global acceptance or winning.
- Never restore old gauges, proof counters or safety readiness as current health.

## 3. Board fault containment and recovery

- Reconcile eligible healthy capacity into 3, 2+1, or 1+1+1 engines; physical presence
  alone is insufficient. Honor explicitly strict administrator mappings.
- Preserve per-serial, per-engine-count/mask, host and binary learning profiles.
  Never apply a single-engine clock as an unmeasured three-engine operating point.
- Enforce one native owner per board, bounded cleanup and generation fencing.
- Confirm the expected serial on the actual opened USB handle before applying
  that board's operating point (narrow upstream chiavdf change).
- Persist thermal/invalid-proof quarantine across full service crashes and reboot.
- Re-admit only after appropriate fresh cooling/readback checks or offline verified
  self-tests at a safe operating point. Never submit an unverified or invalid proof.
- Keep healthy boards useful while a board is absent, recovering or quarantined.
- Serialize tuning experiments and suspend exploration during degraded operation;
  retain ordinary recovery tolerance and configured voltage/thermal guards.
- Validate native EOF/termination quiescing, driver deadlines, and independent
  board thermal-protection behavior. Do not assume software can stop hung hardware.

## 4. Operational hardening

- Bound logs, queues, subprocess output, history and memory growth. Disk/logging
  failure must not block the proof/control pipeline or exhaust the system volume.
- Keep last-known-good configuration with atomic validation/reload and diagnostics;
  never silently replace operator intent or credentials with defaults.
- Keep configuration and peer identities accessible without an unlocked GUI;
  authenticate local mutation separately from public read-only observability.
- Retain the sleep assertion in the persistent service, including recoverable
  dependency outages, and release it when explicitly paused.
- Tolerate delayed network, volumes and certificates; retain control and monitoring
  while unavailable and retry without crashing the supervisor.
- Provide remote CLI status, pause/resume, reload and bounded diagnostic export.
- Package/verify all helper binaries and document launchd permissions, non-root
  deployment identity, FileVault unlock, power restoration and upgrade migration.

## Gates for each PR

Run Cargo formatting, clippy with warnings denied, workspace tests, applicable
Swift build/tests and packaging/installer input tests. Use fixture-only processes,
sockets, credentials and nodes. Do not install launchd jobs or change the live M6
system as an implicit test. Preserve unrelated checkout changes.

Lifecycle fixtures must include restart after crash and shutdown, durable Pause,
Pause during startup/backoff/cleanup, stale commands, malformed state, duplicate
owners, parent death, worker descendants and actual worker EOF shutdown.
Further PRs add stage-specific stalls, task exits, node outage, hotplug, invalid
proofs, thermal quarantine, full disk and GUI quit/logout fixtures. Hardware smoke
tests, boot tests and a 72-hour fault-injection soak / seven-day canary are required
before claiming unattended production qualification, not substitutes for code gates.
