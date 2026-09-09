# stage-bench

One command that provisions a round on the staging vote chain with the ballot
you choose, drives a complete vote, and reports where the time went.

```bash
infisical run --projectId=40862c6d-a089-4355-b405-0477be0ee3b1 --env=staging -- \
  make stage-bench STAGE_BENCH_ARGS="run --proposals 37"
```

## Why this exists

`docs/helper_delivery_benchmark.md` describes a capture procedure: run a wallet
against staging, save the observability snapshots from three boundaries, then
compute occupancy and percentiles by hand. That procedure produced the numbers
behind the 32-slot helper-delivery admission work — helper delivery falling from
238.76 s to 18.67 s over 3 bundles × 37 proposals × 16 shares — and it is not
repeatable at any useful cost.

So no phase *below* helper delivery has ever been measured beside it. A
regression in ZKP2 proving, PIR, chain advance, or vote-tree sync would not show
up until someone repeated the whole manual capture. This crate is that
procedure as a command.

It measures; it asserts almost nothing. The one correctness claim it makes is
that the round it timed actually completed, because a run that delivered
nothing quickly is not a fast run.

## What it is not

Not a conformance suite. It injects no crash, no stall, and no helper outage —
`recovery-conformance` owns all three. This crate borrows that crate's staging
plumbing (credentials, published endpoints, round provisioning, the voter
wallet, the warm PIR template, the synthetic helper fleet) rather than
duplicating it, and adds a driver that reports.

The driver itself is deliberately *not* shared. `recovery-conformance`'s
`round_run.rs` is the control three live fault matrices are compared against;
threading a benchmark's needs through it would change the thing those matrices
are measured by. `src/drive.rs` is its sibling, and says so.

## Commands

```
stage-bench run       [ballot] [fleet] [pacing] [output]
stage-bench analyze   <run-dir>
stage-bench preflight [ballot]
```

`analyze` re-derives the report from a finished run directory without touching
the network, so an archived run can be re-read after the code that produced it
has moved on.

`preflight` runs every check `run` performs before it spends a round —
credentials, the voter wallet's identity, the coordinator key, the published
deployment, the ballot — and stops. Run it first on a machine that has not run
the benchmark before.

### `run` flags

| Flag | Default | What it changes |
| --- | --- | --- |
| `--proposals N` | 37 | Ballot width. 1 to 50, the SDK's own bound. |
| `--option-widths 2,3,4` | `2,3,4` | Cycled across the ballot; each 2 to 8. |
| `--ballot <path>` | — | Replay a vote manager's round export instead. Conflicts with the two above. |
| `--helpers N` | 1 | 1 is the real staging primary; 2 to 10 build a synthetic fleet routed onto it. |
| `--bundle-concurrency N` | 3 | Bundles advanced at once; the SDK's own default. Use 1 for a cold-PIR run. |
| `--proof-concurrency N` | 3 | Vote-commitment proofs built at once in a bundle; 1–15. |
| `--vote-window <s>` | 21600 | Seconds until the round's vote end. |
| `--tracking-budget <s>` | 1800 | Seconds the confirmation phase may run. |
| `--confirm-concurrency N` | 1 | 1 = shipped tracker. Above 1 = concurrent focused confirmation, an experiment. |
| `--budget <s>` | derived | Worker wall bound. |
| `--max-records N` | 262144 | Detailed records per reported invocation. |
| `--no-warm-pir` | off | Skip the cached proof template. |
| `--out <dir>` | `runs` | Where run directories are created. |

## Reading the report

Real output, from a three-proposal round on staging:

```
-- helper delivery --
  active shares    peak   32  avg    6.3  over     9.4s  (144 workflows)
  initial POSTs    peak   32  avg    6.3  over     9.4s  (144 attempts, 15.3/s)
  POST latency     p50   0.386s  p95   0.677s  p99   0.682s  max   0.691s
  recovery POSTs   0   post outcomes pending=144   acceptances succeeded=144

-- phases, by wall time --
  stage                                calls   wall(s)    sum(s)  peak   p50(s)
  helper::attempt                        911    400.36    179.32    32    0.156
  helper::tracking_run                     1    390.87    390.87     1  390.866
  helper::tracking_wait                   18    378.45    270.04     1   15.002
  round::run                               1     20.47     20.47     1   20.473
```

