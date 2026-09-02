# Chain submission implementation plan

## Purpose

This document sequences the implementation of the chain-submission state
machine specified by [`chain_submission_invariants.md`](chain_submission_invariants.md).
The work moves transaction submission, reconciliation, and confirmation from
Vizor into the SDK while keeping transport injection simple for clients that
need Tor.

The sequence is intentionally additive. Each phase must compile, pass its own
tests, and leave the repository in a usable state. Database migration and
legacy API removal occur near the end so that the state model and protocol can
be reviewed before they become authoritative over existing wallet data.

This plan does not supersede the invariant specification. If implementation
work reveals a conflict, update the specification and its cited regression
tests in the same change rather than silently weakening an invariant.

## Goals

- Give the SDK one authoritative chain-submission lifecycle for delegation and
  singleton vote transactions.
- Make each public operation an idempotent, bounded advancement of that
  lifecycle rather than a long-running background loop.
- Let the SDK own endpoint construction, request and response encoding,
  timeouts, retry eligibility, polling, event interpretation, and durable
  transitions.
- Let the host provide an HTTP transport so Vizor can route requests through
  Tor without reimplementing protocol behavior.
- Reserve durable intent before a POST and never blindly repeat an ambiguously
  dispatched POST.
- Preserve existing domain projections and helper-share advancement when a
  transaction is confirmed.
- Keep exact commitment-tree recovery available as a secondary reconciliation
  path after the core state machine is established.
- Make the Rust API and module layout easy to discover and model from the
  facade alone.

## Initial scope and deferred work

The first coordinator implementation covers delegation and singleton vote
submission, matching current Vizor behavior. Batch vote submission and exact
tree recovery follow after the normal transaction-status path and durable state
machine are proven, but both land before the version-18 lifecycle becomes a
production authority or Vizor cuts over. This avoids activating a lifecycle
that can create sticky `Recovering` rows without a recovery path or disabling
the SDK's existing atomic-batch workflow before its replacement is available.

The initial work does not introduce an SDK-owned background runtime. Hosts
decide when to call an advancement method and may schedule another call when a
pending result is returned. The SDK performs at most one bounded pass per call:
it may submit an eligible transaction and then perform the immediately useful
status check, but it does not sleep or poll indefinitely.

Intermediate additive phases may temporarily leave legacy APIs available for
compatibility. They must not create two authoritative writers after the new
lifecycle is activated. Version 18 remains unreleased while the SQLite store,
exact recovery, and batch coordinator are assembled; production activation and
cutover establish the new authority only after all three are complete. Legacy
mutation APIs are then removed in a later compatibility phase.

## Target architecture

### Module boundary

Introduce a thin public `chain_submission` facade with private child modules
grouped by responsibility:

```text
chain_submission/
  mod.rs                 public API, durable vocabulary, and black-box contract
  state.rs               pure lifecycle transitions and invariants
  transport.rs           host transport contract and response limits
  protocol.rs            chain endpoint and wire-protocol interpretation
  coordinator.rs         one bounded advancement pass
  generation.rs          semantic generation and transaction construction
  confirmation.rs        atomic domain projection on confirmation
  recovery.rs            exact tree matching, added in a later phase
  store.rs               private persistence contract
```

Names may be refined during implementation, but the facade must remain small,
children should be private unless a client genuinely needs their types, and
dependencies should flow from data and pure transitions toward the
coordinator. There must be one authoritative representation of submission
state rather than parallel state in client, protocol, and storage layers.

### Durable lifecycle

The six durable states and their creation/transition edges are:

```text
new -> Submitting
Submitting -> Tracking | Recovering | Rejected
Tracking -> Tracking | Recovering | Confirmed | Rejected
Recovering -> Recovering | Confirmed
migration -> LegacyConfirmed
```

