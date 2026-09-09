# Round orchestration invariants

## Status and purpose

This document is the normative specification for round planning and vote-work
orchestration in `zcash_voting`: how the SDK decides, from durable state and
the authenticated roster, what work a round still owes, and how the round
executor carries that work out. Changes to the behavior described here must
update this document and its behavior-oriented conformance tests in the same
change.

It sits between two existing specifications and does not restate them:

- [`chain_submission_invariants.md`](chain_submission_invariants.md) owns the
  durable lifecycle of one chain submission (reservation, dispatch
  classification, tracking, recovery, confirmation). Orchestration consumes
  that lifecycle through its phase projections and drives it through
  `ChainSubmissionClient::advance_until_terminal_in_epoch`; it never reads or
  writes `chain_submissions` rows directly.
- [`helper_submission_invariants.md`](helper_submission_invariants.md) owns
  helper-share planning, delivery, tracking, and recovery for one share.
  Orchestration decides *which* shares are owed and *when* a plan must exist;
  the helper specification decides how a plan is built and delivered.

The design has one rule, stated once, that every planning decision derives
from:

> Undispatched work follows the current roster. Possibly dispatched or
> confirmed work survives roster changes. An atomic batch is one unit and is
> never partially retired, advanced, or recast. Helper plans and the round's
> immediate-share designation stay bound to the durable generation they were
> made for.

Every place that rule used to be re-derived (planner filters, retirement sets,
fallback loops, executor rerouting, delivery-plan exceptions) is replaced by
one classification over one snapshot, described below.

## Scope and authority

Orchestration owns:

- reading the round's durable state as one consistent snapshot;
- grouping votes into units (a singleton or one atomic batch);
- classifying each unit and each bundle into exactly one obligation;
- projecting obligations into the host-facing `RoundPlan` and `NextStep`
  list, in a stable order;
- executing one obligation per executor call under the right lock, with the
  step's scope captured once and its partial results carried through every
  outcome and failure;
- composing those calls into a run that ends at a state only the host can
  resolve, under a stated pacing and failure policy;
- the round-wide immediate-share designation as durable state.

The last two are separate layers on purpose. `vote_work` executes exactly one
obligation per call and returns. `round_drive` decides which obligations to run
and when to stop, and owns no classification of its own. A host supplies
transports, timing, signing material and cancellation to both.

Orchestration does not own chain submission state, helper delivery mechanics,
delegation proving, transport, or sidecar identity. It reads their phase
projections and calls their typed entry points.

## Terminology and state model

A **round snapshot** is a plain-data view of everything orchestration reads
for one wallet and round: bundles with their delegation phase and tx hash,
votes with their canonical vote phase, stored choice, tree position, tx hash
and recovery summary, share rows with their phase and helper acceptance,
ballot intents, helper-plan presence per vote, the persisted immediate-share
designation, and the lifecycle hash of every in-flight atomic batch row. It is
loaded inside **one deferred read transaction on one connection hold**, so no
in-process writer and no other process can interleave a write between two of
its reads. It carries no database handle.

A **vote unit** is the smallest thing the chain lifecycle dispatches: either a
**singleton** `(bundle_index, proposal_id)` or an **atomic batch**
`(bundle_index, ordered_batch_digest)` with its ordered members. Units are
formed from the votes' persisted recovery bundles. A persisted batch whose
members are not all present, not all in one vote phase, claimed by two
batches, or whose submitted members report different transaction hashes, is a
**planning invariant violation** and the plan fails with `InvalidInput`
rather than guessing.

Each unit has a **lifecycle position**, an exhaustive coarsening of
`VotePhase`:

| Position | `VotePhase` | Meaning |
|---|---|---|
| Uncast | `Prepared`, or no row | Nothing durable the chain could have seen. |
| Undispatched | `Committed` | Proof, recovery bundle and signature are durable; no POST reserved. The wallet owns it: it may be retired or recast. |
| OnWire | `Submitted`, `SubmissionManaged` | A POST was reserved or dispatched. The chain lifecycle owns it: it is driven to resolution whatever the ballot or roster now say. |
| Terminal | `SubmittedWithoutHash`, `SubmissionRejected` | The lifecycle ended without a confirmation. No step is planned. A hashless dispatch may have landed, so it holds its bundle against a fresh cast; a rejected vote spent nothing and holds nothing. |
| Confirmed | `Confirmed` | The vote has a tree position; its helper shares are owed. |

A unit is **lifecycle-owned** when its position is OnWire, Terminal, or
Confirmed. That predicate has one home and is used by the planner, by the
ballot-intent write path (a lifecycle-owned intent cannot be cleared), by
retirement, and by helper-plan derivation.

Each unit has a **roster relation** to the authenticated roster: **Rostered**
when every member's proposal is in the roster, **LeftRoster** when any member
is not. The relation is per unit, not per member, because a batch is
indivisible.

Each unit has a **ballot relation** to the durable ballot intents:
**Agrees** when every member's intent is `Choice(c)` with `c` equal to the
stored choice; **Unrecorded** when some member has no intent and no member
conflicts; **Conflicts** when some member has `Skipped` or a different
`Choice`. A stored choice that disagrees with the member's own recovery bundle
is a planning invariant violation.

An **obligation** is one unit of executable work the round still owes. The
canonical obligations are:

| Obligation | Subject | Carries |
|---|---|---|
| `Delegate` | bundle | bundle index |
| `AdvanceDelegation` | bundle | bundle index, whether the delegation is a structurally imported capability, phase, tx hash |
| `Retire` | vote unit | unit id and every member; an undispatched unit a member of which left the roster |
| `Cast` | bundle | every draft `(proposal_id, choice)` the bundle must cast, the units to retire first, the delegation prerequisite if any |
| `ReconcileChain` | vote unit | unit id, ordered member proposals, phase, tx hash, the delegation prerequisite if any |
| `Deliver` | confirmed vote | vote key, tree position, the share indexes owed, whether a durable helper plan already exists |
| `Confirm` | share | share key; whether a helper accepted it; whether an attempt's outcome is unknown or still in flight |
| `Blocked` | bundle cast | the reason the host must resolve: an open ballot, or unrostered intents to clear |

An obligation carries everything its execution needs. The executor never
rescans the plan for sibling steps, never re-derives batch membership from an
anchor, and never reinterprets one step kind as another.

## Planning invariants

### Snapshot atomicity

- `resume_plan` reads the round through one snapshot loaded in one deferred
  read transaction, and performs no other database read.
- The snapshot loader receives only a transaction handle, so it cannot
  re-enter the connection mutex, and it never writes.
- Two plans over the same durable state and the same roster are equal.

### Classification

Classification is a pure function of the snapshot, the units and the roster.
It has no clock and no network. The per-unit rule is one exhaustive match:

| Lifecycle | Roster | Ballot | Obligation |
|---|---|---|---|
| Undispatched | Rostered | Agrees | `ReconcileChain` |
| Undispatched | Rostered | Unrecorded | none; the unit holds its bundle. For a batch, `Agrees` means every member agrees: one undecided member holds the whole batch (`an_undispatched_batch_holds_until_the_ballot_agrees_with_every_member`), and its already-decided members are `withheld_casts` because nothing has been dispatched for them either (`a_held_batch_withholds_the_members_the_ballot_already_decided`) |
| Undispatched | Rostered | Conflicts | singleton: none, and it holds nothing; the cast pass recasts and the persisted cast replaces the row. Batch: invariant violation, since the intent write path clears an unsubmitted batch whole |
| Undispatched | LeftRoster | any | `Retire` for the whole unit; the cast pass recasts the rostered members |
| OnWire | any | Agrees, Unrecorded | `ReconcileChain` |
| OnWire | any | Conflicts | invariant violation |
| Terminal | any | Agrees, Unrecorded | none |
| Terminal | any | Conflicts | invariant violation |
| Confirmed | any | Agrees, Unrecorded | per member: `Deliver` for missing shares, `Confirm` for every unconfirmed submitted share, carrying whether it was accepted or has an outcome-unknown attempt |
| Confirmed | any | Conflicts | invariant violation |

The consequences that follow, and that tests pin:

- A `Committed` vote for a proposal that left the roster does not hold its
  bundle: it is retired, and the bundle's rostered proposals are cast again.
  If it belongs to a batch, the entire batch is retired and every rostered
  member is recast.
- A `Submitted` or `SubmissionManaged` vote, and every confirmed vote, is
  advanced or delivered whether or not its proposal is still in the roster
  and whether or not it has a recorded intent. Its shares are owed to the
  helpers whatever the roster now says.
- A conflicting intent for anything past `Committed` is an error, never a
  recast.
- The **unrostered intents** the host must clear are the durable intents
  outside the roster whose proposal is not covered by any lifecycle-owned
  unit. A lifecycle-owned intent cannot be cleared and is neither reported nor
  allowed to withhold casting.

### Cast gating

For each bundle and each rostered proposal with a `Choice` intent that no
live unit covers with the same choice (a unit being retired, and a stale
singleton, are not live), a cast is due. A bundle is **held** by a live unit
in `Committed`, `Submitted`, `SubmissionManaged` or `SubmittedWithoutHash`, or
by a delegation in `SubmissionManaged`, `SubmittedWithoutHash` or
`SubmissionRejected`; a due cast on a held bundle plans nothing at all, not
even the delegation prerequisite, because nothing can be cast there until the
holder resolves. On a free bundle the cast is `Cast` when the ballot is
**terminal** (no open proposal and no unrostered intent to clear) and
otherwise `Blocked` with the reason.

`Blocked` is never projected as a `NextStep`; the plan reports it through
`open_proposals`, `unrostered_intents` and the absence of a cast step.

How a bundle's drafts are grouped into cast obligations follows the build's
`ATOMIC_VOTE_BATCHES_ENABLED` constant, and nothing else. It ships enabled;
target chains must serve `cast-vote-batch` before adopting this SDK version:

- with batching **on**, all drafts for one bundle form one `Cast` obligation
  and are cast as one atomic batch (or one singleton when there is one draft)
  (`every_draft_of_a_bundle_is_one_cast_obligation_with_its_delegation_prerequisite`,
  `a_cast_step_for_one_proposal_resolves_to_the_bundles_whole_draft_set`);
- with batching **off**, each draft becomes its own `Cast`
  obligation over a single proposal, and each is dispatched as a singleton on
  `cast-vote`. The conformance tests require the shipped, enabled shape.

The grouping decision is made once, in `classify`, and the rest of the
specification is written in terms of the units it produces. A singleton
obligation is terminal on its own: it is planned, cast, confirmed and has its
shares delivered without reference to the bundle's other drafts. The
atomic-batch rule — one unit, never partially retired, advanced, or recast —
governs the batches that exist, and with batching off none are formed for new
work. A round whose durable rows already hold a batch keeps planning as a
batch under either setting, because units are formed from the votes'
persisted recovery bundles rather than from the constant.

### Delegation obligations

- `Delegate` is planned for a bundle whose delegation is `Prepared`,
  `PcztBuilt` whenever that bundle has a `Cast` or a
  `Blocked` cast, so the prerequisite is visible while the voter decides the
  rest of the roster.
- `AdvanceDelegation` is planned for `Submitted` and `SubmissionManaged`
  delegations; an imported capability advances without a signer.
- `SubmittedWithoutHash` and `SubmissionRejected` delegations plan nothing and
  block their bundle's casts.
- `Delegate` prepares and persists the proof only; it never signs or broadcasts.
  A `Proved` bundle waits for a terminal ballot, then `Cast` signs delegation
  and casts together through `delegate-and-cast-vote-batch`, including a
  one-choice ballot. The first witness is synthetic and needs no tree sync.
- A combined recovery unit owns its delegation prerequisite. It plans only
  the batch reconciliation, never a standalone `AdvanceDelegation`.
- Existing standalone and imported delegations retain their prerequisites.
  Terminal-ballot signing preflight covers fresh combined casts before the
  wave starts. Early proof preparation needs a driver but no signature.
- `a_delegate_step_cancelled_after_preparation_keeps_the_proof_without_signing`
  and `a_partly_decided_ballot_still_runs_the_delegation_prerequisite` pin the
  early-preparation boundary. `combined_lifecycle_confirms_delegation_and_every_vote_together`
  pins the shared submission ownership and atomic confirmation.
  The release-only `combined_executor_zkp2_prepares_submits_confirms_and_delivers`
  exercises the executor with real cast proofs: early preparation does not
  sign or POST, terminal casting submits one combined envelope, and helpers
  receive shares only after the delegation and every vote are confirmed.
  `combined_executor_zkp2_cancellation_reopens_and_resumes_without_signing`
  cancels after combined persistence, reopens the database, and advances the
  exact same envelope without a hotkey or delegation driver. Both run through
  `make proofs` with scripted transports, without contacting a live chain.
- A `Cast` on an imported delegation never owes the voter's key: the imported
  transaction is already on the chain, the cast waits for it to confirm, and
  the wallet holding an imported capability has no delegation key to offer.
  `an_imported_delegation_with_a_terminal_ballot_needs_no_signer` pins this;
  only a fresh combined cast is counted by the signing preflight.
- Whether a cast signs its own delegation is decided once, by planning, and
  carried on the obligation as `Obligation::Cast::signs_delegation`. It is
  true for a fresh combined cast and false for a delegation that is already
  `Confirmed` or imported. The signing preflight reads that flag rather than
  re-deriving the answer from the delegation phase and the imported flag a
  second time, so the preflight and the plan's work summary cannot disagree
  about which bundles owe the voter's key.
