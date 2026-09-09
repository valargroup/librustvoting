# recovery-conformance

Staging crash-recovery conformance for `zcash_voting`.

## Why this exists

The crate's ~1400 unit tests prove that durable rows are written in the right
order. They cannot prove the claim those rows exist to support: that an app
**killed** mid-round — no unwinding, no `Drop`, no flush, no graceful SQLite
close — restarts against the same sidecar and the same live chain and
converges, without spending a note twice or losing a vote.

Every unit test ends in a clean `drop(db)`, and a clean drop is the one thing a
crash is not. `docs/chain_submission_invariants.md` lists "a process crash while
a `Submitting` reservation exists" as possibly-dispatched and specifies
`abandoned Submitting on restart -> Recovering`; nothing killed a process to
check.

This package does. It provisions a real multi-proposal, multi-bundle round on
staging, drives it in a child process, injects one fault, then reopens the same
sidecar and asks the only question that matters: **does the round still know
what it owes?**

The oracle is `session::resume_plan`, a pure function of durable state. Nothing
in memory survives the fault, so the plan after reopen *is* the complete
definition of the remaining work.

### Three faults, three matrices

| Axis | Fault | The question only it can ask |
| --- | --- | --- |
| **Crash** (`staging_conformance`) | `abort()` at a named durable boundary | Does a round killed here still know what it owes? |
| **Hang** (`stall_conformance`) | One class of network request never answers | Does the request end *at all*, or does it wedge the round? |
| **Fleet** (`helper_fleet_conformance`) | Ten helpers whose reachability changes | Does a partial placement get completed, exactly once, on the helpers that are still owed? |

The second and third exist because the first cannot reach them.

A crash is an abrupt, observable fault: the process dies and leaves durable
evidence. A **hang** is its opposite — nothing dies, nothing unwinds, and the
only thing standing between the wallet and a wedged round is a deadline it
imposed on itself before making the request. Nothing here ever checked that any
such deadline is actually applied, and one of them, PIR, has no SDK-side bound
at all when a host supplies its own transport.

The **fleet** axis exists because a suite driven against a single helper URL
cannot see the placement layer at all. With one configured helper the target
count is 1, the per-helper quota is the whole commitment, and the minimum
planning pool is 1: every rule about splitting a vote's shares across a fleet,
repairing a partial deficit, resuming against a plan whose targets are now
unreachable, and never re-POSTing to a helper that already accepted is
unreachable code. Ten helpers make each of those a statement a run can be wrong
about.

## Running it

Not part of `make test` and not in CI: it needs the network and it kills
processes.

```bash
infisical run --env=staging -- make recovery-conformance   # all three matrices
make recovery-conformance-check                            # type-check and lint
```

**That default now costs hours.** Three matrices provision roughly thirty-five
rounds on `svote-1` between them, and the fleet matrix places five times the
helper traffic of a one-helper round. A change that can only affect one axis
should pay for one axis:

```bash
infisical run --env=staging -- make recovery-conformance-crash
infisical run --env=staging -- make recovery-conformance-stalls
infisical run --env=staging -- make recovery-conformance-fleet
```

The hermetic tests run under every one of these, and under
`RECOVERY_CONFORMANCE_WITHOUT_STAGING=1 cargo nextest run -p
recovery-conformance` with no staging access at all. They cover the taxonomies,
the placement arithmetic, the orchestration logic, and — importantly — the two
fault wrappers themselves (`tests/fault_routes.rs`). Those wrappers are the
load-bearing mechanism of both new axes: one that quietly delegated its armed
class, or quietly let a synthetic helper URL reach the network, would leave both
live matrices running, reporting green, and proving nothing.

To re-run only the stages a change could affect:

```bash
infisical run --env=staging -- env \
  RECOVERY_CONFORMANCE_STAGES=after-vote-broadcast,after-vote-confirmed \
  make recovery-conformance
```

The other two axes select the same way:

```bash
RECOVERY_CONFORMANCE_STALLS=share-post,pir-query   # hang matrix
RECOVERY_CONFORMANCE_FLEET=half-then-other-half    # fleet matrix
```

Each matrix builds its own control run, unconditionally, because every terminal
comparison is against it — and the fleet matrix's control is built against *its
own* ten-helper fleet, since comparing a ten-helper round to a one-helper
control would compare two different rounds and call the difference a finding.

An unrecognized name fails the run rather than selecting nothing, and a matrix
that attempts nothing, or passes none of what it attempts, fails as well: a
green run over untested ground is the way a suite like this rots.

### Credentials

No secret lives in this repository, and none is written to disk. Two values are
read from the environment at run time:

| | |
| --- | --- |
| `VOTE_MANAGER_VOTE_SDK` | Scoped coordinator key. Authorizes `MsgCreateVotingSession`, which the chain restricts to the vote manager. **Not** an attestation key — the suite self-signs the dynamic config it trusts. |
| `VOTE_SDK_VOTER_TEST` | The fixed voter's 24-word BIP39 mnemonic. |

Read them live from Infisical on every run. Do not point the suite at a local
snapshot of exported secrets: such a file goes stale the moment a key is added
or rotated, and a stale value fails as a rejected authorization, which reads
like a permissions problem rather than a wrong key.

To check a key is present without printing it, test the **value**, not the exit
code — `infisical secrets get` returns 0 for a key that does not exist and
substitutes the literal `*not found*`:

```bash
infisical secrets get VOTE_MANAGER_VOTE_SDK --env=staging -o json \
  | grep -q '"secretValue":"\*not found\*"' && echo absent || echo present
```

## Crash stages