`Submitting` means intent has been durably reserved before POST. `Tracking`
means a candidate transaction hash is known and status reconciliation is
authoritative. `Recovering` means ordinary candidate polling is no longer
sufficient because a request may have been dispatched without a usable hash or
the bounded tracking window expired inconclusively; candidate-first
reconciliation and exact tree matching remain available for bound generations.
A digestless migration guard is also `Recovering`, but reports
`RecoveryUnavailable`, remains permanently unbound, and performs no network
work. `Recovering` is sticky: later status responses, retries, rejection, or
cancellation cannot move it to `Tracking` or `Rejected`. `Confirmed` and
`Rejected` are terminal for a semantic generation. Migration may also create
terminal `LegacyConfirmed` records when version-17 confirmation is known but
its semantic generation cannot be reconstructed. They have no digest,
candidate hash, attempts, or network transitions, preserve only
legacy-observed positions, and block a fresh submission for the matching
legacy identity. Incomplete v17 chain evidence without derivation inputs
similarly becomes a permanent digestless `Recovering` guard.

The pure transition layer should accept typed observations instead of HTTP or
SQLite values. It must reject illegal transitions and enforce required data,
including the presence of a candidate hash in `Tracking` and the preservation
of the recovery boundary in `Recovering`.

### Public client shape

The exact Rust spelling can evolve while implementing, but the facade should
converge on a small client-oriented API of this form:

```rust
pub struct ChainSubmissionClient<T> { /* private */ }

impl<T: ChainTransport> ChainSubmissionClient<T> {
    pub async fn advance_delegation(
        &self,
        request: AdvanceDelegation,
    ) -> Result<ChainSubmissionResult, ChainSubmissionFailure>;

    pub async fn advance_vote(
        &self,
        request: AdvanceVote,
    ) -> Result<ChainSubmissionResult, ChainSubmissionFailure>;
}
```

Inputs identify the wallet and round context needed to derive a semantic
generation; callers do not supply or mutate lifecycle state. A successful
call returns one of:

- `Confirmed`, representing atomically durable confirmation and exposing a
  confirming transaction hash only when confirmation source is `hash`; tree
  confirmation never synthesizes or promotes a candidate hash, while
  `LegacyConfirmed` returns the distinct source `legacy_projection`,
  legacy-observed positions, and no transaction hash;
- `Pending`, including the public pending state, optional candidate hash, and
  a diagnostic suitable for scheduling or display; digestless guards return
  `RecoveryUnavailable` and are not automatically rescheduled;
- `Rejected`, including the deterministic rejection diagnostic; or
- `Cancelled`, only when cancellation precedes possible dispatch and no
  stronger durable state exists.

A call already cancelled on entry still loads the authoritative row. If it
finds an abandoned `Submitting` row, it atomically normalizes the row to
`Recovering` and returns `Pending(Recovering)` without starting network work.

Operational failures remain distinct from durable outcomes. An error carries
the strongest truthful lifecycle state already durable or known to the call so
callers never need to infer persistence by reading an error string.

### Transport boundary

The host-supplied transport is an HTTP mechanism, not a chain protocol client:

```rust
pub trait ChainTransport: Send + Sync {
    fn chain_get(
        &self,
        request: ChainHttpRequest,
    ) -> ChainTransportFuture<'_>;

    fn chain_post_json(
        &self,
        request: ChainHttpRequest,
        json: Vec<u8>,
    ) -> ChainTransportFuture<'_>;

    fn chain_post_json_with_dispatch(
        &self,
        request: ChainHttpRequest,
        json: Vec<u8>,
        dispatch: ChainPostDispatch,
    ) -> ChainTransportFuture<'_>;
}
```

The SDK supplies the complete URL, headers, timeout, and response-size limit.
The transport returns status, content type, headers needed by the protocol,
and bounded response bytes. The SDK also applies its own outer deadline, so a
custom transport cannot make a lifecycle invocation unbounded by ignoring the
request metadata. Only the transport implementation may classify dispatch
certainty. The dispatch-aware POST marks the last definitely-unsent boundary
immediately before handing the request to a network stack; the default trait
implementation marks conservatively when an existing transport's POST future
is first polled. Transport failures must distinguish:

- `DefinitelyUnsent`: no request byte was released to a network stack that may
  deliver it; and
- `PossiblyDispatched`: any later failure, interruption, or cancellation.

The lifecycle decides retry eligibility from that classification and durable
state. It never infers that a request was definitely unsent from an HTTP status,
timeout, or missing response.