- A combined batch whose first POST the chain definitely rejects is retired by
  the lifecycle (see `docs/chain_submission_invariants.md`, "Definite
  rejection"): the bundle's delegation reads `Proved` again and the next plan
  owes a fresh `Cast` with a new digest. The run that observed the rejection
  ends `ChainTerminal` and never recasts within itself; a host that runs again
  retries once per run. The release-only
  `combined_executor_zkp2_rejection_returns_the_bundle_to_proved_and_recasts_on_the_next_run`
  exercises both runs.
- That per-run retry is bounded. Retirement deliberately leaves the delegation
  setup untouched, so nothing else durable records that the chain refused this
  delegation, and an unbounded retry would re-prove every member and re-POST
  the identical delegation on every run forever. The lifecycle therefore counts
  consecutive rejections in `combined_cast_rejections`, keyed by the delegation
  generation each recast reuses rather than by the batch digest, which is
  re-randomized on every cast. At `MAX_CONSECUTIVE_COMBINED_REJECTIONS`
  (currently 2) the bundle joins the holding set: planning emits neither `Cast`
  nor `Delegate` for it, and the round reports `blocking_recovery` because a
  person must decide whether the delegation is worth another attempt.
  `consecutive_combined_rejections_accumulate_against_one_delegation_generation`
  pins the accumulation, the cap and the host's release.
- The block is advisory, not a terminal delegation phase, and it is
  self-healing in three ways. A confirmed batch clears the streak, so a later
  unrelated rejection starts from one
  (`a_confirmed_combined_batch_forgets_its_rejection_streak`). Discarding an
  unbroadcast delegation setup deletes the ledger row with it, because the
  generation the count described is gone. And the snapshot only blocks a
  bundle whose *current* delegation generation still equals the one the
  rejections were counted against, so rebuilding or re-proving the delegation
  lifts the block on its own. `VotingDb::retry_blocked_combined_cast` is the
  host's explicit "the cause is fixed, try again": it clears the streak, takes
  the round's submission gate, and touches no proof or signature, so the next
  cast reuses the same delegation and a further rejection restarts the streak
  at one.
- For hosts: the delegation signature prompt has moved from ballot-open to
  ballot-terminal time. `NeedsDelegationSignatures` no longer fires for
  `Delegate`, which only prepares the proof; it fires for the `Cast` that signs
  the combined transaction. `RoundStepProgress::DelegateAndVoteBatchPersisted`
  is a new progress kind between vote signing and helper planning.

### Share obligations

- A confirmed vote's expected share indexes come from its recovery bundle.
  Indexes with no share row are `Deliver` work.
- A `Submitted` share row is `Confirm` work. Dispatch delivers such a row
  again from its durable plan only when no helper has reached it at all (no
  accepted, ambiguous, or in-flight helper): no helper holds it, so polling
  cannot confirm it. A row with an acceptance or an ambiguous or in-flight
  attempt belongs to background tracking: redelivery excludes those helpers,
  and only tracking can confirm, reconcile, or replenish it
  (`a_confirm_share_step_for_an_accepted_share_polls_instead_of_delivering`,
  `a_share_with_only_ambiguous_evidence_is_polled_not_redelivered`,
  `a_blocking_confirm_share_step_delivers_before_polling`).
- `Deliver` states whether the vote's durable helper plan exists. A fresh
  cast, and a `ReconcileChain` for a unit that was never dispatched (a cast
  whose plan preparation failed after persistence), make plans durable before
  the chain broadcast; work already on the wire reconciles the chain first
  and loads or creates the plan only after confirmation, right before
  delivery, so an open ballot cannot keep an already-dispatched vote from
  being polled or recovered
  (`a_committed_vote_never_dispatched_prepares_its_plan_before_the_chain`,
  `a_dispatched_vote_is_reconciled_before_its_ballot_is_terminal`).

### Projection

`RoundPlan` and `NextStep` are a projection of the obligations, not a second
source of truth:

- one `NextStep` per non-blocked obligation, with `Cast` expanding to one
  `CastVote` per draft, `ReconcileChain` to `AdvanceVote` or
  `AdvanceVoteBatch` (anchored on the first member), `Deliver` to one
  `SubmitShares` per owed index, `Confirm` to `ConfirmShare`;
- steps are ordered delegation first, then vote and share submission,
  then share confirmation; within a rank proposal-primary, then bundle, then
  share index, so an interrupted question finishes across bundles before a
  later question resumes;
- `blocking_prerequisite` answers from the obligation's prerequisite;
- the derived flags (`has_unconfirmed_shares`, `blocking_share_work`,
  `has_recoverable_vote_or_share_work`, `immediate_share_key`,
  `immediate_share_confirmed`, `recovered_*_work`, `primary_action`) are
  computed from the obligations and the snapshot only;
- `delegation_bundles_needing_work` and `delegation_bundles_needing_signing`
  are the per-bundle form of `needs_delegation_signing` and
  `has_in_flight_delegation`, ascending and deduplicated. Any delegation step
  puts its bundle in the first. `Delegate` and `AdvanceDelegation` put it in
  the second — a delegation in flight is not signed and done, because advancing
  one re-signs its locked generation — so the second is exactly the per-bundle
  form of `needs_delegation_signing` and cannot disagree with it.
  `AdvanceImportedDelegation` is the one exclusion: an imported capability is
  already broadcast and never asks the voter for a signer. A host showing
  per-bundle delegation progress reads these rather than filtering
  `delegation_statuses` against these rules itself, which is how a host drifts
  from the planner.

Regression tests: `delegation_bundles_are_reported_per_bundle_ascending`,
`an_imported_delegation_owes_work_but_never_the_voter_s_key`,
`a_bundle_with_several_delegation_steps_is_named_once`, and
`a_plan_with_no_delegation_work_names_no_bundles` in
[`round_planning/tests/resume_plan.rs`](../zcash_voting/src/round_planning/tests/resume_plan.rs).

A round holding a ballot choice with no bundle rows owes bundle setup, not
vote work. Eligibility does not persist a bundle plan, so a host that records
a ballot before running setup reaches this state on a fresh round. It is a
resolvable ordering condition, not malformed input: the plan reports
`needs_bundle_setup` with a `Delegate` primary action and no steps, so the
round stays plannable and the recorded choices are still reported. Planning
never treats it as "nothing to do", which would silently skip the round
(`choice_intent_without_bundles_reports_bundle_setup`).

A host-selected `NextStep` is resolved back to exactly one obligation in a
fresh plan taken under the lock, or to no work.

## Identity: what is durable and what is derived

| Fact | Authority | Derived where |
|---|---|---|
| vote phase, delegation phase, share phase | `chain_submissions`, `votes`, `bundles`, `share_delegations` | `phases.rs` projections |
| atomic batch membership and order | recovery bundle JSON on each member, batch digest | unit grouping |
| helper plan for a vote | `helper_share_plans`, cleared by trigger when the vote's undispatched generation changes | plan presence in the snapshot |
| round immediate-share designation | `round_immediate_share` row, first writer wins, immutable, voided with its undispatched generation | seeded once from the highest eligible bundle and lowest chosen proposal |
| roster, vote end, last-moment window, helper fleet | host, per call | never persisted; classification input |
| lifecycle ownership of an intent | derived from the unit's lifecycle position | never persisted |

The legacy `rounds.phase` column is a lossy round-wide high-water mark, not an
authority for bundle completion. Bundles progress independently: one bundle may
persist a vote and move the marker to `VoteReady` while another still owes its
delegation proof. Persisting that later proof must commit its bundle-local
artifacts and preserve `VoteReady`; it must not regress the marker, reject the
valid completion, or infer that the proof can be skipped. Proof reuse is
decided from the bundle's canonical durable delegation phase and target
binding.

The immediate-share designation is the one orchestration decision that must
survive restarts and roster changes exactly as first made, so it is durable
state, not a value re-derived from whichever roster the host passed. A
persisted designation for a proposal that later leaves the roster still
names the round's immediate share; a plan is never re-derived to name a
second one.

## Check-then-act

Classification is a fact about a read snapshot. Execution happens later, so
the planner never gates on a fact that the act does not re-verify inside its
own write transaction or lock:

| Obligation | Serialized by | Re-verified at act time |
|---|---|---|
| `Retire` | round lock | the row is still `tx_hash IS NULL AND vc_tree_position IS NULL` and not lifecycle-owned, inside `BEGIN IMMEDIATE`; batch members are expanded from the durable batch, not the snapshot |
| `Cast` | round lock | retirement as above; the bound hotkey is the bundle's confirmed delegation target; the vote end has not passed on the host clock; the persist step refuses a vote whose recovery bundle appeared meanwhile |
| `ReconcileChain` | round lock, then the chain lifecycle's own generation lock | the generation digest compare-and-swap of the chain lifecycle |
| `Deliver` | round lock | plan creation under `BEGIN IMMEDIATE` with the `commitment_bundle_json` compare-and-swap; designation compare-and-swap; `CommittedVote::confirmed` re-reads the generation |
| `Confirm` | per-share operation lock | the helper specification's quorum rules |
| `Delegate`, `AdvanceDelegation` | bundle lock | the delegation pipeline and coordinator |

A vote has reached the chain when **either** witness of its confirmation is
present: `tx_hash IS NOT NULL OR vc_tree_position IS NOT NULL`. Hash
confirmation writes the first and tree confirmation the second, and the schema
forbids a tree confirmation from carrying a hash, so no query may treat the
hash alone as the answer — see "Authoritative durable record" in
[`chain_submission_invariants.md`](chain_submission_invariants.md) for why. The
`Retire` row above states the negation of that rule. Two kinds of check depend
on it, and they break in opposite directions.

A check asking whether the vote is *finished* fails closed and stalls the
round. A tree-confirmed vote must clear its proposal's authority bit, or the
next vote on the bundle rebuilds a stale vote-authority note and the chain
rejects its nullifier as already spent
(`a_tree_confirmed_vote_clears_its_proposal_authority_bit`); it must not count
as a competing pending vote chain, or it locks its bundle out of every later
proposal (`a_tree_confirmed_vote_is_not_a_competing_pending_chain`). Vote-tree
sync goes further still: a POST that was released spends the delegation VAN
whether or not its response was ever classified, so the durable
`chain_submissions` row retires the bundle's `gov_comm` expectation rather than
waiting for a hash (`a_dispatched_vote_retires_its_bundles_van_expectation`).

A check *refusing* an act on a vote already on chain fails open and permits
what it exists to prevent: rebuilding the vote into a competing generation
(`a_tree_confirmed_vote_cannot_be_rebuilt`), accepting a ballot intent that
disagrees with it
(`an_intent_conflicting_with_a_tree_confirmed_vote_is_refused`), or replacing
its choice and commitment (`a_tree_confirmed_vote_cannot_be_replaced`). This
class is the more dangerous, because a stalled round announces itself and a
permitted rebuild does not, which is why all three are tested rather than
assumed.

Delegation setup uses the chain coordinator's matching hierarchy: shared
account access, shared round access, and exclusive access to its bundle.
Distinct bundles can therefore build setup concurrently, while wallet or
round deletion and chain lifecycle work for the same bundle remain excluded
(`another_bundle_builds_while_delegation_setup_is_active`,
`delegation_setup_excludes_only_its_bundle_lifecycle`).

The executor takes exactly two authoritative plans per step: one un-locked to
choose the step, one under the lock to resolve it to an obligation. The plan
returned on an outcome is a host-facing projection, not a control input.

## Executor invariants

- **Scope is captured once.** Wallet id, round id and its bytes, roster,
  network, hotkey material, host inputs and the operation epoch are captured
  at step entry into one scope value and read from it for the step's whole
  duration. A step never re-reads its binding or wallet id part-way through.
- **Partial results are kept.** A step accumulates its chain outcome, share
  delivery reports and signed delegation in one ledger; every outcome,
  cancellation and failure is built from that ledger, so a later error cannot
  drop an earlier durable confirmation or an accepted delivery.
- **Interruption is observed at every boundary.** Cancellation or an epoch
  change ends the step as `Cancelled` before the next network or proving
  boundary, and a queued lock wait abandons its place.
- **Locks outlive detached work.** A lock is held by the detached proving or
  re-signing task, not by the future that may be dropped.
- **Delivery evidence has one classifier.** Vote completion and delivery
  diagnostics share `share_tracking::delivery_progress`: a completed task with
  no accepted or ambiguous helper is incomplete; an ambiguous-only share waits
  for tracking; definite acceptance of every share permits completion. The
  existing `a_share_every_helper_answered_ambiguously_waits_for_tracking_rather_than_advancing`
  conformance test covers the executor classification.
- **Helper observations name the delivered share.** An atomic step retains its
  anchor proposal identity, while every member's helper HTTP attempts and retries
  bind that member's actual bundle, proposal, and share index
  (`atomic_round_delivery_attributes_every_proposal_share_and_retry` in
  `share_tracking/tests/observability.rs`).
- **One completion path.** Fresh casts and resumed units go through one
  completion routine that differs only in when helper plans are made
  durable; there is no second driver.
- **A confirmed unit delivers through one shared share queue.** Fresh casts,
  combined delegation-and-cast completion, and resumed chain reconciliation
  all recover fresh confirmed handles and use the same bounded queue across
  their ordered members. Every member's full plan is validated before its
  first share is admitted. A local preparation or execution failure does not
  prevent independent eligible members from completing. No helper POST starts
  before durable chain confirmation. Already-confirmed `Deliver` obligations
  remain per proposal and invoke the same queue with one vote; this does not
  change planning, step selection, or lock scope.
  The shared queue admits at most 32 active share workflows process-wide,
  with aggregate planned fan-out bounded by the helper specification's weighted
  admission budget. The independent initial-POST ceiling remains 128. These ceilings do not
  change unit grouping, proposal order, or per-share placement and recovery.
- **Every finalized delivery report is retained before deciding the step.**
  `ShareOutcome` events are emitted once per available report in completion
  order, with the actual vote identity, after recording it in the ledger.
  The drained ledger is normalized to original unit order. The first hard
  error in unit order (and persisted payload order within a vote) wins,
  including over cancellation. Without a hard error, interruption wins, then
  any incomplete delivery yields `HelperDeliveryIncomplete`, then any
  ambiguous-only proposal yields `Pending`; only all-complete delivery yields
  `Advanced`. A fast ambiguous proposal therefore cannot hide a later
  incomplete one. Shared preflight failures still end the operation before
  delivery. Every exit retains the chain outcome and signed delegation.
  These contracts are covered in `share_tracking/tests/delivery_queue/executor.rs`
  by `round_driver_refills_across_confirmed_members_and_retains_every_report`,
  `round_completion_folds_all_proposals_before_deciding_disposition`,
  `round_failure_retains_confirmation_and_successes_from_later_proposals`, and
  `hard_error_outranks_callback_cancellation_after_all_durable_effects_are_kept`.
  `combined_reconciliation_delivers_later_proposals_while_the_first_is_unfinished`
  in `chain_submission/generation/tests/combined/helper_delivery.rs` exercises
  combined-envelope recovery through `RoundDriver` without invoking the prover.
- **Prerequisites are refused at dispatch.** A step whose obligation carries
  a delegation prerequisite fails with `InvalidInput` naming it, before any
  I/O.
- **A listed step always resolves.** A step the locked plan no longer lists is
  `NoWork`: another pass finished it. A step that plan *still* lists but that
  resolves to no obligation is an `InvariantViolation`, because both facts come
  from the same read and can only disagree if projection and classification
  have. Answering `NoWork` there would let any caller that re-selects from a
  refreshed plan loop on it forever, so the refusal lives here rather than in
  each host's loop.

## Round driving

`round_drive` composes executor calls. It adds no facts about a round; every
decision about what a step *means* stays in the planner and the executor. Its
`mod.rs` is a facade holding the host-facing types and the entry point; the
mechanism is in children, one per responsibility — `run_loop`, `selection`,
`signing`, `dispatch`, `run_ledger`, `quiescence`, `tally`, `policy`,
`progress` — so a change to one decision has one place to go.

- **There is one way to choose work.** The executor runs the obligation a
  host-selected step resolves to; the driver is what chooses steps. No entry
  point advances a round from its plan head on its own, because that is a
  second driver with none of this section's guarantees.
- **Selection is always from a plan the driver read itself.** The plan on a
  `RoundStepOutcome` is a host-facing projection, not a control input, so it is
  reported to the host and never used to choose the next step.
- **The host context is read once per dispatch, not once per run.** A run can
  take minutes, and a long proof can cross the last-moment or vote-end
  boundary, so the step that follows plans against the clock it actually runs
  under. This does not weaken "scope is captured once": each step still
  captures one context at entry and reads it for its whole duration.
- **Round-locked obligations never run concurrently; bundle-locked ones may.**
  Two round-locked steps in flight would queue on one lock and gain nothing but
  a held proving thread. One fresh plan admits an ordered wave of distinct
  bundle-locked steps up to `max_bundle_concurrency` and the remaining dispatch
  budget; the wave stops before the first round-locked step. Every admitted
  step captures its own host context. Results are folded in dispatch order
  after the complete wave drains, so every durable effect is retained.
- **Scheduling and locking read one table, not two.** `round_lock::bundle_scope`
  is the single definition of which lock a step takes; the executor locks with
  it and the driver schedules with it. They must not be two matches that can
  drift, because a driver that believed a round-locked step was bundle-locked
  would admit a wave of steps that then serialize on one lock, each holding a
  proving worker open for the wait. The bundle a wave deduplicates on is the
  same bundle the executor locks, so no two admitted steps can contend
  (`the_driver_schedules_by_the_executors_own_lock_scope`,
  `only_delegation_proving_is_bundle_scoped`).
- **An abandoned run reports only that it was abandoned.** Every pre-dispatch
  early return describes a state of the round, and a run the host has left must
  not describe one — there is no dispatch after it whose epoch binding could
  correct the answer. Each blocking or host-facing operation before such a
  return is therefore followed by an interruption check: the plan read and the
  `PlanRefreshed` callback, then building the per-dispatch host contexts and
  reading stored signing material
  (`an_epoch_switch_during_planning_is_not_reported_as_a_round_state`,
  `an_epoch_switch_while_gathering_contexts_is_not_a_signature_handoff`).
- **A wait must lead somewhere.** A re-poll is a pause before another
  dispatch, so it is not scheduled once the dispatch budget is spent: the wait
  could not produce another poll, and a host-configured interval would hold the
  run open for its whole length before the next pass could report the
  exhaustion.
- **A failed plan read does not outrank an interruption.** The read spans the
  same window as a successful one, so an abandoned run is not reported
  differently merely because its concurrent database read happened to fail.
  This guard has no conformance test: a cancellation observable on the error
  path is observable at the check one statement earlier unless it lands inside
  the read itself, and nothing in the API can place it there deterministically.
- **A report describes the round the run left, not the one it found.** A wave
  makes durable progress and can then stop the run, so the plan and tally read
  before it no longer describe the round: a rejection the wave persisted would
  still be listed as a step to run, and a proposal whose vote confirmed would
  still count as incomplete. Both are refreshed from durable state before a
  wave-ending quiescence is reported, and before a cancellation observed on the
  pass after a wave — that return owes the refresh for the same reason, and a
  run cancelled before it dispatched anything has nothing to refresh and reads
  nothing. The refresh is best effort: a run stops for a reason the wave
  produced, and a failed re-read is not that reason, so the pre-wave values
  stand rather than replacing it
  (`a_rejected_submission_stops_the_run_carrying_its_diagnostic`,
  `a_run_cancelled_after_a_wave_still_reports_what_the_wave_did`).
- **The foreground never dispatches a share that requires background
  tracking.** A share a helper accepted needs confirmation polling; an
  ambiguous or in-flight attempt needs duplicate-safe reconciliation and
  possibly replenishment. Both are filtered out of the candidate stream
  whatever else the plan lists. Plan order can put one ahead of a share no
  helper has reached, and a pending poll promotes the step it named, so leaving
  either selectable lets background work starve the delivery the round
  actually owes — indefinitely, until the dispatch budget runs out
  (`an_accepted_share_never_outranks_the_delivery_the_round_owes`,
  `an_outcome_unknown_share_never_outranks_the_delivery_the_round_owes`).
- **A host-configured value cannot crash the host.** `pending_repoll` is
  unbounded, so an absolute deadline built from it may not be representable;
  the wait treats that as "until interrupted" rather than panicking on the
  addition, as the chain client's own re-poll wait already did
  (`an_unbounded_repoll_waits_instead_of_overflowing`).
- **A step callback is a boundary too.** `StepFinished` and `StepFailed` run
  host code, so a cancellation or an epoch switch can arrive during the fold. A
  wave that also produced a terminal or stalled outcome reports `Cancelled`
  rather than that outcome, which is what `RoundDriver::run` promises; the
  diagnostic is not lost, since `chain_outcomes` and `failures` carry it either
  way (`a_cancellation_raised_by_a_step_callback_outranks_the_wave_s_own_stop`).
  The post-wave refresh blocks on the database, so it is itself the last
  boundary before that return and is followed by its own check; `finish`
  touches nothing further, so the window closes rather than moving.
- **A report's fields say what they hold.** `chain_outcomes` is every chain
  outcome the run observed, tracking results included, not only terminal ones.
  `RoundDrivePolicy::pending_repoll` paces every unfinished obligation, not
  only chain tracking: a share that became tracking-owned after selection and a
  confirmed vote whose delivery waits on ambiguous attempts use the same delay,
  so it is helper retry latency as well. `delegations` is what the run
  *signed*, which is not what it submitted: a
  step cancelled between signing and building its chain request produces a
  bundle, and `SignedDelegationBundle` carries no submission state — its wire
  `status` is always `ready_for_submission`. The durable answer for a bundle is
  in the report's plan, whose `delegation_statuses` entry carries the phase,
  the transaction hash and whether the submission is terminal.
- **A dispatch belongs to the epoch its run captured.** The driver decides to
  dispatch, then plans, builds each host context and reads stored signing
  material before the step begins. A step that captured its own epoch on entry
  would adopt an epoch the host switched to across that gap and prove, persist
  or broadcast for a session already left. Driver dispatches therefore inherit
  the run's `entry_epoch` through `advance_step_in_epoch`, and stop at the
  step's first boundary instead
  (`a_dispatch_decided_in_an_earlier_epoch_is_cancelled_not_adopted`,
  `a_dispatch_in_the_run_s_own_epoch_still_runs`). `advance_step_in_epoch` is
  the only way to run a step, so there is no entry point that could capture a
  newer epoch as its own.
- **Stop-round failure isolation is strictly serial.** When
  `FailureIsolation::StopRound` is selected, the driver admits one step at a
  time so no later obligation can already be running when the first failure
  ends the run. `SkipBundle` uses bounded bundle concurrency.
- **Failure isolation is per bundle.** Under `SkipBundle` a failed obligation's
  bundle is skipped for the rest of the run and every other bundle keeps going;
  every failure is reported together at the end, each with the durable effects
  its step had already made.
- **A `Pending` step is re-polled only while its chain work is `Tracking`, or
  after chain confirmation while helper delivery awaits ambiguous attempts.**
  An episode that ended in recovery has already escalated to the exact tree
  once; re-polling it for the rest of the round would hide a stuck submission
  the host can retry later, so the run stops and names it. A confirmed outcome
  is not stalled chain recovery: the wait leads to a fresh plan whose share
  obligations continue helper tracking. The step a re-poll named is dispatched
  next whenever the refreshed plan still lists it, so a pending submission is
  not starved by a step that sorts earlier, and the run cannot poll forever:
  every dispatch, re-polls included, counts against `max_dispatches`.
- **A failure keeps everything its step already did.** Durable effects survive
  the failure that followed them: share deliveries that reached helpers, the
  chain outcome a step observed before failing on the helper work after it, and
  the delegation a `Delegate` step signed before it lost the chain
  (`a_signed_delegation_survives_the_failure_that_followed_it`).
  `RoundStepFailure::delegation` carries the last of those for the same reason
  `share_deliveries` carries the first.
  A step that confirms and then fails is reported in `chain_outcomes` exactly
  as a successful one is, because the run did observe it
  (`a_chain_outcome_survives_a_failure_that_followed_it`). The
  `bundle_index` on a failure record is *attribution*, not isolation:
  `StopRound` names the bundle too while suppressing nothing, so
  `RoundRunReport::skipped_bundles` is the authoritative list of what was
  actually skipped (`stop_round_ends_at_the_first_failure`).
- **A recorded failure outranks a healthy-looking handoff.** A run that failed
  and then finds only background share work left reports the failure, not
  `BackgroundShareWorkOnly`; the latter reads as "the timer finishes it" and
  would hide the failure. For the same reason a terminal chain disposition that
  carries no outcome is reported as a failure rather than as a finished round:
  the outcome is the only place the rejection survives.
- **The stop reason is decided from what the run can still dispatch, never
  from a round-wide flag.** A run stops when the plan lists no step it would
  admit: no step at all, or only `ConfirmShare` steps that require background
  tracking, or only steps on bundles a failure isolated. It reports, in
  this precedence: a recorded failure, then a persisted submission it cannot
  advance, then missing bundle setup, then an unfinished ballot, then the
  background share handoff, and only then `NoWorkLeft`. Anything the host must
  act on outranks a handoff that asks nothing of it.

  `RoundPlan::blocking_recovery` is deliberately not the predicate. It is a
  property of the whole round, so it stays true for a terminal submission that
  plans no step at all and for a step on a skipped bundle. Reading it as
  "foreground work remains" made a round whose only remaining steps were shares
  a helper already held poll them for the entire dispatch budget and then
  report `PassBudgetExhausted` — an invariant-level event — in place of the
  rejection or the failure the host had to act on. Once nothing dispatchable is
  left, `blocking_recovery` means exactly "durable submission state this run
  cannot advance", which is why it maps to `PersistedChainTerminal` there and
  nowhere else
  (`a_terminal_submission_outranks_shares_the_timer_would_finish`,
  `a_skipped_bundles_own_work_does_not_keep_the_run_dispatching`,
  `a_recorded_failure_outranks_every_healthy_handoff`,
  `an_open_ballot_outranks_the_share_handoff`,
  `bundle_setup_outranks_the_ballot_it_blocks`,
  `an_empty_plan_with_nothing_owed_is_no_work_left`).

  Both halves of that question are asked **per share, of its own obligation**,
  and only of steps this run would admit. `blocking_share_work` is round-wide
  too, so it stayed true for an undelivered share on a bundle a failure had
  isolated, and polled the healthy bundles' tracking-owned shares for the same
  budget. A share no helper has reached is delivered rather than polled and is
  foreground work; a share some helper accepted or may hold can only be
  finished safely by the host's background tracking
  (`an_undelivered_share_is_foreground_work`,
  `an_undelivered_share_on_a_skipped_bundle_does_not_hold_the_run_open`).
- **The dispatch budget is evaluated against a fresh plan.** After the final
  admitted dispatch or wave, the driver re-plans, refreshes the tally, and
  prefers natural quiescence when the work completed. If work remains,
  `PassBudgetExhausted::remaining`, `RoundRunReport::plan`, and the tally all
  describe that same read. A zero budget still performs and reports one plan.
- **Each admitted bundle is judged by its own signer context.** The host source
  is sampled once per dispatch and nothing requires two samples to agree, so no
  single mode stands for the wave. A bundle whose own context signs during its
  step is owed nothing; the stored-material requirement falls only on bundles
  that read it, plus the ones this wave has not reached. Collapsing the modes
  either way is wrong: taking the first context would broadcast a bundle under
  a signer the host had stopped offering, and applying one bundle's stored
  requirement to all would demand a durable row for a bundle that signs itself
  — a handoff the host can never satisfy, because there is nothing to store.
  A step with no `DelegationStepInputs` at all is owed whatever is stored,
  since a durable row cannot make a step run that has no driver, and it does
  not condemn a bundle whose row already exists
  (`each_bundle_is_judged_by_its_own_signer_context`,
  `a_bundle_that_cannot_sign_does_not_condemn_one_that_already_has`).

  The driver can only know the mode of bundles it has admitted, and it takes
  those as speaking for the round. That is a **contract on the host**, stated
  on `RoundHostSource`: a context names no bundle, so repeated calls cannot
  attribute their answers, and an implementation must offer the same signer
  mode for every bundle of a round. The round-wide handoff exists for the
  Keystone device flow, where the voter signs every bundle before any is
  broadcast, and that flow is uniform by construction. A source that answered
  with a stored signer for one bundle and a self-signing mode for another it
  had not yet been asked about would be told to store a signature for a bundle
  that never needed one, and the run would not progress until it did; the API
  cannot detect this, because learning an un-dispatched bundle's mode would
  require dispatching it.
- **Missing stored Keystone signatures are a host handoff.** Before admitting
  signer-requiring bundle work, the driver verifies that **every bundle the
  round still owes a delegation for** has a durable signature row — not only
  the ones the current wave would run. A wave is bounded by the concurrency
  limit, so checking its members alone would prove and broadcast the signed
  bundles and report the unsigned ones a wave later: the voter would sign in
  several device rounds, and delegations would already be on the wire before
  the first of them. Absence yields `NeedsDelegationSignatures` before anything
  is dispatched; malformed stored material remains an executor failure.
- **Progress is exact, and measured against a baseline the host selects.** A
  proposal is complete when no `Cast` and no `ReconcileChain` obligation covers
  it **and it is not one of the plan's `withheld_casts`** — the rostered choices
  that still owe a cast this pass could not draw up, because the ballot is not
  yet terminal, the bundle is held by a vote already on the wire, the round has
  no bundle rows at all, or the choice's undispatched batch is still waiting on
  a member the ballot has not decided. Those choices own no obligation, so
  absence alone does not mean done: a ballot recorded before bundle setup — the
  supported ordering — would otherwise read as fully complete beside a
  `NeedsBundleSetup` quiescence and no vote at all, and a decided member of a
  held batch would read as complete before the batch was sent and then regress
  once deciding the rest of it produced the `ReconcileChain`
  (`a_ballot_recorded_before_bundle_setup_completes_nothing`,
  `a_withheld_cast_is_not_a_completed_selected_choice`,
  `a_decided_member_of_a_held_batch_is_not_a_completed_selected_choice`). Obligation membership
  names every member of an atomic batch, which a host
  counting `NextStep`s cannot see: a batch projects to one `AdvanceVoteBatch`
  carrying only its first member's id, so a host counting steps reads a
  six-proposal batch as one question.

  `remaining_obligations` counts only what this layer can execute: `Blocked`
  and `Retire` are both excluded, because neither is ever dispatched on its own
  and a `Retire` without a surviving `Cast` would otherwise report work owed
  beside a `NoWorkLeft` quiescence
  (`a_retire_is_not_work_the_tally_reports_as_owed`).

  `RoundDrivePolicy::progress_baseline` chooses only what the *total* counts.
  Both baselines are captured from the run's first plan and share the same
  completion measure.

  - `ProgressBaseline::Run` (the default, and the historical behavior) counts
    the vote work that first plan owed. A round resumed with two questions left
    reports a total of two.
  - `ProgressBaseline::SelectedChoices` counts every durable selected choice
    whose vote belongs to the current roster or chain lifecycle, read from
    `RoundObligations::choice_proposals` together with
    `lifecycle_owned_choices`.
    It is the baseline that can hold a choice no obligation names, which is why
    completion reads `withheld_casts`; a run baseline holds only what its first
    plan already owed.
    Skipped proposals are excluded because they owe no vote submission.
    A choice whose proposal left the roster after its vote reached the chain is
    **kept**: the host cannot clear that intent and its work deliberately
    outlives the roster change, so it is in neither `choice_proposals` nor the
    clearable `unrostered_intents` and has to be named separately. Dropping it
    would move the selected-choice total and hide a vote still on the wire. A clearable
    unrostered intent is not kept — the host resolves it and any recast is
    planned fresh — and neither is a vote with no durable choice at all, which
    the wallet drives to resolution but the voter did not select. With
    unchanged selections and roster, the same resume reports the same
    selected-choice total.

  The choice belongs to the host because it depends on what the host's progress
  label claims to be counting, which the driver cannot know.
- **Every event names the step it came from.** `RoundStepProgress::ChainOutcome`
  and `TreeSynced` carry no subject of their own, so a run that interleaves
  bundles must attribute them or a host will misread per-bundle progress.

The host-facing projections of a run live in `wire` beside the plan and step
views, in the same flat serde-stable shape, and are built only from a report
the driver produced. They are a projection, never a second source of truth.
The projection must be *total*: a cross-language binding sees what a native
caller sees, so every field of `RoundRunReport` — the signed delegation bundles
included — reaches `RoundRunReportView`. Dropping one silently gives two
different answers to "what did this run do" depending on which side of the
boundary asked.

## Required conformance coverage

Conformance is demonstrated by behavior. Tests cover:

### Classification (pure, no database)

- every `VotePhase` maps to exactly one lifecycle position (compile-checked by
  an exhaustive match) and every table row above yields the stated
  obligation;
- a batch with a departed member is retired whole and its rostered members
  are recast; no obligation ever names a subset of a batch;
- an on-wire or confirmed unit for a proposal outside the roster, or without
  an intent, still yields `ReconcileChain` or `Deliver`;
- a conflicting intent on anything past `Committed` is an invariant
  violation;
- an unrostered intent covered by a lifecycle-owned unit is neither reported
  nor blocks casting; one that is not covered blocks casting until cleared;
- a cast is `Blocked` while a proposal is open or an unrostered intent is
  clearable, and plans nothing while the bundle is held by a live committed,
  on-wire or hashless unit or a managed or terminal delegation;
- mixed-phase batches, a vote claimed by two batches, a missing batch member,
  and conflicting batch hashes are invariant violations with the existing
  messages;
- `Deliver` is owed for missing shares; every submitted, unconfirmed share is
  `Confirm`, carrying whether it has an acceptance or outcome-unknown attempt.

### Snapshot

- the loader runs inside one deferred read transaction and never takes the
  connection mutex itself (a contending writer thread does not deadlock);
- the snapshot's phases, hashes, intents and plan presence equal what the
  per-call readers returned for every existing planner fixture.

### Projection and plans

- every existing `resume_plan` test in `round_planning/tests/resume_plan/`
  passes unchanged: step order, flags, and `InvalidInput` messages are byte
  for byte what they were;
- each `NextStep` resolves to its obligation; a `CastVote` for one proposal
  executes the bundle's full draft set without rescanning; a `ConfirmShare`
  with neither acceptance nor outcome-unknown evidence resolves to delivery;
  a stale step resolves to no work.

### Designation

- the designated vote's own plan writes the designation in its transaction
  and every later plan reads it
  (`the_designated_votes_own_plan_writes_the_designation_and_every_plan_reads_it`);
- a designation survives its proposal leaving the roster
  (`a_persisted_immediate_designation_survives_its_proposal_leaving_the_roster`)
  and a lower choice recorded afterwards, and submission reads the row rather
  than re-deriving
  (`a_later_lower_choice_does_not_move_the_designation_or_block_its_submission`);
- a batch every member of which left the roster is retired once and reported
  whole (`a_batch_whose_every_member_left_the_roster_is_retired_once_and_recast_from_nothing`);
- a designation is voided with the undispatched generation it was made for
  and is not voided by confirmation
  (`the_designation_is_voided_with_its_undispatched_generation_but_not_by_confirmation`);
- a version 19 sidecar with a marked plan backfills exactly one immutable
  designation row (`v19_immediate_markers_backfill_to_v20`).

### Round driving

- an undecided round stops for the ballot rather than reporting the round
  finished, and a fully skipped ballot stops with nothing left to do
  (`a_round_the_voter_has_not_decided_stops_for_the_ballot`,
  `a_fully_skipped_ballot_stops_with_no_work_left`);
- a partly decided ballot still runs the bundle's delegation prerequisite,
  because the planner lists it while the voter decides the rest of the roster
  (`a_partly_decided_ballot_still_runs_the_delegation_prerequisite`);
- a delegation obligation with no signing material stops the run naming its
  bundles, before anything is dispatched
  (`a_delegation_step_without_signing_material_stops_naming_its_bundles`,
  `a_missing_stored_keystone_signature_stops_for_the_host`,
  `stored_keystone_handoff_names_only_unsigned_bundles`), and the handoff names
  every unsigned bundle before anything is dispatched, including one outside
  the first wave
  (`every_unsigned_bundle_is_named_before_anything_is_dispatched`,
  `the_handoff_names_every_unsigned_bundle_not_only_one_wave`);
- a choice recorded before bundle persistence stops for bundle setup, and a
  persisted rejected or hashless submission stops for manual handling rather
  than reporting success
  (`a_choice_without_bundles_stops_for_bundle_setup`,
  `a_rejected_submission_stops_the_run_carrying_its_diagnostic`,
  `a_persisted_hashless_submission_requires_manual_handling`);
- cancellation observed before the first plan stops the run without reading one
  (`a_cancelled_control_stops_before_the_first_plan`), and the dispatch budget
  reports a fresh plan that still has work while natural quiescence wins after
  a successful final dispatch
  (`the_dispatch_budget_stops_a_plan_that_never_shrinks`,
  `a_final_allowed_dispatch_refreshes_before_deciding_quiescence`);
- the tally counts every chosen proposal the run starts owing, and a skipped
  proposal is not a question to complete
  (`the_tally_counts_every_chosen_proposal_the_run_starts_owing`,
  `a_skipped_proposal_is_not_a_question_to_complete`);
- a failed bundle is skipped and the rest of the round still runs, every
  failure names the bundle it isolated, and the skip is reported as it happens
  (`a_failed_bundle_is_skipped_and_the_rest_of_the_round_runs`,
  `a_skipped_bundle_is_reported_as_it_happens`); `StopRound` instead ends at
  the first failure and isolates nothing
  (`stop_round_ends_at_the_first_failure`);
- the re-poll wait is polled rather than slept through, so cancellation or a
  new operation epoch ends it immediately
  (`a_cancelled_host_does_not_pay_the_rest_of_the_repoll_wait`,
  `a_new_operation_epoch_ends_the_repoll_wait`,
  `the_repoll_wait_runs_to_completion_when_nothing_interrupts`);
- a tracking submission is polled again after that wait, and a rejected one
  stops the run carrying its diagnostic
  (`a_tracking_submission_is_polled_again_after_the_repoll_wait`,
  `a_rejected_submission_stops_the_run_carrying_its_diagnostic`); one stuck in
  recovery stops rather than being polled for the rest of the round
  (`a_submission_stuck_in_recovery_stops_instead_of_being_polled_forever`), and
  one that never confirms stops at the dispatch budget naming the work it left
  (`a_submission_that_never_confirms_stops_at_the_dispatch_budget`); confirmed
  chain work awaiting ambiguous helper attempts is replanned rather than
  reported as stalled recovery
  (`confirmed_chain_work_pending_on_helpers_is_replanned_not_stalled`);
- selection takes plan order, passes over an isolated bundle entirely, and
  prefers the step a re-poll named while the plan still lists it
  (`round_drive::tests::selection`);
- independent bundle-locked steps overlap only up to the configured limit and
  never overshoot the remaining dispatch budget
  (`bundle_steps_run_up_to_the_configured_limit`,
  `one_bundle_slot_keeps_bundle_steps_serial`,
  `dispatch_budget_is_not_overshot_by_concurrent_launches`);
- every step observation names its step, including the subjectless chain
  outcome, and a plan is reported before anything is dispatched
  (`every_step_observation_names_its_step`,
  `a_run_reports_its_plan_before_it_dispatches_anything`);
- an atomic batch counts every ordered member rather than its anchor, and
  progress is measured against what the run started owing
  (`a_batch_counts_every_ordered_member_not_just_its_anchor`,
  `progress_is_measured_against_what_the_run_started_owing`);
- the selected-choices baseline keeps its total across a resume, counts every
  member of an atomic batch, and excludes a skipped proposal, while the default
  baseline stays run-relative
  (`the_selected_choices_baseline_keeps_its_total_across_a_resume`,
  `the_selected_choices_baseline_counts_every_member_of_an_atomic_batch`,
  `a_skipped_proposal_is_not_a_selected_choice`,
  `selecting_a_baseline_does_not_disturb_a_round_both_agree_on`,
  `the_default_baseline_is_the_run_so_existing_hosts_are_unchanged`);
- a choice whose cast the plan could not draw up counts as owed, not as done,
  for a ballot recorded before bundle setup, for a cast withheld while the
  ballot is open, and for a decided member of a batch still waiting on the rest
  of itself
  (`a_ballot_recorded_before_bundle_setup_completes_nothing`,
  `a_withheld_cast_is_not_a_completed_selected_choice`,
  `a_held_batch_withholds_the_members_the_ballot_already_decided`,
  `a_decided_member_of_a_held_batch_is_not_a_completed_selected_choice`);
- the selected-choices baseline keeps a choice whose vote the chain lifecycle
  owns after its proposal left the roster, and drops one the host can still clear
  (`a_lifecycle_owned_unrostered_choice_stays_selected`,
  `a_clearable_unrostered_intent_is_not_a_selected_choice`,
  `the_selected_choices_baseline_holds_a_choice_the_chain_lifecycle_owns`);
- a share a helper accepted or may hold is left to the host's background
  tracking, and neither can outrank a later share the foreground can deliver
  (`a_share_a_helper_already_holds_is_left_to_background_tracking`,
  `an_accepted_share_never_outranks_the_delivery_the_round_owes`,
  `an_outcome_unknown_share_never_outranks_the_delivery_the_round_owes`), and
  the default policy is pinned
  (`the_default_policy_is_the_cadence_hosts_were_driving_by_hand`).

### Executor

- a failure after chain confirmation carries the chain outcome and the
  deliveries that succeeded, at every failure site;
- a bundle that persists its delegation proof after another bundle advanced
  the round marker to `VoteReady` keeps that later marker and commits the proof
  (`a_late_bundle_proof_preserves_vote_ready_round_phase`);
- the epoch and binding captured at entry are the ones a step uses after a
  long proof even if the host rebinds meanwhile;
- a step with an unresolved delegation prerequisite is refused before I/O;
- every step the projection emits resolves back to an obligation
  (`every_projected_step_resolves_to_the_obligation_it_came_from`), which is
  what makes an unresolvable listed step an invariant violation rather than a
  reachable state; a step the plan no longer lists is `NoWork` without network
  I/O (`empty_plan_and_stale_steps_return_no_work_without_network_io`);
- a resumed on-wire vote is reconciled with the chain before its helper plan
  is required, even while the ballot is not terminal.

## Reviewer checklist

- Does the change let anything select a step from a plan it did not read
  itself? An outcome's plan is a projection, not a control input.
- Does a new driver decision belong to classification instead? The driver
  schedules; it does not decide what work a round owes.
- Does the change add a second place that decides whether a unit is still
  the wallet's to plan? It must use the lifecycle position instead.
- Does any code path treat a batch member on its own?
- Does the planner read outside the snapshot, or does the snapshot loader
  take the connection mutex?
- Does an obligation lack something its execution then goes looking for?
- Is a fact the planner gated on re-verified by the act's own transaction or
  lock?
- Is the immediate-share designation read from its row, and never
  re-derived once a row exists?
- Does a new failure constructor take the step ledger?
- Is the corresponding conformance test named in this document?


### Live combined recovery conformance

The `recovery-conformance` matrices exercise fresh combined rounds. Their scoped
snapshots require the complete authorization and ordered membership, helper plans
before a fresh POST, and the final VAN plus every contiguous vote position at
confirmation. Crash resumes preserve already-persisted PCZT/proof fingerprints,
authorization, and batch membership. A target-only round-driver pass removes the
voter mnemonic and supplies no signing inputs before normal round completion.

`recovery-conformance/tests/combined_recovery.rs` rejects missing authorization,
partial membership or confirmation, wrong generation metadata, missing helper
plans, and evidence belonging to another wallet. The combined POST fault wrapper
is exercised on both sides of dispatch by
`combined_post_stalls_on_the_selected_side_of_dispatch`. Live tree-read stalls
start from a real hashless dispatch because a fresh combined cast does not need
the initial tree synchronization of a standalone delegation.

Early delegation preparation forwards the selection, PCZT and proof progress
sequence through the supplied reporter. In particular, `PcztBuilding` precedes
the PCZT write and `PcztBuilt` follows it; the fresh combined preparation path
must not replace this reporter with a no-op. The live `after-note-selection`
and `after-pczt` stages depend on those boundaries. The hermetic
`proof_preparation_reports_selection_before_a_preparation_failure` test also
pins the start event when preparation fails before setup.
