# Unattended macOS deployment

Opening an already configured app starts a user-session supervisor if none is
available and `service.start_automatically` is enabled (the default). Run also
starts it when needed, without administrator approval. This startup does not send
Resume: saved Run/Pause intent remains authoritative. An explicit Pause survives
app relaunch and reboot. With the app open, an unavailable session supervisor is
retried at most once every 30 seconds. Quitting the interface leaves the supervisor
and requested proof work running. The full-node RPC sync display is independent
of supervisor availability and proof readiness.

Session supervision starts when the app opens. Use the system registration below
for boot operation without a GUI login or app launch; session startup reconnects
to an existing registered supervisor rather than creating another owner.

Install the signed app at `/Applications/Timelord.app`. Configure the node identity
and native client using the interface, then choose **Enable background service…**
in Settings. This is an explicit macOS administrator approval, not an automatic
privileged launch. Existing foreground versions must be paused first. The
installer refuses an occupied listener or ownership lock and never kills a process
by name or port.

The generated system LaunchDaemon runs at boot as the selected **non-root** user,
without a GUI login. Its label is `net.chia.timelord.supervisor`, with the app's
bundle identifier associated for macOS background-item attribution. The installer
uses a generated plist because the deployment username/config path vary per Mac;
it validates signatures/ownership and restores prior registration on failure.
Its root-owned plist is `/Library/LaunchDaemons/net.chia.timelord.supervisor.plist`.
It executes the bundled supervisor directly; no shell or Python is in the service
runtime. Administrative installation writes the registration; proof work is never
run as root.

For SSH installation (substitute actual paths/account):

```sh
sudo /bin/bash /Applications/Timelord.app/Contents/Resources/install-service.sh install \
  --app /Applications/Timelord.app \
  --config '/Users/operator/Library/Application Support/Timelord/config.toml' \
  --user operator
```

The same command with `plan` prints and validates inputs without registering a
job. `sudo /bin/bash scripts/install-service.sh remove` unregisters the fixed job
without requiring an intact app or configuration, and preserves durable Run/Pause
state. Keep a copy of the removal script for recovery. Updates
must retain the same config path/account so explicit Pause is preserved.
Configuration, node credentials and parent directories must be accessible to that
account without a logged-in GUI or unlocked keychain. Avoid removable or network
volumes for boot-critical identities and config.

Run control as that account (including over SSH), not as root:

```sh
sudo -u operator /Applications/Timelord.app/Contents/Helpers/timelord-supervisor status \
  --config '/Users/operator/Library/Application Support/Timelord/config.toml'
```

Replace `status` with `pause`, `resume`, or `reload` as appropriate. Mutating
commands support `--expected-revision` for concurrent administration. Pause is
persisted before acknowledgment and stops the worker family. Reload preserves
Run/Pause. A corrupt intent file keeps production stopped with `state_error` until
an explicit administrator command repairs it; it is never treated as a fresh Run.
The private supervisor directory beside config holds its socket, locks and intent.
Do not delete lock files while processes may still be alive.

Status reports supervisor/worker process identity and recovery state. It is **not**
a claim that ASICs are connected or verified proofs are being submitted. Worker
status/history and the independent work watchdog distinguish those stages. A CLI
request timing out has an unknown outcome: reread status before retrying mutation.
The supervisor's read-only monitoring listener defaults to `127.0.0.1:9259` and
stays up while paused or replacing a worker. Choose a reachable
`service.supervisor_observability_listen` for remote monitoring and re-register
after changing this address or the network label. See [observability](observability.md)
for the separate worker metrics, alert matching and readiness semantics.

Worker diagnostics go to `worker.log` inside the private supervisor directory,
with two rotated backups (5 MiB per file). A bounded queue drops diagnostic output
rather than block proof/control work if disk or logging is slow. Loss and write
errors are reported by supervisor metrics/status; worker status also reports its
own diagnostic queue. launchd stdout/stderr are discarded to avoid an unlimited
second log. A failed supervisor is diagnosed with launchctl, CLI validation and
external liveness monitoring rather than a growing crash log.

The supervisor holds an idle-system-sleep assertion during requested Run, even
while the worker or node is unavailable. Explicit Pause and supervisor exit release
it. Display sleep/lock is permitted. A failed assertion is visible and retried at
most once a minute; it never stops useful proofs. Forced system sleep, power loss
and FileVault unlock still require deployment policy and physical qualification.

