# Helper delivery and immediate-share confirmation benchmark

This procedure compares the continuous queue at 16 and 32 active share workflows.
The initial POST ceiling stays 128. It also separates SDK waits from the time
between helper acceptance and visible chain confirmation. Run the wallet yourself;
no helper deployment or live wallet operation is part of the SDK test suite.

## Revisions and workload

Use the commit `feat: trace helper delivery and confirmation boundaries` as the
instrumented 16-slot baseline and its successor
`perf: admit 32 concurrent helper share deliveries` as the candidate. Record full
commit hashes, SDK and wallet versions, release build settings, network/round,
helper fleet in configured order, and UTC start/end times with each capture.
Both revisions must use the same wallet build configuration and report options.

Run at least three fresh 3-bundle × 37-proposal rounds for each revision,
alternating 16, 32, 16, 32, 16, 32. Keep bundle sizes, proposal count, configured
fleet, and preparation state comparable. Record whether delegation preparation,
proof keys, and other caches were precomputed or reused. Do not restore completed
share deliveries or replay an accepted round: duplicate responses measure reuse,
not fresh delivery. Preserve production randomness and placement policy.

Each clean run should produce 111 vote proofs and 1,776 distinct shares. The
reported 1,776 successful initial POSTs is the expected count only with the
original placement configuration (one definite target per share). With larger
fleets, derive the expected accepted placements from each persisted target. Keep
HTTP attempts, queued/duplicate acknowledgements, accepted placements, failed
attempts, ambiguous outcomes, and recovery POSTs as separate counts.

## Capture

Retain serialized `OperationObservability` snapshots and the authoritative domain
results from the existing reported wallet entry points:

- `RoundDriver::run_with_report` for the main run;
- `confirm_pending_share_with_report` for every focused confirmation invocation;
- `ShareTrackingDriver::run_with_report` (or `track_pending_shares_with_report`
  when the host drives individual passes) for background tracking.

Use `ObservabilityOptions { max_records: 65_536, max_summary_groups: 65_536,
..Default::default() }` at each boundary. Save diagnostics before propagating an
error. A readable summary alone cannot reconstruct concurrency. Record all three
drop counters; if any are nonzero, increase the relevant limit for a fresh run or
mark affected metrics incomplete. A long-running tracker freezes its snapshot
only when its invocation returns. Preserve the final snapshot on cancellation.
Host-controlled gaps between separate invocations require those invocations'
wall-clock anchors; they are not SDK timers.

