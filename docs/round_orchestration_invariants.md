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
- the round-wide immediate-share designation as durable state.

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
| Undispatched | Rostered | Unrecorded | none; the unit holds its bundle |
| Undispatched | Rostered | Conflicts | singleton: none, and it holds nothing; the cast pass recasts and the persisted cast replaces the row. Batch: invariant violation, since the intent write path clears an unsubmitted batch whole |
| Undispatched | LeftRoster | any | `Retire` for the whole unit; the cast pass recasts the rostered members |
| OnWire | any | Agrees, Unrecorded | `ReconcileChain` |
| OnWire | any | Conflicts | invariant violation |
| Terminal | any | Agrees, Unrecorded | none |
| Terminal | any | Conflicts | invariant violation |
| Confirmed | any | Agrees, Unrecorded | per member: `Deliver` for missing or not-yet-accepted shares, `Confirm` for accepted `Submitted` shares |
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

All drafts for one bundle form one `Cast` obligation and are cast as one
atomic batch (or one singleton when there is one draft).

### Delegation obligations

- `Delegate` is planned for a bundle whose delegation is `Prepared`,
  `PcztBuilt` or `Proved` whenever that bundle has a `Cast` or a
  `Blocked` cast, so the prerequisite is visible while the voter decides the
  rest of the roster.
- `AdvanceDelegation` is planned for `Submitted` and `SubmissionManaged`
  delegations; an imported capability advances without a signer.
- `SubmittedWithoutHash` and `SubmissionRejected` delegations plan nothing and
  block their bundle's casts.
- Every vote or share obligation on a bundle whose delegation is not
  `Confirmed` carries that bundle's delegation step as its **prerequisite**.

### Share obligations

- A confirmed vote's expected share indexes come from its recovery bundle.
  Indexes with no share row are `Deliver` work.
- A `Submitted` share row is `Confirm` work. It blocks the foreground while
  no helper has accepted it (`sent_to_urls` empty). Dispatch delivers such a
  row again from its durable plan only when no helper has reached it at all
  (no accepted, ambiguous, or in-flight helper): no helper holds it, so
  polling cannot confirm it. A row with an ambiguous or in-flight attempt is
  polled: redelivery excludes those helpers and only tracking can classify
  them
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
  computed from the obligations and the snapshot only.

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
- **One completion path.** Fresh casts and resumed units go through one
  completion routine that differs only in when helper plans are made
  durable; there is no second driver.
- **Prerequisites are refused at dispatch.** A step whose obligation carries
  a delegation prerequisite fails with `InvalidInput` naming it, before any
  I/O.

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
- `Deliver` is owed for missing and unaccepted shares, `Confirm` only for
  accepted ones.

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
  for an unaccepted share resolves to delivery; a stale step resolves to
  no work.

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

### Executor

- a failure after chain confirmation carries the chain outcome and the
  deliveries that succeeded, at every failure site;
- the epoch and binding captured at entry are the ones a step uses after a
  long proof even if the host rebinds meanwhile;
- a step with an unresolved delegation prerequisite is refused before I/O;
- a resumed on-wire vote is reconciled with the chain before its helper plan
  is required, even while the ballot is not terminal.

## Reviewer checklist

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
