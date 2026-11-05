# Single-engine default

A board assigned one chain uses physical engine 1 (zero-based), native mask `2`.
Two-engine boards retain engines 0/1 (mask `3`); three-engine boards use all engines
(mask `7`). Engine selection is fixed, not searched by automatic tuning.

Automatic profiles include the physical engine mask in their identity. Older
profiles without a mask describe the former lowest-numbered engines, so an old
single-engine profile is not reused for engine 1. Existing two-/three-engine
profiles remain applicable.

## Hardware evidence — October 23, 2026

One engine per board was active, at requested and acknowledged 1.0 V. The native
client SHA256 was
`dd8cbeae65e0d1dbefb7aae08bfd01b074bea1efa5941ddef9dad592a7a9563e`.
Each screen used two distinct 1024-bit discriminants and 20M iterations per proof;
proofs were independently verified, including rejection of iteration-tampered
proofs. Retained values, checkpoint threshold 250000, four auxiliary threads and
three proof threads were used throughout. Guards stopped a trial at 55°C or 20
recoveries within 10 seconds.

| Board / serial | 1920 MHz | 1960 MHz | 2000 MHz |
| --- | --- | --- | --- |
| ASIC 1 / CHIA0000000116 | All engines: 2 proofs, 0 recoveries each | Engine 1: 2 proofs, 0 recoveries; engines 0/2 hit cutoff | Engine 1: 2 proofs, 11 recoveries; engines 0/2 hit cutoff |
| ASIC 2 / CHIA0000000032 | Engines 1/2: 2 proofs, 0 recoveries each; engine 0 hit cutoff | Engine 1: 2 proofs, 0 recoveries; engines 0/2 hit cutoff | All engines hit cutoff |
| ASIC 3 / CHIA0000000063 | All engines: 2 proofs, 0 recoveries each | Engine 1: 2 proofs, 0 recoveries; engines 0/2 hit cutoff | All engines hit cutoff |

ASIC 2 was also tested in engine order **0 → 1 → 1 → 0**, at both 1920 and
1960 MHz, with two 30M-iteration proofs per successful trial. In both crossovers,
engine 0 hit the cutoff before and after engine 1 completed four verified proofs
with zero recoveries. Engine 0 failed while cooler than the clean engine-1 runs.

At 1960 MHz engine 1 delivered approximately 1.262M IPS for the long screening
proofs, with approximately 1.274M raw IPS. These short tests demonstrate different
usable clock limits; they do not establish overnight stability, precise maximum
clocks, or a live Grafana-compatible mean improvement. 1940/1980 MHz were not tested.
There were no thermal stops or independently rejected proofs in these trials.

Initial simultaneous USB initialization caused device-open failures. Those setup
failures were excluded, and every reported comparison used serialized native
initialization with acknowledged clocks and physical-engine telemetry checked.
The live app was restored to its original settings after testing.

If later controlled measurements show another engine outperforming engine 1,
consider adding engine comparisons to calibration. Current evidence supports the
simpler fixed default.