Vizor's Tor adapter implements this trait. The SDK's ordinary Hyper transport
also implements it, allowing clients with no custom routing requirements to
use the same state machine.

Cancellation is checked before releasing each request. Cancellation after any
POST byte crosses the dispatch boundary is treated as possibly dispatched.

### Semantic generations

A semantic generation identifies one immutable transaction meaning rather
than one API attempt. Its identity is exactly `(wallet, chain/network, round,
kind, bundle, proposal-or-batch)`; choices, commitments, proofs, signatures,
successor outputs, and recovery material belong only to the generation digest.
Retrying transport for the same meaning reuses the same generation. Changed
meaning may create a new generation only when no authoritative row exists or a
fresh definitely-unsent reservation was removed. Once possible dispatch exists
for an identity, a different generation digest is rejected.

Generation derivation, transaction construction, and durable reservation must
be explicit and testable. Concurrent calls for the same generation serialize
through the submission-identity lock and observe the same authoritative row.
That lock also prevents concurrent generations from racing for one identity.
Before requiring recovery inputs for generation derivation, the coordinator
checks every applicable legacy identity for a migration-only guard, including
every singleton member identity of a proposed batch. It returns
`LegacyConfirmed` directly for a singleton; a digestless `Recovering` guard
remains pending with `RecoveryUnavailable` and without network work, regardless
of later recovery inputs; and an overlapping batch is rejected without
dispatch.
For the supported v17 compatibility case, that key is
`(wallet, chain/network, round, vote, bundle, proposal)` and is eligible for
`LegacyConfirmed` only when no batch markers exist and both legacy VAN and VC
positions are recorded. Batch indicators are exactly any non-null batch field
in recovery JSON or one non-null historical hash shared by multiple vote rows.

### Confirmation and helper interaction

Confirmation writes the submission terminal state and the existing domain
projection atomically. For delegation this includes the VAN position and, for
hash confirmation, the transaction hash. For votes it includes the
vote-commitment position, recovery material, the transaction hash for hash
confirmation, and any helper-share advancement required by
[`helper_submission_invariants.md`](helper_submission_invariants.md). Tree
confirmation writes positions but no transaction hash.

Chain positions are parsed and stored as `u64` values constrained to SQLite's
signed range. Conversion to existing narrower domain APIs must be checked and
must not truncate. Helper scheduling, placement, retry, and delivery behavior
is unchanged except for two integration points: the existing confirmed-vote
transaction continues to advance compatible helper state atomically, and live
round cleanup must stop deleting generation-bound helper and recovery state.
The latter is an intentional cross-specification behavior change. A separate
prerequisite helper-share PR updates
[`helper_submission_invariants.md`](helper_submission_invariants.md), removes
the standalone recovery-clear behavior, and revises its cited regression tests
before the version-18 lifecycle is activated.

## Buildable implementation phases

### Phase 1: Pure state model and public vocabulary

Add the `chain_submission` facade, typed semantic generation identifiers,
durable state values, public result values, diagnostics, and a pure transition
function. Do not add networking or persistence.

The transition tests must cover every legal edge, every rejected edge,
terminal-state idempotence, sticky recovery, required candidate hashes, and
preservation of the ambiguous-dispatch recovery boundary. Phase 1 exposes no
recovery-retry transition: Phase 6 introduces retry authorization together with
the recovery pass, lifecycle lock scope, and atomic store operation needed to
make that capability valid. The vocabulary also includes migration-only
`LegacyConfirmed` and digestless `Recovering` guard constraints, although only
the migration store may create them.

Buildable outcome: the crate exports a reviewed lifecycle vocabulary and pure
state machine, but no production caller can submit a transaction through it.

### Phase 2: Chain protocol and transport seam

Add the `ChainTransport` contract, bounded HTTP request and response types, the
default Hyper implementation, and a private protocol client for:

- `POST /shielded-vote/v1/delegate-vote`;
- `POST /shielded-vote/v1/cast-vote`; and
- `GET /shielded-vote/v1/tx/{hash}`.

