# Deployment health and metrics

The Rust service serves Prometheus metrics directly. No exporter or websocket
metrics client is needed for the timelord. The macOS UI and proof computation are
unchanged. The optional deployment listener is disabled by default:

```toml
[service]
status_listen = "127.0.0.1:9257"
observability_listen = "0.0.0.0:9258"
```

Restart the service after changing listeners. Use a private deployment network or
an authenticated reverse proxy for remote access. The deployment listener exposes
only the three endpoints below; status, history and controls remain local. It has
no built-in authentication or TLS. All listener ports must be distinct, including
ASIC client ports. An occupied listener causes startup to fail with its bind error.
Automatic USB assignment chooses unused client ports starting at 18110, skipping
both HTTP listener ports. If those overlap the usual 18110–18112 range, board
ports can extend above 18112. An unexpected deployment listener exit latches
health as failed on the local status endpoint while proof work continues.

| Endpoint | Response |
| --- | --- |
| `/metrics` | Prometheus text format, available through node/board outages and topology replacement |
| `/healthz` | Plain `200 Ok` while essential supervision tasks function; `503` after an unexpected exit or during shutdown |
| `/readyz` | JSON `{"ready":true,"reason":"ready"}` with 200; otherwise 503 and a fixed reason |

All three are also on the existing loopback status listener. `/health`,
`/v1/status` and `/v3/history` retain their existing contracts. Endpoints only read
cached state: no USB calls, node RPC, or chart-file reads. Scrapes copy bounded
current hardware fields and do not traverse chart histories.

Readiness requires node authentication, initialized scheduler state and usable
clients for required chains. Managed clients must finish native initialization,
have socket capacity, and have temperature telemetry no older than 30 seconds.
Unmanaged clients must verify work in their current connection generation.
ICC is required only while the adapter has ICC work pending. An idle ICC engine
and paused automatic tuning do not independently fail readiness. A node outage or
recoverable board failure affects readiness, not liveness. Topology replacement
is live but temporarily unready. Reasons are `setup_required`,
`waiting_for_node`, `waiting_for_chain_state`, `waiting_for_clients`,
`reconfiguring`, `service_unavailable`, and `ready`.

## Metric contract

All application series carry `network` (`mainnet` or `testnet11`). Prometheus
supplies region, instance, job and deployment labels. Board labels use configured
serials, falling back to configured IDs. Physical engine numbers are `0`, `1`,
`2`; recovery events without an engine use `unknown`. Socket slots are not
physical engine identities. `chain_assignment` associates boards with chains;
`raw_ips` associates boards with physical engines. No ambiguous multi-engine
socket-to-chain mapping is invented.

One process-owned registry retains counters and event timestamps across USB
runtime replacements. Counters start at zero and reset only on process restart.
Unobserved measurements are absent. Saved chart history never populates live
metrics. State and assignment labels are replaced when owners change. Counters
for previously seen boards remain, preserving process totals after removal.

All names below have the prefix `chia_timelord_`:

| Metrics | Type / extra labels | Meaning |
| --- | --- | --- |
| `version`, `build_info` | gauge; `version`, plus `revision` on build info | Actual service package version / build revision, value 1 |
| `process_start_time_seconds` | gauge | Start of this service process |
| `healthy`, `ready` | gauge | Probe state, 0 or 1 |
| `topology_changes_total` | counter | Applied topology replacement attempts |
| `estimated_ips` | gauge | Latest validated completion from any chain in this node session |
| `chain_estimated_ips` | gauge; `chain` | Latest validated completion for each chain in this session |
| `proofs_verified_total`, `proof_verification_failures_total` | counter; `chain` | Native verifier outcomes for scheduled work; failures include priming checks |
| `last_verified_proof_timestamp_seconds` | gauge; `chain` | Wall-clock time of the most recent verified scheduled completion |
| `submissions_total`, `last_submission_timestamp_seconds` | counter / gauge; `kind` | Successful node writes of signage, infusion or end-of-sub-slot bundles |
| `board_up` | gauge; `board` | Board passes managed/unmanaged readiness checks |
| `board_state` | gauge; `board,state` | Current state as a one-hot value 1 |
| `board_temperature_celsius`, `board_frequency_hertz`, `board_voltage_volts` | gauge; `board` | Observed operating measurements; omitted without current telemetry |
| `raw_ips`, `raw_sample_timestamp_seconds` | gauge; `board,engine` | Running engine's native rate and observation time; expire after six seconds |
| `chain_assignment` | gauge; `board,chain` | Current assignment, value 1 |
| `asic_recoveries_total` | counter; `board,engine` | Native recovery log events |
| `client_restarts_total` | counter; `board,reason` | Replacement native processes actually launched |
| `thermal_stops_total` | counter; `board` | New thermal guard stops, counted once per latch |
| `node_connected`, `node_reconnects_total` | gauge / counter | Authenticated link and reconnections after the first authenticated connection |
| `last_node_message_timestamp_seconds`, `last_peak_received_timestamp_seconds` | gauge | Last observed node frame / decoded peak |
| `peak_updates_total` | counter; `result` | Scheduler accepted, ignored or rejected peak updates |
| `scheduler_peak_height`, `deficit` | gauge | Accepted peak values, absent before a peak or after session replacement |
| `work_pending` | gauge; `chain` | Adapter has an unfinished scheduled request, 0 or 1 |
| `work_cancelled_total` | counter; `chain` | Adapter requests dropped without completion |
| `vdf_sessions_started_total` | counter; `chain,reason` | Native challenge initialization acknowledged; initial, input change or reconnect |
| `clients_configured`, `clients_connected` | gauge | Assigned client capacity and observed sockets (not proof readiness) |
| `chain_required`, `chain_available` | gauge; `chain` | Required work roles and usable assigned capacity, for alerts |
| `tuning_paused` | gauge | Automatic tuning paused, not proof service paused |
| `tuning_state` | gauge; `board,state` | Current bounded tuning state |
| `operating_point_changes_total` | counter; `board,reason` | Acknowledged trial, backoff or restore writes |
| `proof_duration_seconds` | histogram; `chain,stage` | Observed proof pipeline boundaries below |
| `submission_duration_seconds` | histogram; `kind` | Successful bundle write duration |