Each stage sits immediately next to a durable commit. `touches_chain()`
distinguishes whether the armed run itself may have submitted anything. Every
matrix case still gets its own round, because the post-crash resume drives that
round to quiescence and therefore eventually submits even after a pre-POST
crash.

| Stage | Durable state it leaves |
| --- | --- |
| `before-delegation` | bundles only |
| `after-note-selection` | bundles only; notes chosen, nothing written yet |
| `after-pczt` | `bundles.pczt_sighash` + TX1 effects, write-once |
| `after-proof` | `proofs` row |
| `after-signing` | proof + any Keystone signature |
| `before-broadcast` | `chain_submissions` `submitting`, bytes never sent |
| `after-broadcast-unread` | `submitting`, transaction may be on chain, no hash |
| `after-broadcast-read` | as above; the real response is captured for the parent |
| `after-tracking` | `tracking` + candidate hash — needs a one-pass chain policy, see below |
| `before-cast` | delegation confirmed |
| `after-tree-sync` | delegation confirmed; cached tree synced |
| `after-vote-proof` | nothing new — the proof is lost by design |
| `after-vote-commit` | `votes.commitment_bundle_json`, no POST reserved; the seam is the fleet preflight |
| `after-helper-plans` | `helper_share_plans` + `round_immediate_share` |
| `before-vote-broadcast` | vote `submitting`, bytes never sent |
| `after-vote-broadcast` | vote `submitting`, POST dispatched |
| `after-vote-confirmed` | `confirmed` + `votes.vc_tree_position` |
| `before-share-post` | `share_delegations.attempting_urls` |
| `after-share-post` | `attempting_urls`; helper answered, outcome unwritten |
| `after-share-accepted` | `sent_to_urls` |

### The two sharp cases

- **`before-broadcast`** — nothing was sent, yet the abandoned reservation must
  normalize to `Recovering`, **not** disappear: a restarted process cannot
  prove the bytes never left. Conservative by design, and the single most
  valuable test here.
- **`after-broadcast-unread`** — the transaction is on staging and the wallet
  has no hash for it. Resume must reach `Confirmed` by exact-tree scanning and
  must never POST a second transaction spending the same notes.

## Hung requests

One target per run, each naming a class of request the SDK makes and the
deadline it is supposed to apply to it. The stall is injected below `RouteHttp`,
which is the lowest HTTP seam the SDK exposes and sits *under* every one of
those deadlines — so a hang injected there is a hang the SDK is supposed to end,
and a run that never ends is the finding.

| Target | Request | Bound it must keep |
| --- | --- | --- |
| `lightwalletd` | every lightwalletd RPC | 20s unary, 10s connect |
| `pir-query` | any PIR request | 60s request budget |
| `delegation-post` | `POST .../delegate-vote` | 150s |
| `vote-post` | `POST .../cast-vote` | 150s |
| `transaction-lookup` | `GET .../tx/{hash}` | 10s |
| `commitment-tree-read` | `GET .../commitment-tree/...` | 60s |
| `helper-preflight` | `GET .../status` | 30s hard |
| `share-post` | `POST .../shares` | 30s POST, 60s fan-out |
| `share-status` | `GET .../share-status/...` | 10s per share |

Two of these deserve their names read carefully.

- **`pir-query` is the asymmetric one.** `pir_client::Transport` takes no
  timeout argument at all, so a host that supplies its own PIR transport has no
  SDK-side bound whatsoever; the budget this target exercises lives only inside
  `HyperTransport`. It is the class most worth watching for regression.
- **`commitment-tree-read` is one class serving two callers**, and deliberately
  not split. Chain recovery's exact-tree scan and the vote-commitment-tree sync
  issue the same request to the same path on the same host, and nothing in the
  request distinguishes them. Naming two targets no wrapper could tell apart
  would be a taxonomy that lies.

`lightwalletd` is currently **skipped, by name and with its reason printed**.
`lwd.rs` dials tonic directly rather than through an injected transport, so no
route wrapper reaches it; covering it needs a black-hole listener the parent
holds open. It is skipped rather than quietly absent, because a target missing
from a run is how a matrix rots.

Where the hang lands inside the request matters as much as which request it is,
and mirrors the crash matrix's own `BroadcastPoint`: a hang **before** the
dispatch hook must be classified definitely unsent, and one **after** it must be
treated as possibly delivered. The two POSTs that carry a transaction are
therefore stalled after dispatch — the ambiguous half, and the one with a safety
claim attached — while the reads are stalled before it, which also exercises
connection setup.

## Helper fleet scenarios

Ten synthetic helpers, because ten is where the arithmetic becomes interesting:
the target count is 5, the per-helper initial quota is 12 of 16 shares, and a
complete batch needs a planning pool of 7. Ten also sits exactly on
`SHARE_HELPER_TARGET_COUNT_CAP`, so the cap is pinned by a live fleet rather
than only by a unit test. `tests/helper_fleet_plan.rs` asserts all three numbers,
so a scenario cannot keep passing while quietly testing something else.

Only the staging primary answers `/shielded-vote/v1/shares` — the secondary and
the PIR host return 404 — so the fleet is ten names under the reserved
`.invalid` TLD, and reachability is decided in the route rather than by the
network. A helper that **answers** has its request rewritten onto the real
primary, so the POST, the acceptance and the response are genuine; one that
**refuses** or **never answers** never leaves the process. The wallet's journal
records the synthetic URL either way, and that is the identity
`attempting_urls`, `sent_to_urls` and the persisted planning fleet are all
written in terms of.