The SDK owns URL normalization, JSON encoding, response-size enforcement,
timeout values, success and deterministic rejection parsing, event parsing,
and retry eligibility. Redirects are never followed. Production chain and tree
endpoints require authenticated encryption, and a transport configured for a
privacy route must fail closed rather than falling back to a direct connection.
The transport alone classifies whether dispatch was definitely unsent. A
protocol response such as HTTP 422 is data, not a transport failure. The
implementation must not carry Vizor's current practice of retrying a POST after
an ambiguous timeout.

Use a scripted fake transport to test exact URLs and JSON, response limits,
malformed responses, non-JSON bodies, deterministic rejections, redirect
rejection, production-HTTPS enforcement, privacy-route failure without direct
fallback, definitely unsent failures, and ambiguous POST failures.

Buildable outcome: protocol behavior is usable and exhaustively testable
without SQLite or a live server; existing APIs remain unchanged.

### Phase 3: Semantic generation and confirmation refactor

Extract typed transaction-generation and confirmation operations from the
legacy delegation, vote, and confirmation entry points. Preserve the existing
wire encoding and Zcash transaction construction. Make confirmation accept a
typed confirmed observation and keep all domain projection writes in one
transaction. Implement batch grouping, ordered-membership validation,
generation derivation, and expected-layout derivation here even though public
batch submission remains disabled; Phase 5 migration depends on these pure
operations.

This phase is a refactor only: legacy entry points continue to delegate to the
same behavior and the new state machine is not yet authoritative. Add tests
showing that semantic generation is stable for identical meaning, changes when
meaning changes, and that event layouts and checked position conversions are
correct.

Buildable outcome: transaction construction and atomic confirmation are
cohesive internal services that both legacy and upcoming lifecycle code can
call without duplicating business rules.

### Phase 4: Lifecycle coordinator against a private store contract

Implement one bounded advancement pass over a private `ChainSubmissionStore`
interface and the transport/protocol layer. Begin with an in-memory test store;
do not migrate SQLite yet.

The coordinator must:

1. capture the wallet, chain, round, submission identity, and host operation
   epoch once, then acquire the account/round operation gate, the bundle lock
   when a VAN is consumed or advanced, every applicable submission-identity
   lock in canonical order, the database handle, and the immediate transaction
   in exactly that order;
2. load any matching migration-only guard, including every member legacy
   identity for a batch, without requiring generation recovery material;
3. return `LegacyConfirmed`, or keep a digestless `Recovering` guard
   permanently pending with `RecoveryUnavailable`; reject a batch overlapping
   either guard class without dispatch;
4. derive and lock the semantic generation when no unbound legacy guard exists;
5. load its authoritative record and conservatively normalize an abandoned
   `Submitting` row to `Recovering`, even when cancelled on entry;
6. reserve fresh intent before POST;
7. submit only when the transition model permits it;
8. durably classify every reserved POST before further network work or return:
   remove a fresh reservation after a definitely-unsent failure, persist a
   usable first hash as `Tracking`, and persist ambiguous dispatch as
   `Recovering`;
9. start the durable tracking window exactly once when first entering
   `Tracking`, never reset it through polling, diagnostics, timestamp updates,
   or restart, and atomically promote an inconclusive expired row to
   candidate-preserving `Recovering`;
10. record a returned recovery candidate without leaving `Recovering`;
11. poll known candidates and apply pending, confirmed, or rejected observations
    only through state-legal transitions;
12. re-derive the locked generation immediately before confirmation and apply
    confirmed domain projections atomically through the store boundary;
13. return a typed result after one bounded pass;
14. check cancellation and the captured operation epoch before reservation,
    dispatch, retry, lookup, each scan request, and the confirmation commit
    point; and
15. make confirmation persistence non-cancellable after that commit point.

Until Phase 6 activates tree recovery, `Recovering` always returns pending and
never retries POST. A same-generation recovery retry becomes eligible only
after candidate-first reconciliation and a completed bounded no-match tree
pass.

Concurrency tests must prove the exact lock order, single reservation, single
eligible POST, bundle serialization, idempotent advancement, serialization per
submission identity, rejection of a competing generation for that identity,
independent progress for unrelated bundles and identities, account-switch and
operation-epoch isolation, cancelled-entry normalization without network work,
and safe cancellation at every specified boundary. Reopen tests must prove
that the tracking-start timestamp and finite tracking deadline survive polling,
diagnostic updates, and process restart without extension.