Chains are `challenge`, `reward`, `infused_challenge`. Bundle kinds are `signage`,
`infusion`, `end_of_sub_slot`. All reason/state labels use fixed enumerations;
error text, paths, hashes, request IDs and heights are never labels.

The legacy `estimated_ips` uses existing receipt-time elapsed work and truncation
to tenths, published only after successful native verification. It is absent
before the first completion and cleared on session loss/replacement. Between
completions it holds its last value; it is not current productivity. Consult
verified-proof timestamps and counter progress for freshness. For an exporter-like
window average, average actual scrapes of this held gauge, not per-proof samples.
Independent chain rates must never be summed. A successful submission is a write
to the node, not node acceptance or a network win. ICC priming is not a scheduled
completion and does not update proof counters or IPS.

`proof_duration_seconds` stages use monotonic clocks:

- `discriminant`: actual blocking preparation execution, including cancelled work
  that finishes preparing; excludes worker scheduling delay.
- `client_wait`: lane ownership and native socket availability wait.
- `request_to_verified`: complete scheduled adapter request through verification,
  including waits/recovery retries. Cancelled requests have no completion sample.
- `verification`: native verification work plus its blocking-worker wait.
- `bundle_wait`: time from a chain's verified completion until all required chain
  proofs are ready. Cancelled bundles have no observation.

Stages overlap and must not be summed. Cached discriminants do not create a new
preparation observation. The raw/delivered IPS gap is not a measurement of proof
construction cost. Pending work is only what the adapter observes; private
scheduler queues are not approximated. No scheduler fork is required.

## Scraping, alerts and rollout

Examples are in `monitoring/prometheus.yml`, `monitoring/kubernetes-probes.yml`
and `monitoring/alerts.yml`. Probes use liveness to detect internal failures and
readiness to detect external availability. They must not restart the service
merely for an offline node or recoverable USB failure.

Alerts use counter progress and timestamps rather than `rate(estimated_ips)`.
No-progress alerts retain a 15-minute mainnet / two-hour testnet pending-work
window, after a five-minute counter lookback. Ordinary ASIC recoveries are allowed
and are not independently an alert. Inspect recovery rates together with delivered
IPS and temperatures before adjusting a deployment's policy. Alert examples also
cover scrape/metric loss, internal failure, sustained node disconnection, missing
required clients, verification failure and thermal stops.

Keep full-node monitoring with the existing full-node exporter. This service does
not emit `chia_full_node_*`, compact-proof series, exporter build information,
arbitrary debug values, or legacy fastest/slow aliases without equivalent scheduler
semantics. Block/signage alerts and the node's timelord-connection metrics continue
to come from the full-node exporter.

Audit dashboard/alert selectors for `job`, `application`, `component`, `network`,
region and instance names before changing targets. Our examples use job
`timelord-app`; existing deployments may use a different job or relabeling scheme.
Do not scrape both old/new timelord exporters as one logical instance or aggregate
their duplicate IPS series. First validate one deployment alongside existing
monitoring, compare aligned raw scrape windows, then replace scrape targets after
review. This PR performs no live deployment or operating-point changes.

## Validation

Install Prometheus (`brew install prometheus` supplies `promtool` on macOS).
`scripts/check.sh` runs the Rust/Swift checks plus exposition/rule validation.
For just the observability checks:

```sh
TIMELORD_TEST_EXPOSITION=/tmp/timelord-fixture.prom cargo test --workspace --locked
promtool check metrics < /tmp/timelord-fixture.prom
promtool check rules monitoring/alerts.yml
promtool test rules monitoring/alerts.test.yml
```

Fixtures use isolated sockets, generated TLS identities and simulated native
proofs, never production node/USB state. Offline checks establish contracts, not
hardware performance or production network acceptance.
