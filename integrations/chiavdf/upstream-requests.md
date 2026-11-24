# Requested native client changes (chiavdf)

The optimized prover and USB driver stay in the pinned public chiavdf
dependency; this repository does not fork them. These are the changes the app
needs upstream, with the app-side support that already exists for each.

## 1. Bounded USB writes

**Problem.** A board's event loop can block forever inside `FT_Write`
(`event_loop → start_hw_vdf → VdfDriver::EnableEngine → FtdiDriver::WriteCmd →
FT4222_SPIMaster_MultiReadWrite → FT_Write → BulkOutRequest → pthread_cond_wait`,
recorded in `integrations/dg-xch/continuous-recovery-deployment.json`). The
process stops emitting telemetry and never answers its engine sockets.

**Request.** Set D2XX write/read timeouts (`FT_SetTimeouts`) on the opened
handle and treat a timed-out transfer as a typed fatal error: quiesce engines
where possible, report `VDF client stopped: usb_write_timeout` and exit
non-zero instead of blocking.

**App support today.** The app detects the stall from native silence after
15 s (`LIVENESS_TIMEOUT_SECONDS`), retires the owner with bounded waits and
records a `Stalled` failure. A typed native exit would make detection faster
and the cause explicit.

## 2. Control-channel heartbeat

**Request.** Emit `TLHB 1 <monotonic_ms>` on stderr at least every 2 s from
the event loop, including while engines are idle.

**App support today.** `ClientLog` already treats any `TLHB ` line as
liveness, alongside temperature and `TLRAW` records. With a heartbeat the
liveness timeout can drop to a few seconds without risking false stalls.

## 3. Machine-readable stop reason

**Request.** Keep the existing `VDF client stopped: <reason>` record, document
its reason vocabulary (`shutdown_unconfirmed`, `live_control_failed`,
`usb_write_timeout`, …) and emit it before every exit.

**App support today.** The app parses the reason and includes it in the
owner's failure (`ASIC client exited (…): live_control_failed`), its saved
history and the status API.

## 4. Close path for a stuck device (pending investigation)

When one board was force-killed while stuck, the other boards on the same hub
briefly disappeared from enumeration and their in-flight live-control requests
failed. Whether the native close path or the hub caused this is open; see
experiment 7 in `docs/hardware-qualification.md`. Do not change the close path
upstream until that experiment has a recorded result.