| Scenario | First run | Resumed run | What only it can prove |
| --- | --- | --- | --- |
| `full-fleet-then-crash` | all ten answer; killed at `after-share-accepted` | all ten answer | A restarted round sends only to the helpers still owed |
| `half-then-other-half` | four answer, six refuse; **killed at the first share outcome** | those four refuse, the six answer | Every remaining share is placed on a helper never tried, while the unreachable ones' acceptances survive untouched |
| `silent-helpers` | four answer, two go silent, four refuse; **killed at the first share outcome** | all ten answer | A silence is journaled as outcome-unknown rather than written off or replayed |
| `whole-fleet-down` | all ten refuse | all ten answer | A round that could not deliver does not record that it did, and still owes the whole placement |
| `fleet-contracts-then-grows` | all ten answer | configured with six | The effective target clamps to the live fleet, dropped helpers are never contacted, and the persisted plan is not redrawn |

Three details in that table are load-bearing:

- **The first half is four, not five.** Five is exactly the target count, so a
  fleet with half up could meet every share's target on its own and leave
  nothing for the second half to repair — the scenario would pass having
  exercised no deficit at all. Four is strictly below the target, so the
  deficit is guaranteed and can only be filled by helpers the first run never
  tried.
- **`silent-helpers` uses only two silent helpers.** A silent helper is bounded
  only by the per-share fan-out budget, so a fleet of them costs that budget on
  every one of the round's 144 shares. Two demonstrate the rule without turning
  one scenario into the longest run in the suite.
- **Contraction is not an outage.** A refused helper is still configured; a
  contracted fleet has genuinely fewer. The SDK treats these differently and so
  do the assertions — which is why `fleet-contracts-then-grows` does not assert
  the original target.
- **Every scenario but `whole-fleet-down` has to be cut short, and that is not
  incidental.** A share **confirms at whatever placement it reaches**, so a
  first run that can deliver at all finishes the round, the resume reports
  `NothingToTrack`, and the flip never happens. The first live run of
  `half-then-other-half` passed exactly that way — all 144 shares confirmed on
  the first half, the second half never contacted, every assertion holding
  trivially against a completed round. The crash is what leaves work owed.
  `whole-fleet-down` is the exception because nothing is reachable there, so the
  outstanding work is guaranteed by the fleet itself.
- **The crash lands after an acceptance, not before one.** `after-share-post`
  fires at the first POST, before anything has been taken, and a first half
  holding nothing makes "those acceptances survive the flip" a claim about an
  empty set. That was observed live too: 730 placements on the second half and
  zero on the first. `after-share-accepted` leaves some behind.
- **A vacuous run now fails rather than passes.** Before any of the placement
  assertions run, the matrix requires the first run to have left shares
  unconfirmed **and** to have recorded at least one acceptance, and reports
  both. That gate is the reason this matrix can be believed; without it, the
  scenario above was green.
- **`full-fleet-then-crash` crashes early, and its reach is correspondingly
  narrow.** `after-share-accepted` fires on the first `ShareOutcome`, so the
  killed process has placed a handful of shares rather than most of them, and
  the no-premature-re-send rule is checked against those. It is a real check on
  real placements, not a broad one; the fleet-flip scenarios are what exercise
  the deficit at scale.

### What the contact record can and cannot see

Durable state answers where a share *ended up*. It cannot answer where a run
declined to send one again, and that is the whole subject of the deficit rules —
so the fleet assertions read two further sources: the route's own fsynced record
of every share POST, and `ShareTrackingRunReport`, which names the distinct
helpers a tracking run reached for each share. They are read together rather
than one instead of the other; a disagreement between what the route saw and
what the SDK believes it did would itself be worth knowing about.

One limit is worth stating rather than discovering. `run_to_quiescence` retries
a run the environment interrupted, and each attempt truncates its own log, so
the route's record covers the **last** attempt only. That can only lose
evidence, never invent it: every assertion here is of the form "nothing in the
contact record may be X", so a missing record weakens the check rather than
failing it wrongly. The SDK's own report is accumulated across the run and does
not have this gap, which is why both are read.

### What a fleet of ten costs

At a target of 1 a round places 16 shares x 9 votes = **144** helper POSTs. At a
target of 5 the same round places **720**, capped at 16 in flight, all landing
on the one staging primary. The fleet matrix therefore carries its own dispatch
ceiling of `144 * 5 * 10` rather than inheriting the crash matrix's: that
constant is sized from the work a resume can actually owe, and a resume here
owes five times as much. Inheriting the smaller number would turn ordinary
convergence into a livelock report.

## Invariants

Split by whether the suite actually asserts them today, and by which axis does
the asserting. Everything in the last table is a property the SDK is designed to
hold that this suite does **not** yet check — listed so the gap is visible, not
as a claim.

### Asserted by the crash matrix