The round drive took twenty seconds; confirmation took six and a half minutes,
and 270 of those seconds were the tracker's own fifteen-second polling cadence.
That is the kind of thing this tool exists to make obvious.

Four things to read carefully.

**Active shares and initial POSTs are not the same number.** A share fanning out
to several helpers opens several requests, so a fleet of ten can hold far more
HTTP requests open than share workflows. The admission policy bounds the first;
the POST semaphore bounds the second. They coincide above only because that run
placed each share on one helper.

**Phases are ordered by wall span, not by accumulated time.** A stage with
thirty-two concurrent workers accumulates far more time than the window it ran
in — `helper::attempt` above accumulated 179 s inside 400 s. The bottleneck is
what held the clock. Both columns are printed, and parent and child durations
are never summed: a queue wait is already inside the delivery that follows it.

**Proposals and bundles are separate tables, because the SDK's work is.** A
combined delegate-and-cast batch is prepared and advanced once per *bundle*,
covering every proposal in it; proving and share delivery are per proposal. A
wide ballot pays the first three times and the second a hundred times, and one
table would make the cheaper axis look like the expensive one.

**An incomplete capture says so, loudly.** If any snapshot dropped records,
summary updates, or stage starts, every concurrency and percentile below it is a
floor rather than a measurement, and the table refuses to imply otherwise.
Re-run with a larger `--max-records`.

## Delivery is the number; confirmation is a tail

The two phases are not comparable and the tool keeps them apart.

**Delivery** — placing every share on its helpers — is what a wallet waits on,
and what PR #315's 238.76 s → 18.67 s measured. It is reported as the headline
and announced on stderr the moment it completes.

**Confirmation** — observing each share's reveal nullifier as confirmed on chain
— is background work a wallet spreads across the whole voting window. It is
inherently slow here for two reasons that are not defects:

- a share cannot confirm until its reveal transaction lands, so early passes
  return `pending` and the tracker sleeps `ready_poll_interval_seconds` (15 s)
  between passes; and
- the tracker's pass walks unconfirmed shares **one at a time**
  (`share_tracking/mod.rs`, `for loaded_share in pending_shares`). The four-way
  `SHARE_STATUS_MAX_CONCURRENT_POLLS` parallelises the quorum search *across
  helpers for one share*, not across shares, so a one-helper fleet polls at a
  strict concurrency of one.

A three-proposal round showed exactly that: `helper::share_status` n=767,
**peak=1**, and `helper::tracking_wait` 18 × 15 s = 270 s of the 391 s tail.

`--tracking-budget` bounds the tail. When it expires the run says so and reports
the confirmation figures as incomplete rather than pretending the round settled.

### Measuring what the serial walk costs

`--confirm-concurrency N` replaces the tracker with N concurrent
`confirm_pending_share` calls over distinct shares. This is legitimate for a
host — the invariants document is explicit that the per-share operation lock is
what keeps two callers off one share — and it runs *instead of* the tracker,
never beside it, because a round admits one run.

It is an **experiment, not a measurement of shipped behaviour**: focused
confirmation also bypasses the grace period and the 15-second cadence, so a
sweep is faster for two reasons at once. The manifest records the mode and the
printed table leads with a banner saying so.

Changing the tracker's own walk to be concurrent is a change to helper-share
behaviour, which `AGENTS.md` gates behind
[`docs/helper_submission_invariants.md`](../docs/helper_submission_invariants.md)
and its named regression tests. This crate does not make that change; it
produces the evidence such a change would need.

## What raising the widths does, and does not, do