Buildable outcome: the complete core lifecycle works in deterministic tests
without changing the on-disk schema or production API authority.

### Phase 5: SQLite v18 migration and inactive SDK integration

Add the version 17 to version 18 migration and the SQLite store implementation
described by the invariant specification. The migration creates the
authoritative `chain_submissions` table, indexes, constraints, and generation
identity needed by the coordinator. It must preserve current wallet data and
backfill only facts that can be represented without guessing. Its schema
permits a missing generation digest only for migration-only
`LegacyConfirmed` and `Recovering` guards; neither has a candidate hash or
network capability, and digestless `Recovering` guards remain permanently
unbound.

Expose `advance_delegation` and `advance_vote` from the SDK facade. Wire them to
the SQLite store and default or injected transport. Once v18 is authoritative
for a database, legacy APIs may remain source-compatible temporarily but must
delegate all chain-submission mutation through the lifecycle or reject it.
They cannot create or mutate a generation independently. Document that
compatibility boundary directly on those APIs. This phase may merge for
integration, but no SDK release containing the v18 migration and no downstream
production cutover occurs until Phases 6 and 7 are complete.

The separate helper-share prerequisite PR must land before this phase, so the
standalone recovery-clear API and storage primitive are already absent.
Ordinary cleanup and reset acquire the same operation and bundle locks and
preserve all live `Submitting`, `Tracking`, `Recovering`, `Confirmed`, and
`LegacyConfirmed` rows together with their generation-bound recovery material,
helper plans, and complete delivery history. Partial pruning refuses protected
ranges and never renumbers bundles. Explicit round or account deletion is the
only destructive escape hatch: it closes the applicable operation gate, blocks
new entrants, returns `Busy` while shared work remains, retains exclusive
access through deletion, and relies on foreign-key cascades to remove owned
recovery and helper rows. A rejected generation may release only its exact
unused recovery material, and only after proving that no earlier unresolved or
later dependent generation needs it.

Migration must import and protect singleton and atomic-batch state even though
the public batch coordinator remains disabled until Phase 7. Tests must cover
fresh and migrated schema equivalence, each specified v17 import class,
`LegacyConfirmed`, canonical-hash collision rollback and the valid complete
batch exception, partial batches, stale-v18 fingerprint rejection, transaction
rollback, and cleanup behavior. The valid v17 shape with recorded VAN and VC
positions but no `commitment_bundle_json` must preserve confirmation, block
dispatch, expose source `legacy_projection` but no invented digest, hash, or
validated layout, and survive ordinary cleanup. Partial positions or a
historical hash without derivation inputs must likewise remain protected by a
digestless `Recovering` guard. Tests also cover empty rows producing no guard,
every exact batch indicator, present-but-null batch fields as non-indicators,
and each position-ownership rule, plus unbound restart planning with no network
work and permanent guard exclusion of a competing native row or attempted
replacement. After complete recovery inputs are added, planning and advancement
must leave the migrated guard byte-for-byte unchanged with null digest, null
candidate, and zero attempts; insert no native row; and perform no network
call. A proposed batch containing any `LegacyConfirmed` or digestless guarded
singleton member must likewise dispatch nothing and insert no overlapping
batch row. Cleanup tests must prove preservation of every unresolved or legacy
guarded generation and all bound helper history, refusal to partially prune a
protected range, absence of any standalone clear primitive, and gated
round/account deletion that cannot race active work. They must also prove that
rejected-generation cleanup respects predecessor and successor dependencies.
Integration tests must reopen the database between every lifecycle edge to
prove durability.

Buildable outcome: the unreleased SDK has a durable restart-safe SQLite
integration for delegation and singleton submission, with migration, cleanup,
and deletion behavior ready for recovery and batch completion before release.

### Phase 6: Exact commitment-tree recovery

Add sticky bound-`Recovering` execution after normal status reconciliation
cannot safely determine the outcome. Digestless guards remain network-disabled.
Each pass selects one validated, fixed, complete, internally consistent tree
snapshot and scans it under the invariant specification's request, byte, leaf,
elapsed-time, and memory bounds.