| | Assertion | Where |
| --- | --- | --- |
| **A1** | Two `resume_plan` calls over the same durable state return identical plans | `deterministic_plan` |
| **A2** | A resumed round reaches terminal quiescence, never `Failures` or `PassBudgetExhausted` | matrix, per stage |
| **A3** | Terminal submission states match the uncrashed control | matrix, per stage |
| **A4** | A second resume plans no further *foreground* work. `ConfirmShare` steps are excluded by design: a round ending in `BackgroundShareWorkOnly` has finished what the foreground owns, and the host's timer closes the rest | `assert_idempotent` |
| **B1** (part) | After a `before-broadcast` crash the abandoned reservation **still exists**; a row that vanished would let the next pass reserve a fresh first attempt and build a second transaction | `assert_stage_state` |
| **B2** | The crashed bundle is *advanced*, never re-delegated — a second delegation would spend its notes twice | `assert_stage_state` |
| **B4** | Terminal rows (`confirmed`/`rejected`/`submitted_without_hash`) are byte-identical across resume | `assert_terminal_rows_unchanged` |
| **B5** | `committed_post_reservations` never decreases | `assert_reservations_monotonic` |
| **C5** | A vote submission never exists without a durable helper plan | `assert_plans_precede_broadcast` |
| **E1** | Bundles other than the crashed one keep their pending work | `assert_other_bundles_untouched` |
| **B3** (part) | A hashless dispatch confirms by one of the two routes open to it, tree scan or same-generation retry, and the route is reported every run. Which one wins is the SDK's choice and not assertable; that no *second* generation was built is what carries the safety claim | `assert_confirmed_by_a_legal_route` |
| **B2/B3** (identity) | Where the stage captured the dispatched response, the transaction the round finally confirms is **the same one** the killed process had already sent — not merely "exactly one eventually confirmed" | `assert_recovered_the_same_transaction` |
| **D2** (part) | `after-share-accepted` records a definite acceptance in `sent_to_urls` | `assert_stage_state` |
| — | Each stage leaves the durable state its row expects (PCZT persisted, proof persisted, vote committed, share journaled) | `assert_stage_state` |

### Asserted by the hang matrix

Every one of these is new. Before this axis existed, nothing in this repository
established that *any* request the wallet makes carries a deadline it keeps.

| | Assertion | Where |
| --- | --- | --- |
| **H1** | **A hung request is ended by the SDK, not by the suite's patience.** The armed run is given a budget derived from the class's own declared bound, far below the 20-minute child budget; a run still hanging at that point is reported as an unbounded request rather than as a slow stage. This is the claim the axis exists for, and a wedged round is not repaired by restarting, because nothing crashed | `assert_the_request_was_bounded`, and `StalledRun::ended_itself` |
| **H2** | **The stall fired, on the class it named, at the point it named.** A run whose stall never fired would satisfy every assertion about "the state a hang left" while proving nothing, because the state inspected would simply be a healthy round — the same rule the crash matrix applies to a stage that stops firing | `assert_the_stall_fired` |
| **H3** | **A hang after dispatch on a transaction-carrying POST leaves its submission row intact.** A restarted process cannot prove the bytes never left, so a row that vanished would let the next pass build a second transaction spending the same notes. The same claim `B1` makes about a crash before broadcast | `assert_a_stalled_submission_survived` |
| **H4** | Once the endpoint answers again the round converges to the uncrashed control, with reservations monotonic and **no second generation** built | `assert_matches_control`, `assert_reservations_monotonic`, `assert_no_second_generation` |
| **H5** | The plan over a stalled round is deterministic, and a settled round plans no further foreground work | `deterministic_plan`, `assert_idempotent` |

What **H3 deliberately does not assert** is the exact state the row settles in.
Normalization is lazy — it happens inside the lifecycle's next admission rather
than at open — so reading a particular state name here would be asserting the
SDK's scheduling rather than its safety. That is the same gap `B1` (rest)
records below.

### Asserted by the fleet matrix

These close `D4` and `D5`, and generalize `D1` and `D2` from "some helper" to
"every helper" — a distinction that does not exist when only one helper is
configured.

| | Assertion | Where |
| --- | --- | --- |
| **D1** (general) | **Every helper that went silent stays journaled.** The reservation is what makes an interrupted attempt recoverable rather than invisible: a helper that accepted the connection and never answered is, from the sidecar, indistinguishable from one never contacted, so the record has to survive. Refusals are deliberately **excluded** — see below. Checked at fleet level: the contact record names the helper but not which share was in flight, and inventing that link would assert more than the evidence supports | `assert_every_unanswered_helper_was_journalled` |
| **D2** (general) | **A definite acceptance is never downgraded, per share, across a restart *or* a fleet flip.** The case it exists for is a helper that accepted and then became unreachable: treating "cannot reach it now" as "never had it" would lose the placement and licence a re-send | `assert_acceptances_never_downgraded` |
| **D3** (part) | A helper that went silent leaves an unresolved attempt journaled, rather than being written off. An outcome nobody can learn must be recorded as unknown | `silent-helpers`, in the matrix |
| **D4** | **Every share ends the round durably confirmed**, however the fleet came and went. This is the completion a round owes; the *placement target* is not owed at the end and is reported rather than asserted — see the finding below | `assert_every_share_is_confirmed` |
| **D5** | **A resume fills only the deficit.** No share is re-POSTed to a helper that already accepted it while any helper it has never been tried with remains. Judged per share, never fleet-wide: a run legitimately POSTs different shares to the same helper, so a flat set of contacted URLs would report a correct delivery as a forbidden re-send | `assert_no_premature_resend_to_an_accepted_helper` |
| **D6** | **No share is POSTed to a helper outside the configured fleet.** The confidentiality claim rests on it, and no durable row would show a breach: the journal records where the wallet *believes* it sent | `assert_no_contact_outside_the_fleet` |
| **D7** | Journaled placement never names a helper outside the configured fleet, so a mis-wired scenario fails as configuration rather than as a mysterious result forty minutes later | `assert_placement_stays_within_the_fleet` |
| **A2/A3/A4, B5** | A round whose fleet changed under it still converges to the control, with reservations monotonic and nothing left to plan | as in the crash matrix |

### A share can confirm while held by fewer helpers than the target

The first live run of this matrix asserted that every share reaches
`target_count` definite acceptances. It does not, and the way it failed is worth
recording rather than tuning away.