After actual ready, verified work is sent, the service saves `last-good.toml` in
its private state directory. An invalid requested configuration can fall back to
that private checkpoint only when its requested network can be parsed and matches
the checkpoint. An absent/unreadable file may also use the checkpoint; malformed
TOML or an unknown/different network refuses startup. The explicit status/metric
flag identifies fallback; the requested file is never overwritten. Reload validates the requested file
in a bounded helper before retiring a useful worker. A schema-valid configuration
with an unavailable node continues retrying that dependency; the app does not
silently revert administrator endpoint choices. The checkpoint stores paths, not
copies of external certificates, keys, drivers or binaries.

All configuration, intent, safety, learning and history checkpoints use private
atomic writes and a stable per-file writer lock. One reusable pending file per
target prevents a new orphan file on every crash. Do not delete live lock files.
Configuration reads are bounded to 1 MiB. Optional profile persistence runs off the
control loop with a bounded outstanding job; failed retest reservations hold the
current point, while ordinary persistence errors leave proof production running.
Critical safety persistence remains conservative and independently supervised.
Operating-point updates carry a native-session token; a delayed update cannot
reactivate a retired owner or overwrite its successor. Optional profile saves
only enqueue healthy evidence in memory. The next required journal transaction
commits that evidence, so an optional file cannot hold the safety writer. Each
critical writer retains its watchdog identity while queued writers wait.
Thermal/invalid-proof detection retires the native owner before fault-journal I/O.
The RAM safety registry is not locked across filesystem writes, and telemetry keeps
draining while a clock-change checkpoint is pending. Optional checkpoint jobs may
remain visibly delayed with their one permit; they do not restart healthy proofs
merely because an optional destination is unavailable.

For a bounded diagnostic report without configuration contents, keys or log text:

```sh
/Applications/Timelord.app/Contents/Helpers/timelord-supervisor diagnostics --config /path/to/config.toml
```

It includes current process/work/recovery status, sleep/logging state and file
metadata. Run `timelord-service validate --config /path/to/config.toml` locally to
inspect a rejected edit. Repair the requested file and reload to leave fallback.
An explicit rollback can restore the private last-good config, then validate and
reload; keep its paths and credentials available to the same deployment account.

## Qualification before unattended operation

Offline lifecycle tests cover worker crashes, durable Pause/reload, duplicate
owners, stale commands, child-family cleanup and abrupt supervisor death with a
wedged worker. Installer plan tests do not install or load a system job. They do
not establish physical boot or hardware acceptance.

Before production: verify signed installation/upgrade/rollback, boot without login,
logout/lock, delayed network/credentials, real USB hotplug and driver failures, and
invalid-proof/thermal quarantine. Run the planned fault-injection soak and canary.
Configure system sleep and automatic restart after power failure for the deployment
policy. FileVault may require an unlock after power loss before user data and this
service become available; launchd cannot bypass disk encryption.

## Upgrade and rollback procedure

Retain the deployment account, original config path, private supervisor state and
peer credentials. Never bundle or replace Run/Pause state with an app update.
Validate the signed replacement and all bundled helpers first. Stage the app on
the same volume before replacing the installed bundle, and keep the previous signed
bundle for rollback. Use the installer's `remove` operation to unregister the old LaunchDaemon without
changing Run/Pause intent, and confirm the old supervisor/worker family has retired
before replacing files. Replace the app atomically, then use `install` to register
the new signed bundle with the same config/account and preserved intent. Do not use
Pause as a temporary upgrade flag or assume reload holds the worker stopped.
Confirm supervisor health, current readiness, native identities and new verified
submissions after resuming a previously requested Run. A deployment that was Paused
must remain Paused. If acceptance fails, restore the previous signed bundle and
registration using the same state/config/account; restore config only as an explicit
administrator action. Installer rollback restores registration on an install failure,
not an entire app/config/credential deployment. Qualify this procedure on a canary
before automating fleet updates.

A rollback target must support the durable intent and safety-state schema. The
board-recovery version retains owner epochs so rollback from operational hardening
can read the same journal. Pre-supervisor GUI-only builds are not unattended
rollback targets. Never delete the safety journal to make an incompatible reader
start; validate the target bundle/state pair on the canary first.