Scan exact candidate layouts:

- delegation: `[delegation VAN]`;
- singleton vote: `[successor VAN, vote commitment]`.

Matches must be exact, ordered, and unambiguous. A match confirms through the
same atomic confirmation operation as transaction-status reconciliation.
No match, malformed input, cancellation, or exhaustion of a recovery bound
leaves the row in `Recovering`; none permits deterministic rejection of an
ambiguously dispatched request.

A valid complete no-match pass yields one private process-local authorization
while the captured generation, operation epoch, and lifecycle locks remain
valid. The store consumes it in one immediate transaction that retires any
inconclusive candidate and reserves one same-generation retry. There is no
standalone retirement mutation, and a hashless row cannot reconstruct or
replay authorization. Cancellation, error, return, lock release, and process
exit invalidate an unconsumed authorization.

A pending or unreadable candidate is polled first, remains available for later
polling, and does not prevent the subsequent tree scan. It blocks redispatch
unless the same immediate transaction consumes a valid complete no-match
authorization and retires it without classifying it as failed. A definitely
committed-failure recovery candidate is cleared while the row remains
`Recovering`, but a later retry still requires a new valid complete no-match
pass. A definitely-unsent failure after retry reservation retains the monotonic
reservation count, restores neither the candidate nor the consumed
authorization, and likewise requires a new complete pass. The attempt count is
diagnostic and never becomes a permanent cross-invocation retry gate.

Tests must cover malformed or inconsistent snapshots, exact and partial
matches, multiple matches, candidate-first polling, committed-failure clearing,
single-use authorization invalidation, failed atomic retirement-and-reservation,
definitely-unsent retry behavior, restarts, pruning boundaries, the complete
`2^24`-leaf traversal under all configured request, byte, elapsed-time, and
memory ceilings, and the prohibition on returning from `Recovering` to
`Tracking` or `Rejected`.

Buildable outcome: the SDK preserves the Android tree-recovery behavior as a
secondary path without complicating the ordinary submission state model.

### Phase 7: Batch vote submission

Activate batch protocol encoding, event interpretation, coordinator entry
points, confirmation, and recovery using the batch generation and layout
derivation introduced before Phase 5. Preserve the existing maximum batch size
and ordered commitment semantics. The exact recovery layout is
`[final successor VAN, ordered vote commitments]`.

Reuse the same coordinator rather than adding a batch-specific lifecycle.
Tests must cover one through the maximum number of actions, order sensitivity,
partial or reordered tree matches, atomic confirmation of every vote, and
restart/concurrency behavior. Batch admission must check every member's legacy
singleton identity under canonically ordered locks; any `LegacyConfirmed` or
digestless guard blocks the whole batch before derivation, insertion, or
dispatch.

Buildable outcome: delegation, singleton vote, and batch vote submission share
one state machine and transport model.

### Phase 8: SDK activation, Vizor cutover, and Tor client wiring

Release the version-18 SDK only after Phases 5 through 7 and the separate
helper-share prerequisite are complete. At this boundary the lifecycle becomes
the sole authority for delegation, singleton vote, and atomic-batch submission;
every remaining source-compatible legacy mutation API must delegate through it
or reject without mutation.

In the Vizor repository, implement a Tor-backed `ChainTransport` adapter using
the same FFI pattern as the existing helper transport. It must preserve the Tor
route, fail closed without direct-network fallback, and retain the transport's
definitely-unsent versus possibly-dispatched classification. Replace Vizor's
chain POST, retry, polling, recovery, and confirmation loops with bounded calls
to the SDK client. Map SDK results into UI scheduling and user-visible
diagnostics.

Vizor may initially invoke only delegation and singleton entry points, matching
its current behavior, but it cuts over to an SDK that already supports every
authoritative transaction shape and exact recovery. The Dart layer may choose
when to call again, but it must not construct chain endpoints, interpret chain
events, repeat ambiguous POSTs, perform independent tree recovery, or write
confirmation state.

Cross-boundary tests must cover Tor success and route failure without fallback,
cancellation, application restart between calls, pending scheduling, confirmed
and rejected results, exact recovery, and ambiguous dispatch without duplicate
POST.