With four helpers answering against a target of five, shares confirmed holding
**two** acceptances. The resumed run then reported `NothingToTrack` after one
pass and contacted nobody — correctly, by its own rules: background tracking
walks *unconfirmed* shares, and these were confirmed. So the shortfall is never
repaired, because nothing is looking for it.

Read carefully, that is consistent with the specification. `target_count`
governs **initial placement**; confirmation is **completion**; and
`target_count` is explicitly "a target for definite acceptances, not a cap on
possession". Nothing says a confirmed share must have met its target.

What it means in practice is that a round can finish with shares held by fewer
helpers than the redundancy policy aimed for, and neither the wallet nor this
suite would notice. Whether that is intended is a question for the protocol
rather than something a conformance suite should decide by asserting one way or
the other — so the suite **reports the placement spread every run** and asserts
only what is genuinely owed. If the answer is that the target should be repaired
after confirmation, this is the test that would prove it, and the assertion is
one line.

`D1` excludes refusals, having once included them. The first live run of this
matrix reported six violations, and every one was this suite's fault rather than
the SDK's: a refused connection is a *definite pre-dispatch* failure, so the
wallet knows no request byte left, clears the reservation, and is right to —
there is nothing to recover, and a retained row would make it poll a helper that
provably never received the share. The rule only ever applied to attempts whose
outcome is **unknowable**. That correction is what the first run bought.

`D5` is stated with its condition because the unconditional claim is **false**,
and knowing why is the point. Recovery walks untried helpers first, then
interrupted ones, and only once a share is overdue does it extend to ambiguous
and then to previously accepted helpers — a deliberate, duplicate-safe last
resort. What must never happen is reaching for that last resort while untried
helpers remain, because that spends a re-send on a helper that already has the
share and leaves the deficit unfilled. A one-URL suite could not even express
this: with a single configured helper, "the helper that accepted" and "the only
helper there is" are the same server.

### Not asserted yet

These need either an exercise that reliably reaches them or an assertion that
does not exist. **Do not read them as tested.**

| | Why not yet |
| --- | --- |
| **A5** | No assertion that a helper share is never sent with the `0` tree-position placeholder. |
| **B1** (rest) | That the row becomes exactly `Recovering` is **not** asserted: normalization is lazy, happening inside the lifecycle's next admission rather than at open, so it needs an assertion that drives one admission and reads the row before the round advances past it. The safety-critical half — the row survives at all — *is* asserted, for a crash (`B1`) and now for a hang (`H3`). |
| **B6** | Generation-identity immutability is trigger-enforced but not checked here. |
| **C1, C2, C4, C6, C7** | Roster changes, per-proposal isolation, the ballot gate, tree-cache consistency, and generation binding have no assertion. |
| **D3** (rest) | Only the journalling half is asserted. That an ambiguity is later *erased* — promoted to a definite outcome by a duplicate-safe retry rather than carried forever — is still unchecked. |
| **E2, E3, E4** | Proposal-primary ordering, round-lock leakage, and run-relative tally are unasserted. |
| **`lightwalletd`** | The one hang target not reached through the shared route. `lwd.rs` dials tonic directly, so covering it needs a black-hole listener the parent holds open. Skipped by name and with its reason printed, never silently absent. |
| Fleet **disagreement** | Every answering helper shares one backend, so the fleet has ten identities and one opinion. A scenario where two helpers disagree about a share is not expressible here. |
| Contracted-fleet **target** | `fleet-contracts-then-grows` reports the placement a clamped target settles on rather than asserting a number. Encoding one before observing it would be a guess wearing an assertion's clothes. |
| **Placement target at completion** | A confirmed share is not required to have reached `target_count` helpers, and nothing repairs it if it did not — observed live at two acceptances against a target of five. The spread is reported every run; see the section above. This is the most substantive open question the fleet axis has surfaced. |

### The sharp submission cases

`before-broadcast` and `after-broadcast-unread` are where a bug costs a voter a
note. Three things are checked, in increasing strength:

1. the crashed bundle is planned for *advancement*, never re-delegation;
2. reservation counts are reported per stage and asserted monotonic — every
   committed POST increments one, so the count is how many times the wallet
   committed to sending;
3. where the stage captured the response, the confirmed transaction hash must
   equal the dispatched one. A round that quietly sent a replacement would
   confirm a *different* hash while looking equally healthy, which counting
   alone cannot detect.

A stalled recovery is not a verdict: the specification separates
`ChainRecoveryStalled` from `ChainTerminal` because running again later may
resolve it, so the matrix waits and re-drives rather than failing.

Neither is a stale vote-tree cache. A crash can leave the cached
vote-commitment tree disagreeing with a delegation that confirmed; the tree sync
detects that, **discards the cache**, and fails the pass so the next one
re-syncs from scratch. That is the SDK repairing itself, so the matrix
re-drives. The rule is deliberately narrow — matched on that one condition —
because a broader one would retry past real findings.

### Reaching the tracking window

`ChainOutcome` is reported once per step, at the end, and carries the
episode's *terminal* outcome — not one event per poll. Under the shipped
45-pass policy an episode polls until the submission confirms, so a stage
waiting to observe one *still tracking* can never fire. The run therefore arms a
single-pass chain policy for `after-tracking` alone; every other stage keeps the
shipped cadence, so the control it is compared against is unaffected.

### Reaching after-vote-commit

`after-vote-commit` names a narrow boundary — a committed vote whose helper
plans are not yet durable — and the progress stream does not mark it: casting
goes from `VoteCommit(Signing)` straight to `HelperPlansPrepared`, which is
already the *next* stage's boundary.

