# Hardware qualification

Offline fixtures exercise the board supervisors, routing and recovery rules
with scripted fake native clients (`hardware::testing::Rig`). They do not
establish physical behaviour. Each experiment below needs explicit operator
authorization for the machine, the boards and the network it runs on (see
AGENTS.md). Never run them against a production timelord as an implicit test.

Record every run as JSON under `integrations/dg-xch/` with the source commit,
native revision, host, board serials, ports, start/end times, the commands run
and the observed status snapshots. A run that is not recorded is not evidence.

## What to watch

- `timelord-service status --config …` (status API v2): per board
  `hardware_state`, `routing_state`, `consecutive_failures`,
  `failures_last_hour`, `probation_until_unix`, `cautious`, `history`.
- Worker log (`.config.toml-supervisor/worker.log`): native per-second output is
  at debug level, so decisions (`Updated chain routes`, `ASIC client stopped`,
  `Native shutdown lacked confirmed quiescence; next start is cautious`) stay
  readable for days. Automatic status shows `holding` while tuning is held.
- `board-safety.json` and `board-history.json` in the supervisor directory.
- Metrics: `chia_timelord_board_failures_total`, `chia_timelord_chain_moves_total`,
  `chia_timelord_delayed_bundles_total`, `chia_timelord_topology_changes_total`.

## 1. Unplug and replug each port

For each board in turn, while the other two produce proofs:

1. Unplug the USB cable. Expect the board's native owner to stop within about
   15 s (liveness) and its chains to move within one routing tick. Its failure
   count must **not** change: the service checks USB after the owner dies and an
   unplugged board is not a failure. It is looked for every 5 s. If USB still
   listed the board at that first check, the stop is counted and then taken
   back when the restart finds it missing (`failure_forgiven` in history, log
   `its last stop is no longer counted`). `ASIC still attached after its owner
   stopped` in the log means the check saw it.
2. Replug after 60 s. Expect a cautious start (`cautious: true`), routes that
   return only at the next peak, and a return to the saved setting after the
   first verified proof (`cautious_complete` in history).
3. Check the other boards: same native PID, no live clock change while the
   disturbance window was open (`automatic.state == "holding"` during it), no
   lost owner.

## 2. Power-cycle the hub

With all three boards on one powered hub, cut hub power for 10 s. Expect all
boards absent, routes empty, no reliability failures recorded for absence, and
a full recovery after power returns. Repeat with each board on its own hub or
host port and compare: shared-bus effects should disappear.

## 3. Kill a native client

`kill -TERM` and later `kill -KILL` one native client PID. Expect one recorded
failure for that board, a cautious restart after 1 s, probation for 60 s
(infused-challenge chain only) and no effect on the other boards.

## 4. Stall a native client

`kill -STOP` one native client PID (simulates a blocked USB write). Expect a
`Stalled` failure after about 15 s, retirement (SIGTERM, then SIGKILL; the final
reap is bounded) and a cautious restart. Resume with `kill -CONT` if needed.
Watch for the shared-bus effect seen on m6mini on 2026-11-18: other boards'
live-control requests failing at the same moment. With the disturbance hold
and serialized live control they should not issue control changes at all.

## 5. Switch the node profile while running

Use the app's connection profiles (or `timelord-service select-node` followed
by `timelord-service reload-node`). Expect `node_profile_change` in history,
the same native PIDs, unchanged `board-safety.json` health, and the chart
starting fresh for the new node. No board may restart.

## 6. Flaky-board soak (24 h)

Leave the known-flaky board (ASIC 2, `CHIA0000000032`) in for 24 hours. Expect
it to keep carrying work whenever it serves, preferring the infused-challenge
chain while it is on probation, so its stalls interrupt only that chain. The
other two boards must keep the challenge and reward chains with no owner
restarts caused by its failures, and `chia_timelord_delayed_bundles_total`
should grow far more slowly than before (stalls on the infused-challenge chain
delay fewer bundles).

## 7. Physical investigation of the shared USB effect

On 2026-11-18 one board's forced kill coincided with both other boards'
live-control failures and all three boards disappearing from enumeration for
about 6 s. To separate software from the bus:

1. Put each board on its own host port (no shared hub) and repeat experiment 4.
2. Repeat with a powered hub and with an unpowered hub.
3. Capture `log stream --predicate 'subsystem == "com.apple.iokit.IOUSBHostFamily"'`
   during the kill.
4. Note whether enumeration disappears for all boards or only the killed one.

The outcome decides whether the native client needs a different close path
for a stuck device (see `integrations/chiavdf/upstream-requests.md`).