Measured on two 37-proposal staging rounds, one bundle-serial and one at the
SDK's shipped widths:

| | 1 bundle / 1 proof | 3 bundles / 3 proofs |
| --- | --- | --- |
| drive | 63.6 s | 58.0 s |
| batch prep, accumulated | 14.2 s | 9.5 s |
| per ZKP2 proof, p50 | 0.117 s | 0.214 s |
| chain advance, accumulated | 20.4 s | 21.1 s |
| delivery work | 447.7 s | 523.0 s |
| delivery peak / effective | 32 / ~14.5 s | 32 / ~16.3 s |

Two things to take from it.

**Vote work is round-serialized by design, so `--bundle-concurrency` moves
less than it looks like it should.**
[`vote_work/round_lock.rs`](../zcash_voting/src/vote_work/round_lock.rs)'s
`bundle_scope` gives `Delegate` and `AdvanceDelegation` a per-bundle scope and
every vote step — `CastVote`, `AdvanceVote`, `AdvanceVoteBatch`, `SubmitShares`,
`ConfirmShare` — the round-wide one. The driver schedules with the same
function, so it will not admit two round-locked steps together. Three bundles
therefore still cast one at a time, which the timeline shows plainly: batch
prep, chain post, and chain advance all report peak concurrency 1 while
`advance_step` reports 3.

**Proof concurrency is sublinear because proving is CPU-bound.** At 3 wide each
proof took 0.214 s instead of 0.117 s; accumulated batch preparation still fell
from 14.2 s to 9.5 s, so it is a real but ~1.5x win, not 3x.

Delivery is not the lever in either run: 523 s of accumulated work landed in
about 16 s of bursts at the full 32 slots.

## The run directory

```
runs/20260909T182204Z-0123456789ab/
  manifest.json                     conditions: ballot, fleet, window, commit, profile
  run-config.json                   what the worker was told; holds no secret
  round.observability.json          the round driver's frozen snapshot
  tracking.0.observability.json     each background tracking invocation
  events.jsonl                      driver boundaries the SDK does not label
  helper-contacts.jsonl             every synthetic-fleet share POST, fsynced
  confirm.observability.json        per-share snapshots, when --confirm-concurrency > 1
  outcome.json                      the authoritative domain result
  metrics.json                      everything derived, in full
  sidecar.db                        the round's durable state
```

Snapshots are written before any error propagates, so a run that failed part way
still leaves the diagnostics that explain where it stopped — and the report is
built and printed over them before the worker's exit status is judged.

## What changes the numbers

Hold these fixed across runs you intend to compare; the manifest records each.

- **Build profile.** A debug ZKP2 proof takes minutes where a release proof
  takes seconds. `make stage-bench` always builds release.
- **The vote window.** The SDK's last-moment window is a fraction of the round,
  so two runs with different windows ran different share schedules.
- **The warm PIR template.** A cold run fetches every padded slot from the one
  synchronous staging PIR endpoint, which dominates its own phase and can fail
  outright. Present by default, and refreshed from every run.
- **Bundle concurrency.** Staging serves PIR from a single endpoint that stops
  answering under roughly fifteen concurrent queries. The default of one is not
  timidity; raising it is an experiment about that endpoint.
- **The fleet.** More helpers means more placements per share, which is a bigger
  workload rather than the same workload measured differently.

## Prerequisites

- `svoted` on `PATH` (round creation shells out to it).
- A scanned voter wallet at `~/.cache/recovery-conformance/voter.db`. Build one
  with `cargo run -p recovery-conformance --example sync_voter`. Shared with the
  conformance suite deliberately: the same fixed seed funds both and scanning it
  takes hours.
- `VOTE_SDK_VOTER_TEST` and `VOTE_MANAGER_VOTE_SDK` in the environment, read
  live from Infisical on every run. Nothing secret is written to the run
  directory or passed on a command line.

Round creation serialises chain-wide, and a delegation is consumed per round:
never run two benchmarks at once, and expect every run to spend a fresh round.