The seam is not in the event stream but in the work. Vote completion probes the
helper fleet between those two commits, and that probe is a real network round
trip through the transport this suite already wraps, so a crash on it lands
squarely in the window with no production change.

The stage was documented as unreachable and skipped in every run before this
was noticed. Nothing is excused from firing now: a stage that does not crash
where it claims to fails the matrix.

## What this suite cannot cover

- **Which route resolves a hashless dispatch.** A crash between dispatch and
  response leaves no candidate hash, and recovery may resolve it either by
  scanning the tree or by re-POSTing the same generation and being handed the
  hash back. Both are specified. Requiring the tree route appeared to work for a
  long time only because the crash boundary was wrong: aborting after the whole
  response had been read let the chain include the transaction first, so the
  tree won the race. At the real boundary the retry usually wins, and waiting
  for block inclusion before resuming does not change it. The route is printed
  every run so a change is visible; the safety claim rests on no second
  generation being built, which is asserted.

- **Atomic vote batches.** `ATOMIC_VOTE_BATCHES_ENABLED = false`
  (`zcash_voting/src/lib.rs`) while no deployed chain serves `cast-vote-batch`,
  so a fresh staging round only ever produces singleton casts. Batch
  classification and recovery stay covered by unit tests. When the route ships,
  the vote stages gain a batch variant and the atomicity invariant becomes
  testable here; nothing else changes.
- **The mid-ZKP-2 proof.** Nothing is durable between `prepare_vote_work` and
  `persist_prepared_vote_work`, so `after-vote-proof` costs minutes of
  re-proving. The suite asserts that this is *only* a cost — no durable damage,
  no orphaned lock, no partial tree.
