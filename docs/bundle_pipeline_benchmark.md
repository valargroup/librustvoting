# Bundle pipeline release benchmark

Run `make bench-bundle-pipelines`. The target builds once, then starts fresh
processes for concurrency one and five with both cold and prewarmed ZKP2 caches.
Each sample runs six independent wallet/sidecar bundle pipelines through the
real round driver, combined atomic casting, chain confirmation, and helper
delivery. Each bundle generates two real ZKP2 proofs. Delegation authorization
uses the existing cached-proof fixture; this benchmark does not measure ZKP1
preparation or a live chain. `make proofs` separately verifies real ZKP1 and
cold-cache behavior. The round-driver refill conformance test covers more than
five bundles within one round.

The scripted peers delay chain POST, chain status, and helper delivery by 250 ms.
They assert one combined envelope, complete confirmation, and successful sibling
share delivery. Workload shape and delay are identical between samples; proof
randomness remains cryptographically fresh.

Optional environment settings:

- `BUNDLE_BENCH_COUNT`: number of pipelines, default 6.
- `BUNDLE_BENCH_DELAY_MS`: each controlled transport delay, default 250.
- `BUNDLE_BENCH_WORKERS`: shared CPU workers, default available parallelism.
- `BUNDLE_BENCH_HEAVY_JOBS`: active heavy jobs, default available parallelism.

Each `BUNDLE_BENCH` JSON record includes SDK workload wall time, bundle/proposal
counts, configured worker/job limits, accumulated admission waits, and cache mode.
The Python harness also emits process wall time, CPU seconds, CPU utilization
(100 percent is one CPU), and peak RSS in bytes. OS measurements include test
runner overhead and warm-up; the SDK timer excludes prewarming for warm samples.
Builds occur before measured samples. Resource usage is measured in separate
wrapper processes so RSS peaks are not inherited from a previous sample.

Record the commit, backend, hardware, OS, and environment settings with results.
Compare equivalent cache modes. Expect overlapping pipelines and bounded proving
resources; do not assert a universal speedup. Controlled network waits, memory
pressure, and internal proof parallelism determine which workload benefits.

## Recorded development sample

Measured on 2026-09-09 against the implementation worktree based on `c6b80d47`,
using the default Zakura backend on an Apple M4 Max, 128 GiB RAM, macOS 26.2.
All samples use six bundles, two proposals per bundle, 250 ms transport delays,
16 shared workers, and a 16-heavy-job limit. Compilation finished before these
samples; no verification jobs from this implementation ran alongside them.
This is a shared development machine with unrelated workloads, not a controlled
performance host. CPU utilization below therefore measures this workload's
observed CPU use, not total machine utilization or available CPU capacity.

| Cache | Pipelines | SDK wall (s) | Process wall (s) | CPU (s) | CPU utilization | Peak RSS (MiB) | Admission wait (µs, sum) |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| cold | 1 | 22.318 | 24.924 | 23.525 | 94.4% | 183.8 | 60 |
| cold | 5 | 13.475 | 16.410 | 23.131 | 141.0% | 327.9 | 137 |
| warm | 1 | 25.277 | 30.783 | 23.685 | 76.9% | 179.5 | 77 |
| warm | 5 | 13.356 | 18.264 | 23.121 | 126.6% | 308.7 | 89 |

The samples demonstrate pipeline overlap with one shared worker pool and a
higher bounded memory footprint at concurrency five. The resource conformance
tests assert worker and heavy-job limits directly; this table is a measurement,
not a hardware-independent throughput assertion. Cache modes are separate runs;
background load can outweigh the cost saved by prewarming.