Keep one local manifest per run and wallet. Use the persisted
`RoundPlan::immediate_share_key` (the round executor's plan projection), not a
new calculation over current choices. `submit_at = 0` alone does not designate
the immediate share. Record:

- bundle index, proposal ID, share index, and the immediate designation;
- the confirmed vote's `ConfirmedVote::vc_tree_position()`;
- `submit_at`, `created_at`, and placement target from `share::list`;
- the validated configured helper order for each invocation/pass, including
  changes during background tracking;
- the corresponding share reveal nullifier locally if joining a chain transaction
  requires it, so replaced generations cannot be mistaken for the same share.

Extract only this metadata from existing SDK results and read APIs. Do not export
whole recovery bundles, wallet databases, request payloads, or signing material.
The SDK diagnostics themselves contain neither endpoint URLs nor nullifiers.
For a fleet refreshed during a tracking run, retain the existing host context
and pass-event timestamps with the manifest; an ordinal is scoped to that fleet,
not a global identity across runs or fleet changes.

## SDK stages

These are implementation labels, not a required fixed sequence. Older reports
can lack them. All offsets and elapsed values below are microseconds.

| Stage | Meaning |
| --- | --- |
| `helper::delivery_queue_wait` | Enqueue to acquiring a share permit; cancellation before admission is `cancelled`. Includes residence before the future is first polled. |
| `helper::active_delivery` | Permit acquired through per-share execution and durable outcome processing. `succeeded` means at least one accepted placement, not chain confirmation. |
| `helper::share_lock_wait` | Waiting for the generation-qualified per-share operation lock, attributed to the actual share. The enclosing operation distinguishes delivery and confirmation. |
| `helper::post_capacity_wait` | Waiting for the initial POST semaphore, inside the fan-out deadline. |
| `helper::post_share` | Parsed initial/direct POST result after retries: `pending` means `queued`, `reused` means `duplicate`, and `possibly_dispatched` means ambiguous. |
| `helper.http.post_json` / `helper.http.get` | One transport attempt. A 2xx is not by itself semantic acceptance or confirmation. |
| `helper::persist_acceptance` | The definite acceptance write; success requires the expected generation still to match. |
| `helper::share_status` | Parsed helper response: `pending` or `succeeded` (confirmed), or failure. |
| `helper::confirmation_quorum` | Complete configured-helper quorum search; one confirmed helper need not establish quorum. |
| `helper::persist_confirmation` | Generation-qualified write after quorum; `pending` means the generation no longer matched, `failed` means storage failed. |
| `helper::confirmation_reused` | Confirmation already present on entry; does not claim a new poll or inclusion timestamp. |
| `helper::tracking_wait` | Actual SDK wait between tracking passes, including waits interrupted by cancellation. |

Helper operation, retry, and HTTP records carry `endpoint_index` when the caller
provided a validated fleet. It is zero-based in that original configured order,
not health order or completion order. Direct calls without fleet context leave it
absent. `attempt` restarts for each parent operation. Join by invocation and
parent IDs as well as share attribution; record IDs are not global.

`active_delivery` describes placement evidence on return. If a cancelled active
workflow returned no placement, this stage can be `failed` while its containing
batch is `cancelled`; use the authoritative batch result for cancellation.

## Delivery comparison

For each bundle and the whole run, report delivery elapsed time, initial HTTP
window, request count, mean/p50/p95/p99 latency, request throughput, average and
peak HTTP occupancy, active-share occupancy, and queue/permit waits. Keep main
run completion and immediate-share confirmation completion separate.

Convert a record to `[started_after_us, started_after_us + elapsed_us)`. For
HTTP occupancy, use only `helper.http.post_json` records whose ancestors identify
initial delivery. Exclude recovery and status requests. Sweep start/end events
(end before start for equal timestamps) to find peak concurrency. Average
occupancy is the sum of attempt durations divided by the interval from first
attempt start to last attempt end, including idle gaps. Throughput is completed
attempts divided by that same interval. Use nearest-rank percentiles over
completed attempt durations; count unfinished/clipped attempts separately rather
than treating them as normal samples. Use `active_delivery` intervals separately
to measure share occupancy. They are not interchangeable when a share fans out
to multiple helpers.

Do not sum parent/child elapsed times or add overlapping bundle phases. A queue
wait is already part of overall delivery time. For records from separate
invocations use `started_at_unix_us + started_after_us` only when clocks are
comparable; label wall-clock jumps and uncertainty rather than hiding them.
Reports clipped at return, dropped records, or missing invocation captures cannot
support exact peak/average claims. Scope occupancy claims to captured operations;
unobserved concurrent wallets still consume process-wide capacity.

Report medians and ranges across runs, plus per-helper latency and errors. The
32-slot change is useful if delivery improves consistently without increased
failures or ambiguous outcomes; a faster single run is not sufficient evidence.
Keep 64 out of this comparison. The prior 555.8 seconds of accumulated request
duration gives idealized references of 34.7 seconds at 16 HTTP slots and 17.4 at
32, only if comparable request work and latency persist. These are not predicted
wall times, and 32 share workflows do not necessarily mean 32 HTTP requests.

## Immediate-share timeline

Build a timeline for the designated share's exact generation across the main
run and every focused/background confirmation invocation:

1. Queue entry and admission; initial POST response and acceptance write.
2. Helper enqueue, effective schedule, processing start, and proof generation.
3. Reveal transaction broadcast, committed inclusion (height and transaction
   identity), and helper-visible committed-nullifier state.
4. Client polls, successful quorum, and durable SDK confirmation.

Existing helper `helper.enqueue` and `helper.process_share` traces can carry
round, proposal, share index, tree position, and schedule. Plain `share received`
logs precede the enqueue attempt and alone do not establish acceptance.
Several plain `proof generated` / `share submitted` messages identify only round
and share index, which repeat across proposals and bundles. Do not join them by
time proximity alone. Use the full trace identity or transaction/nullifier
mapping. A broadcast-accepted log is CheckTx/mempool evidence, not inclusion.
Confirm which instrumentation exists in the deployed helper version; local source
code does not prove a particular run emitted those events.

For each interval include its evidence source and clock precision/offset. Report
queue/schedule wait, helper processing/proving, broadcast-to-inclusion, helper
visibility, client repoll/lock wait, and persistence separately only where the
boundaries are established. Otherwise retain an unresolved interval with the
missing event named. Cross-server timestamps require clock-offset evidence.
Client acceptance-response time can follow server enqueue/processing start, so
these independent observations are not a forced serial ordering.

Background tracking normally waits through a 10-second grace period and uses a
15-second ready-share cadence. Focused confirmation bypasses the grace period;
it retains the same four-request concurrency, ten-second quorum budget, and
configured-helper quorum. Do not attribute the prior 51 seconds to either policy
without identifying which path ran and observing its timeline. Do not change
polling, scheduling, quorum, or recovery rules to make a benchmark finish sooner.

The SDK change is verified by hermetic tests. Live performance and the cause of
the historical delay remain unverified until these captures are available.