- **Rewinding staging.** A delegation consumed on the vote chain stays
  consumed, which is why every mutative stage needs its own round. See
  [Round consumption](#round-consumption-and-what-that-costs).
- **Helpers that disagree.** Every answering synthetic helper is routed to the
  same staging primary, so the fleet has ten identities and one opinion. A
  scenario where one helper accepts a share and another rejects the same share
  is not expressible here, and neither is a helper that returns a *wrong*
  answer. What the fleet models is reachability and placement, not byzantine
  behaviour.
- **Lightwalletd hangs.** The one request class not reached through the shared
  route: `lwd.rs` dials tonic directly rather than through an injected
  transport. The target exists and is skipped by name with its reason printed,
  so the gap is visible in every run rather than absent from the taxonomy.
- **Whether a bound is the *right* bound.** The hang matrix asserts that a
  request ends, not that it ends promptly. A stalled run still drives a whole
  round — three delegations and nine votes, each with a Halo2 proof — bracketed
  by two share-tracking phases bounded at eight minutes each, so sixteen minutes
  of a helper-path run has nothing to do with how long the armed request took.
  The budget is floored at thirty minutes for that reason, and the cost is a
  weak upper bound: a deadline that is applied but far too long would pass here.
  The floor is not negotiable in the other direction — without it,
  `share-status`, whose bound is ten seconds, would be reported as an unbounded
  request on every run, for time its round spent elsewhere. That false finding
  would discredit the one claim this axis exists to make.

## Environment

| | |
| --- | --- |
| Zcash | **testnet**, via `https://testnet.zec.rocks:443` |
| Vote chain | `svote-1` (staging), RPC `https://stage.vote-rpc-primary.valargroup.org` |
| Coordinator | `sv1z4rawnk8ny0pzsewyzm3egdd7296fr8p20fkf8`, derived at `m/44'/133'/0'/0/0` |

Two things here are easy to get wrong, and both fail in ways that look like
recovery bugs rather than configuration:

- **Stardust is mainnet-only.** Every Stardust host reports `chainName:
  "main"`, and no testnet Stardust exists. Pointed at one, the voter wallet
  finds no notes and the run reports "no eligible notes".
- **Read the published config, never a local checkout.** The
  `token-holder-voting-config` working copy can be stale; its `stage/pir.json`
  named a snapshot height that was a plausible *mainnet* height while the
  published one was unambiguously testnet.

`VOTE_SDK_VOTER_TEST` holds **11 notes, which bundle into 3**. That shape is
deliberate and worth not breaking.

Bundling packs notes value-descending, five to a bundle
(`BUNDLE_NOTE_SLOTS = 5`), so 11 notes fill 5/5/1. The privacy trim then tries
to shed the smallest bundle down to `DEFAULT_MAX_PRIVACY_BUNDLES = 2`, but it
may only spend `DEFAULT_PRIVACY_DROP_BPS` — 1% of selected value. With
near-equal notes the last bundle is about 9% of the balance, far over budget,
so the trim breaks immediately and all three survive. The lone note in bundle 3
must still be worth at least `BALLOT_DIVISOR` (0.125 TAZ) by itself or step 4
drops it as sub-ballot.

Three bundles rather than two is what makes the multi-bundle invariants real:
`E1` crashes one bundle mid-proof and asserts the others are untouched, which
needs a bundle to spare. The cost is one extra delegation proof and one extra
vote proof per proposal.

Because bundling is value-sensitive, rebalancing this wallet can silently
change the bundle count and quietly weaken `E1`. The suite asserts the layout
it expects at provisioning time rather than trusting it.

A wallet learns a round's parameters from the signed dynamic config: it fetches
the document, verifies a `RoundAuthPayloadV2` signature over the round id,
`ea_pk`, and PIR layout, and only then trusts the values. **This suite skips
all of that** and reads the round straight off the chain that created it
(`provisioning::fetch_round`).

That trade is sound here and nowhere else. Config authentication answers "is
this round genuine and endorsed" — a question about trusting a third party's
document. The suite provisions the round itself, minutes earlier, with its own
coordinator key, so it already knows the answer; signing a document to verify a
fact it just created would exercise the config layer rather than recovery.

What is *not* skipped is agreement: the parameters used to drive the round come
from the chain's own record, so a provisioning mistake surfaces as a mismatch
instead of being carried forward by a local copy.

## Round consumption, and what that costs

A delegation is consumed **on the vote chain**: once a bundle's delegation is
registered for a round, that round's gov nullifier is spent and the bundle
cannot delegate again. The Zcash notes themselves are untouched — TX1 is a
PCZT-only signing artifact and is never broadcast — so the voting wallet never
needs re-funding. It is the *round* that is one-shot, not the money.

Two consequences shape the suite:

1. **A stage that gets a POST onto the wire consumes its round.** Those stages
   need a freshly provisioned round. `touches_chain()` names whether the armed
   run can do so before crashing.
2. **Driving a resumed round to quiescence is itself mutative**, even when the
   crash was pre-broadcast. A `before-broadcast` crash leaves a `Recovering`
   row that resume will dispatch, which consumes the round just the same.

Therefore every matrix case provisions a distinct round. Sharing a round among
pre-POST crash stages would still let the first case's resume consume that
round, making every later case observe chain effects that its sidecar does not
own. There is no mock prover — the `test-fixtures` seeding helpers skip
proving, which is exactly what this suite must not do — so this isolation is
expensive but required for the terminal convergence oracle.

## Verification record

### Full three-axis runs

Every matrix, end to end, against staging. A pass is 36 provisioned rounds:
21 crash stages, 5 fleet scenarios, 8 hang targets, and a control for each
matrix.

| Pass | Crash | Fleet | Stall | Result | Duration |
| --- | --- | --- | --- | --- | --- |
| 1 | 21/21 | 6/6 | 7 pass, 1 fail, 1 skip | **failed** — `commitment-tree-read` never fired | 11431s |
| 2 | 21/21 | 6/6 | 9/9 | **105/105 passed** | 11809s |
| 3 | 21/21 | 6/6 | 9/9 | **105/105 passed** | 12331s |
| 4 | 18 pass, 2 skip | 6/6 | 9/9 | **failed** — two rounds could not be provisioned | 12917s |

Pass 1's single failure was the anti-rot guard earning its place: the target
reported that no request of its class was ever made, and the cause was that
vote-tree reads were escaping both fault wrappers through a transport the suite
had never injected. The round completed perfectly throughout — nothing else
would have reported it.

Pass 4 failed for a reason that says nothing about recovery, and that is
exactly why it failed. Provisioning a round posts a session to the chain, waits
for its ceremony, and resolves an anchor from lightwalletd; one attempt hit a
transaction that "never reported a round id" and another could not open a
lightwalletd channel. The matrix refused to skip past them — a skipped stage
proves nothing — so a momentary staging hiccup discarded a three-and-a-half-hour
run. Provisioning is setup rather than the thing under test, so it now retries
three times before giving up. The strictness stays; only the fragility goes.

**The stall counts are identical across passes 2 and 3**, target for target:

| target | stalls fired |
| --- | --- |
| `commitment-tree-read`, `delegation-post`, `pir-query`, `transaction-lookup`, `vote-post` | 6 each |
| `helper-preflight` | 9 |
| `share-status` | 93 |
| `share-post` | 112 |

That reproducibility is worth more than the pass line. A target that quietly
stopped hanging would show up here as a count change rather than hiding behind
a green result, which is exactly how pass 1's defect was caught.

### The fault axes, first live runs

| Exercise | Result | What the sidecar showed |
| --- | --- | --- |
| crash matrix, full (pre-#304 branch) | **19 passed, 1 failed, 0 skipped**, 4167s | `after-vote-commit` failed with `HelperDeliveryIncomplete`. It passes on `main`, so #303/#304 closed it. |
| crash, `after-vote-commit` on `main` | pass, 356s | Also passes with this branch's changes applied — 100/100, no regression. |
| fleet, `half-then-other-half` | pass, 370s | 64 acceptances preserved on the departed half; 648 placed by the resume on helpers never tried; 144/144 confirmed. |
| hang, `share-post` | pass, 999s | 112 POSTs hung below the SDK's own 30s deadline; the run ended itself and converged on resume. |

**Not yet run against staging:** the other four fleet scenarios, and eight of
the nine hang targets. Neither axis has been run end to end, and neither has
been run twice. Read the table as "these exercises work", not as "these axes are
verified".

**What the first fleet runs cost, and bought.** Three consecutive runs failed or
passed vacuously, and every one was this suite's fault rather than the SDK's:
`D1` swept in refusals the SDK is right to forget; `D4` demanded a placement
target that a confirmed share does not owe; and the scenario itself finished the
round under its first fleet, so the flip never happened and every assertion held
against a completed round. None of those were visible from reading the code.
That is the argument for running a conformance suite rather than reviewing it,
and the reason each result above is quoted from the database rather than from an
exit code.

> **The three runs below cover the crash matrix only**, and predate #304.
> Nothing in that table should be read as covering the hang or fleet axes.


Three consecutive runs of the full matrix against staging, every stage
exercised and none skipped:

| Run | Result | Duration |
| --- | --- | --- |
| 1 | 20 passed, 0 failed, 0 skipped | 2938s |
| 2 | 20 passed, 0 failed, 0 skipped | 3082s |
| 3 | 20 passed, 0 failed, 0 skipped | 3167s |

Each run provisions a control round plus one per stage — roughly twenty fresh
rounds, sixty across the three, every delegation and vote real and on
`svote-1`.

Under test: this crate, plus **two sets of SDK fixes that are not on this
branch**. The runs are honest only with both named, and this branch does not
pass without them.

- The seven `votes.tx_hash` completion fixes, which this suite found and which
  are in review separately. Without them the vote and share stages cannot
  converge at all.
- The delegation-proof phase fix that `52d0229c` carries on another branch.
  Without it `after-signing` fails deterministically: a bundle persisting its
  proof after another bundle advanced the round-wide phase to `VoteReady` is
  refused as a phase regression.

Both need to land before this branch's own base turns the matrix green. Running
it here today reproduces the defects rather than passing over them, which is
the suite working as intended, not a broken branch.

### What these runs are worth

Earlier runs of this matrix also reported green, and meant considerably less. A
review found four ways a run could pass over ground it had not tested: the
matrix could skip every stage and still report success, the no-second-POST
claim was printed rather than asserted, the control comparison read only
submission state names, and two crash boundaries did not fire where their names
said. The runs above are the first taken after those were closed, so they are
not a repeat of the earlier ones at a higher number — they are the first that
carry the weight the table implies.

### What it caught

Seven SDK defects, all from one root cause — `votes.tx_hash` read as proof a
vote finished, when a vote confirmed by an exact-tree scan never carries one.
Three failed closed and stalled rounds; three failed **open**, permitting a
rebuild of an on-chain vote, a conflicting ballot intent, and silent overwrite
of an on-chain vote. A seventh erased the helper attempt marker in release
builds only, invisible to a suite running with debug assertions on.

Plus an intermittent multi-bundle phase regression: a bundle persisting its
delegation proof after another advanced the round phase to `VoteReady` was
refused. It passed three earlier runs, then failed twice consecutively once
coverage was complete — the kind of race this matrix exists for.

Each fix carries its own commit and hermetic regression test; the specifications
in `docs/` state the rules they restored.

## Design notes

**Every transport must be built on the same route, and one was not.** Vote-tree
reads default to a fresh `HyperTransport` over a route of their own, so until
`RoundExecutor::with_tree_transport` was wired to the shared one they escaped
both fault wrappers — silently, because the round still worked. The stall matrix
is what caught it: `commitment-tree-read` reported that no request of its class
was ever seen, when the requests were in fact being made somewhere no wrapper
could observe. The same gap hid tree traffic from the helper fleet. A transport
the suite forgets to inject is a transport the suite cannot claim anything
about, and nothing else would have reported it.

**Why the faults are injected at `RouteHttp`.** It is the lowest HTTP seam the
SDK exposes, and every deadline the suite is trying to check — the chain post
timeout, the helper fan-out budget, the PIR request budget, the tree request
timeout — is applied *above* it. A stall injected there is therefore a stall the
SDK is supposed to end. Injecting higher, at the helper or chain transport,
would test the wrapper's own patience instead. One seam also covers chain,
helper, PIR and tree traffic at once, which is what makes "every network
request" a claim the taxonomy can actually enumerate rather than a slogan.

**Why the helpers are rewritten rather than proxied.** A local proxy would need
a port, a certificate the wallet would rightly refuse, and a second HTTP
implementation sitting between the SDK and the helper. Substituting the host
inside the request leaves TLS, SNI and the response entirely the backend's own,
and the wallet still writes down the synthetic URL — which is the identity every
placement and recovery decision is made against.

**Why the fault wrappers are two, not one.** `StallingRoute` decides which
request class stops answering; `HelperFleetRoute` decides which helpers exist.
Stacking them keeps each doing one thing, and an empty plan makes either a
pass-through, so a control run and a faulted run share one code path rather than
being two implementations whose agreement would prove nothing.

**Why share tracking runs through `ShareTrackingDriver`.** It used to assemble
its own loop out of `track_pending_shares`, a pass counter and a delay it
computed itself. That was retired: `ShareTrackingDriver` owns the cadence now,
and the pieces the loop was built from were a second, unenforced schedule beside
the one the invariants document specifies. A conformance suite asserting
recovery behaviour has to drive shares the way a host actually does, or it is
asserting against a schedule no host will ever run. Two things the fleet
scenarios depend on fall out of the change: the host's helper fleet is re-read
**once per pass**, so a fleet can change under a live run, and
`ShareTrackingRunReport` names the distinct helpers a run reached for each
share — which is the only record of where a run *declined* to send again.

**Why a run is bounded by the suite rather than by the driver.** A healthy
tracking run ends at the round's vote end, which for these rounds is fourteen
days out. The suite races it against its own budget instead; dropping the future
is a supported way to end a run, so the race is not a leak.


**Why a child process.** Both provers run on dedicated 64 MiB-stack OS threads
that are deliberately not cancellable, and they hold the round lock through a
cloned `Arc` so it outlives a dropped future. An in-process crash model would
leave a live prover still holding that lock and still writing to the sidecar the
"restarted" run had just reopened — corrupting the state under test. Killing the
process is what makes the detached prover go away.

**Why the round lock does not help us.** `vote_work::round_lock::ROUND_LOCKS` is
a process-global map. It gives no cross-process exclusion, so safety across the
crash rests entirely on SQLite locking plus the `chain_submissions` triggers and
unique indexes — which is exactly what should be under test.

**Why a missed stage is a failure.** A child that finished the round would
satisfy every assertion about "the state a crash left", because the state
inspected would simply be a completed round. `run_until_crash` therefore
requires `SIGABRT` at the armed stage and rejects the
`EXIT_STAGE_NEVER_REACHED` exit, so a stage that stops firing fails loudly
instead of decaying into a no-op.