Buildable outcome: the released SDK is the complete chain-submission authority,
and a Vizor build supplies only Tor transport, scheduling, cancellation, and UI
mapping. The Vizor change is released together with, or immediately after, the
SDK activation it depends on.

### Phase 9: Remove legacy APIs and compatibility state

Status: complete in this repository. Vizor pins the pre-removal revision, so
its `#[cfg(test)]` module and its
`rust/src/wallet/voting/README.md` responsibilities table need a follow-up when
that pin advances; no production Vizor code is affected.

After Vizor and other known clients have migrated, remove the legacy
submission and confirmation mutation APIs identified by the invariant
specification, including manual record/confirm paths that can bypass durable
reservation or atomic lifecycle transitions. Remove obsolete session steps,
FFI calls, database columns, and compatibility shims only after repository and
downstream searches show no remaining callers.

Any destructive schema cleanup should be its own migration and must preserve
the authoritative v18 lifecycle records. Legacy hash and position projection
columns cannot be removed while a `LegacyConfirmed` or digestless `Recovering`
guard depends on them, unless that migration first copies all required
evidence into a self-contained typed representation and updates the normative
specification. Update examples, generated bindings, and public documentation
in the same change. Add compile-fail or API-surface tests where practical to
ensure bypasses are not accidentally reintroduced.

Buildable outcome: there is one public chain-submission authority, one durable
state representation, and no supported path that can skip the state machine.

Capability-import follow-up: an already-broadcast delegation is represented by
the distinct `AdvanceImportedDelegation` request. Its first pass lazily adopts
the database-owned package hash into `Tracking`; later passes are signer-free
and status-only. They never POST, scan, or retry. Successful status validation
confirms atomically, while a committed failure becomes terminally rejected.

## Pull request and release strategy

Each phase should normally be one reviewable pull request or a short linear
stack when generated bindings require mechanical follow-up. Every PR must
build the full workspace and include the tests for its new boundary. Phases 5
through 7 form one unreleased integration sequence: shipping between them would
either expose two practical authorities or activate a lifecycle without exact
recovery or support for an existing transaction shape.

Recommended release gates are:

1. Phases 1 through 4 may merge as inactive foundations.
2. The separate helper-share cleanup PR lands before Phase 5.
3. Phases 5 through 7 merge and stabilize without an SDK release containing
   the version-18 migration.
4. Phase 8 produces the first released v18 SDK and cuts Vizor over without an
   intermediate production authority.
5. Phase 9 waits for a downstream caller audit and is released as an explicit
   compatibility change.

## Verification required throughout

In addition to phase-specific tests, every activated path must be exercised
against these invariant classes:

- durable reservation before POST;
- definitely-unsent versus possibly-dispatched transport outcomes;
- exact account/round, bundle, identity, database, and transaction lock order;
- captured wallet context and operation-epoch checks at every boundary;
- restart after every durable transition;
- candidate-hash polling without duplicate POST;
- an immutable durable tracking window and candidate-preserving expiry;
- sticky recovery;
- terminal-state idempotence;
- submission-identity concurrency and wallet lifecycle serialization;
- atomic submission state, domain projection, and helper-share advancement;
- cancellation on entry, before a request, and during ambiguous dispatch,
  including durable normalization of abandoned `Submitting`;
- response byte limits, redirect rejection, production HTTPS, privacy-route
  failure without fallback, and malformed protocol data;
- cleanup without deleting an active, recoverable, legacy-confirmed, or
  helper-bound record, plus gated exclusive round/account deletion; and
- compatibility migration from version 17.

The conformance test list in `chain_submission_invariants.md` remains the
authoritative acceptance checklist. Test names added during implementation
should be cited there as they become concrete.

## Completion criteria

The migration is complete when Vizor supplies only transport, scheduling,
cancellation, and UI mapping; the SDK exclusively owns chain protocol and
durable lifecycle decisions; delegation and all vote shapes use the same
coordinator; ambiguous POSTs cannot be blindly repeated; exact tree recovery
uses one validated complete snapshot; confirmation and live-round cleanup
preserve the required domain, recovery, and helper state atomically; and the
legacy mutation APIs have been removed.
