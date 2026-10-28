# Unattended macOS deployment

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
job. `remove` unregisters the job but preserves durable Run/Pause state. Updates
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
status/history and the upcoming work watchdog distinguish those stages. A CLI
request timing out has an unknown outcome: reread status before retrying mutation.
The supervisor's read-only monitoring listener defaults to `127.0.0.1:9259` and
stays up while paused or replacing a worker. Choose a reachable
`service.supervisor_observability_listen` for remote monitoring and re-register
after changing this address or the network label. See [observability](observability.md)
for the separate worker metrics, alert matching and readiness semantics.

Logs currently go to `supervisor.log` beside config; bounded log rotation and
persistent sleep protection belong to the operational-hardening PR.

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
