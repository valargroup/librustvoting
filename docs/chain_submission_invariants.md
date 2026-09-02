# Chain submission invariants

## Status and purpose

This document is the normative specification for vote-chain submission in
`zcash_voting`. Changes to the behavior described here must update this
document and its behavior-oriented conformance tests in the same change.

The design has one authoritative `chain_submissions` row for each semantic
generation, plus migration-only legacy guards for v17 chain evidence whose
generation cannot be reconstructed. A complete legacy confirmation uses
`LegacyConfirmed`; incomplete evidence uses a digestless `Recovering` guard.
It does not use an attempt journal or a durable scan workflow.

The normal path is:

```text
reserve Submitting -> POST -> store hash as Tracking -> poll -> Confirmed
```

If ordinary candidate polling can no longer safely determine the outcome, the
row becomes `Recovering`: either a request may have been dispatched without a
usable hash, or the bounded `Tracking` window expired inconclusively. Recovery
polls a candidate hash first when one exists, then may search the commitment
tree for the generation's exact output layout. `Recovering` is sticky: later
responses and retries may add a candidate hash, but they do not erase the
durable recovery ambiguity. Tree recovery never runs for ordinary `Tracking`.

## Scope and authority

The lifecycle covers delegation, singleton vote, and atomic vote-batch
submission. It owns:

- reservation before POST;
- transport classification, bounded retry, and failover;
- transaction-hash polling;
- trigger-gated commitment-tree recovery;
- atomic confirmation;
- restart planning and generation locking; and
- cleanup and deletion guards.

All public submission entry points are typed. Callers cannot submit arbitrary
JSON or directly record hashes, confirmation positions, or lifecycle states.
Domain hash and position columns are projections maintained by the lifecycle,
not competing sources of submission state.

Vote API and commitment-tree endpoints are trusted for chain status, events,
and validated snapshots. A malformed, incomplete, contradictory, or
unsupported response is not evidence. Routes that promise privacy fail closed,
mutation and lookup redirects are not followed, and production endpoints use
authenticated encryption.

One process exclusively owns a voting database. Operations capture wallet,
round, submission identity, and host operation epoch once; an account switch
cannot retarget in-flight work.

## Identity and semantic generation

A submission identity contains:

```text
(wallet, chain/network, round, kind, bundle, proposal-or-batch)
```

The migration-only legacy singleton identity is the recovery-independent key
`(wallet, chain/network, round, vote, bundle, proposal)`. `LegacyConfirmed` is
eligible only for a v17 vote with no batch markers and with both recorded
bundle VAN and vote-commitment positions. It is never inferred for delegation
or for an atomic batch. A row is batch-indicated exactly when recovery JSON
contains a non-null value for `batch_digest`, `batch_index`, or `batch_size`,
or when two or more vote rows share one non-null historical transaction hash.

`kind` is exactly one of:

- `delegation`;
- `vote`, with one proposal; or
- `vote_batch`, with one complete ordered batch digest.

The lifecycle derives a generation digest and expected output layout from the
locked durable bundle and vote recovery rows. The digest binds every semantic
input that can change the chain effect, including:

- identity and round;
- input nullifiers;
- delegation setup and VAN randomizer;
- proposal choices;
- batch membership and order;
- successor VAN and vote commitments; and
- the proofs, signatures, and recovery material that define those effects.

Confirmation-only hashes, positions, timestamps, diagnostics, and a
software-delegation SpendAuth signature are excluded. A restarted software
delegation may therefore be re-signed, but the lifecycle must verify the new
signature against the same locked semantic generation before dispatch.

No final signed request body is persisted. Every request is reconstructed as a
closed SDK wire type from locked durable inputs. The database stores neither a
payload digest nor duplicate descriptor JSON. The same derivation code is used
at reservation, retry, recovery, and confirmation so disagreement fails before
dispatch or persistence.

The expected tree layouts are:

```text
delegation: [delegation VAN]
vote:       [successor VAN, vote commitment]
batch N:    [final successor VAN, vote commitment 0, ..., vote commitment N-1]
```

Batch members use signed action order. Intermediate batch VANs are not tree
outputs.

## Authoritative durable record

There is exactly one `chain_submissions` row for a semantic generation. A
database uniqueness constraint covers its full identity and generation digest.
Creation and every durable state or field mutation occur in immediate
transactions. Migration-only legacy guards are instead keyed by the available
legacy identity, have no invented generation digest, and permanently block a
fresh submission for that identity. A partial unique index permits at most one
digestless guard per legacy identity, and reservation checks for that guard
under the submission-identity lock before deriving or inserting a generation.
The same transaction prohibits any digestless guard and native generation row
from coexisting for one submission identity.

The row stores only:

- the typed identity;
- generation digest, nullable only for migration-only `LegacyConfirmed` and
  digestless `Recovering` guards;
- durable state;
- one optional canonical candidate transaction hash;
- attempt count;
- an optional immutable tracking-start timestamp;
- a bounded redacted diagnostic;
- final confirmation source;
- final VAN and vote-commitment positions in generation order, or the
  unvalidated legacy domain positions for `LegacyConfirmed`; and
- creation and update timestamps.

Final ordered positions use a closed typed encoding, not descriptor JSON. All
positions are `u64` in the lifecycle and must fit SQLite's signed integer range.
Position zero is valid. Schema checks require a digest for every native row.
They require `LegacyConfirmed` to have source `legacy_projection`, both
observed positions, no candidate hash, attempts, or tracking-start timestamp,
and no recovery transitions. A digestless `Recovering` guard likewise has no
candidate, attempts, or tracking-start timestamp and cannot scan, retry, or
confirm unless complete recovery inputs first permit the same generation to be
derived and atomically bound to the row.

The row does not store:

- one row per POST or any attempt history;
- a final signed request;
- payload digests;
- recovery descriptor JSON;
- hash provenance or ownership classes;
- an outcome-precedence ladder;
- tree-scan cursors, epochs, pages, partial matches, or endpoint history; or
- a second recovery state machine.

Candidate hashes are canonical lowercase 32-byte hashes. A candidate is only a
handle to poll; it is not confirmation and has no durable provenance class.
Legacy guards do not import a candidate or confirmed hash; any historical hash
remains only in its unchanged legacy projection column.
Diagnostics are bounded, valid UTF-8, escaped, and redacted before storage.
Raw response bodies and sensitive cryptographic material are never persisted in
diagnostics or emitted through ordinary logging.

Domain transaction hashes and positions are written only by atomic
confirmation. After migration, runtime planning and status reads begin with
`chain_submissions`; legacy domain columns cannot override its state.

## Durable states

The only durable states are:

```text
Submitting
Tracking
Recovering
Confirmed
LegacyConfirmed
Rejected
```

Their meanings are:

- `Submitting`: the first POST is durably reserved, but its response has not
  been durably classified.
- `Tracking`: a usable candidate hash is known and no durable recovery
  ambiguity exists. Reconciliation only polls that hash.
- `Recovering`: ordinary candidate polling is no longer sufficient to resolve
  the outcome because a request may have been dispatched without a usable hash
  or the bounded tracking window expired inconclusively. An optional candidate
  hash is polled before tree recovery. Migration may also create a digestless
  guard when v17 contains incomplete chain evidence without derivation inputs;
  that guard performs no network work while unbound. The state is sticky until
  confirmation or explicit deletion.
- `Confirmed`: chain success and all required local confirmation updates are
  durable. New confirmations record `hash` or `tree` as their source and record
  the exact generation positions. Fully reconstructable migrated version-17
  confirmations record `legacy_import`.
- `LegacyConfirmed`: version 17 recorded the required domain confirmation
  positions, but lacks the recovery material needed to derive a generation
  digest or validate the exact output layout. It preserves the known
  confirmation with source `legacy_projection` without claiming either fact
  and permits no submission, reconciliation, or recovery.
- `Rejected`: the generation received a definite chain rejection, or its sole
  tracked hash is committed unsuccessfully.

The normal transitions are:

```text
new -> Submitting
Submitting -> Tracking       usable success hash
Submitting -> Recovering     possible dispatch without usable hash
Submitting -> Rejected       definite chain rejection
Tracking -> Tracking         hash still pending
Tracking -> Recovering       bounded tracking window expires inconclusively
Tracking -> Confirmed        committed success and atomic persistence
Tracking -> Rejected         committed failure
Recovering -> Recovering     candidate, retry, no match, or interruption
Recovering -> Confirmed      candidate success or exact tree layout
```

`Recovering` does not transition to `Tracking` or `Rejected`. In particular, a
later hash, rejection, committed-failure candidate, cancellation, or empty scan
cannot erase the original ambiguity. A pending or unreadable candidate is never
overwritten and blocks another POST. A definitively committed-failure candidate
is atomically cleared before a later same-generation retry may be reserved; the
row remains `Recovering` and tree recovery remains available.

Terminal rows are immutable except for idempotent replay of identical
confirmation data and explicit round or account deletion. `LegacyConfirmed` is
terminal, rejects every runtime transition and confirmation replay, and cannot
be promoted or replaced by a reconstructed generation. Only deterministic
reclassification during a retried atomic migration may recreate the same
marker. Conflicting terminal data is an invariant error and writes nothing.

## Reservation and transport classification

Before releasing any POST byte, the lifecycle:

1. acquires the round/account gate, bundle lock where applicable, and
   submission-identity lock;
2. derives the recovery-independent identity and loads its authoritative row;
3. returns `LegacyConfirmed`, or returns a digestless `Recovering` guard as
   pending when complete inputs cannot bind it, before requiring recovery
   material;
4. loads and locks the generation inputs and derives the generation digest and
   expected layout;
5. creates the `Submitting` row, or validates the existing same-generation
   `Recovering` row;
6. increments the attempt count for the request; and
7. commits the reservation.

Guard lookup, optional binding of a digestless `Recovering` guard, and native
row insertion share the same identity lock and immediate transaction, so a
concurrent call cannot bypass the guard.

If reservation fails, dispatch does not occur. A process-local in-flight guard
prevents cleanup, replacement, or deletion from racing response
classification.

Only transport code can classify a failure as `DefinitelyUnsent`, and only
before request bytes are released to a network stack that may deliver them.
Cancellation before that boundary is also definitely unsent.

For a first attempt, definitely-unsent failure removes the fresh `Submitting`
reservation; it does not create chain rejection or ambiguity. For a retry from
`Recovering`, it leaves the row `Recovering`.

Everything after the dispatch boundary is `PossiblyDispatched`, including:

- timeout or interruption;
- cancellation after dispatch;
- response-body or decoding failure;
- unusable or hashless success;
- HTTP 408 or 429 after POST;
- gateway or other ambiguous transport failure; and
- a process crash while a `Submitting` reservation exists.

Possible dispatch without a usable hash must durably transition `Submitting` to
`Recovering` before any scan or retry. On restart, an abandoned `Submitting`
row is conservatively changed to `Recovering`; the new process cannot prove
that request bytes were never released.

A canonical success hash transitions the first attempt to `Tracking`. If the
row was already `Recovering`, the hash becomes its candidate and the state
remains `Recovering`.

A definite chain rejection transitions `Submitting` to `Rejected`. A rejection
while already `Recovering` cannot settle the earlier possible dispatch. Its
code and log are diagnostic; if it also contains a canonical hash, that hash
may fill an absent recovery-candidate slot and is polled before the next tree
scan. It never overwrites a pending or unreadable candidate. Because the
protocol does not specify that an error hash identifies an earlier accepted
transaction, the hash is only a recovery handle.

POST attempts, endpoints, body sizes, request durations, and backoffs are
bounded by configuration with safe finite maxima. Redirects are not followed.
Retries are allowed only for the same semantic generation.

## Reconciliation and retry

One lifecycle facade provides typed delegation, vote, and batch entry points.
It owns submission, polling, recovery, and confirmation; planners do not
compose lower-level mutation APIs.

Reconciliation is state-driven:

- `Submitting` left by a crashed process becomes `Recovering`.
- `Tracking` polls its candidate hash and never scans the tree. If a configured
  finite tracking window expires without a definitive result, it atomically
  becomes `Recovering` while retaining the candidate hash.
- a bound `Recovering` row polls its candidate hash first. If hash polling does
  not confirm, the lifecycle may perform one bounded tree recovery pass.
- a digestless, unbound `Recovering` guard performs no network work and returns
  pending with `RecoveryUnavailable`.
- `Confirmed`, `LegacyConfirmed`, and `Rejected` perform no network mutation.

A pending or temporarily unreadable candidate remains available for later
polling. In `Recovering`, it does not permanently disable tree recovery, but it
does prohibit another POST. A committed-success candidate proceeds to
confirmation. A `Tracking` candidate that is committed unsuccessfully becomes
`Rejected`; a `Recovering` candidate that is committed unsuccessfully is
atomically cleared while the row remains `Recovering`.

After candidate polling and a bounded no-match tree pass, a bound `Recovering`
row may retry POST only for the same generation and only when its candidate
slot is empty. The retry reservation increments the row's attempt count while
preserving `Recovering`. A later usable hash fills the empty candidate slot,
but sticky recovery remains in force. An unbound guard is never eligible.

A pending or unreadable candidate is never treated as committed failure.
Endpoint disagreement, malformed responses, and temporary lookup failure
remain retryable diagnostics rather than terminal evidence.

The tracking window begins when the row first enters `Tracking`. Its start is
stored durably and never changes; candidate polling, diagnostics, restarts, and
timestamp maintenance cannot reset or extend the window.

Restart plans are derived from the authoritative row:

- `Tracking` schedules hash polling;
- bound `Recovering` schedules candidate-first reconciliation and tree
  recovery, then same-generation retry when permitted;
- unbound `Recovering` schedules no network work and reports
  `RecoveryUnavailable`;
- `Confirmed` enables dependent domain and helper work;
- `LegacyConfirmed` satisfies the chain-confirmation dependency and blocks
  resubmission, but missing helper inputs are not invented or scheduled;
- `Rejected` schedules no reconciliation; and
- absent rows permit fresh work if bundle causality allows it and no matching
  `LegacyConfirmed` identity exists.

An unresolved generation or legacy guard blocks only later work that consumes
its unknown successor VAN. Independent bundles remain schedulable.

## Sticky recovery and tree matching

Tree recovery is authorized only by a bound durable `Recovering` row. It never
runs for a digestless guard, ordinary `Tracking`, merely pending hashes, or
fresh unsubmitted work.

Before scanning, the lifecycle re-derives the generation digest and complete
expected layout from locked durable recovery rows. Missing or corrupt private
recovery material keeps the row `Recovering`, reports a stable bounded
diagnostic, and does not turn uncertainty into rejection.

Each recovery pass:

1. polls the current candidate hash, if any;
2. selects one fixed, complete, internally consistent tree snapshot whose
   validated metadata declares its final size;
3. scans that snapshot under per-request and whole-pass bounds;
4. compares leaves locally without transmitting expected commitments; and
5. accepts only one complete unique ordered layout.

The scanner validates snapshot identity, heights, roots, ranges, absolute
indexes, pagination progress, final size, canonical field encodings, and
response bounds. Recovery uses the following fixed ceilings:

- `16,777,216` leaves, the full `2^24` vote-commitment-tree capacity;
- `4,096` leaf-range requests of at most `4,096` leaves each;
- `8 MiB` per response and `32 GiB` across the complete pass;
- `60 seconds` per request and `72 hours` across the complete pass; and
- `16 MiB` working memory beyond the expected layout and transport buffers.

The tree's documented month-scale design point is approximately one million
leaves, so the leaf ceiling retains more than sixteen-fold headroom and also
covers every structurally valid tree. Responses are processed as a stream; the
complete tree is never retained in memory. Configuration may lower a
per-request range, byte, or timeout only when the derived request count, total
bytes, and worst-case elapsed time still fit the complete-pass ceilings. An
invalid combination is rejected before chain submission or recovery is
enabled. Before reading leaves, validated snapshot metadata must show that a
complete traversal fits. There is no smaller whole-pass work budget that can
repeatedly truncate a valid snapshot. Metadata claiming more than `2^24`
leaves is malformed and no leaf scan starts. Cancellation is checked between
requests and before the confirmation commit point.

Finding one member is insufficient. Singleton outputs must be adjacent and in
order. A batch must contain the final successor VAN followed immediately by
every vote commitment in signed action order. Partial, reordered, overlapping,
or independently located members do not confirm.

The entire selected snapshot is checked even after a match, so a second
complete match rejects the result. Duplicate or partial matches, malformed
pages, delayed indexing, cancellation, endpoint exhaustion, and transport
interruption leave the row `Recovering` with no partial position write. A
responsive endpoint serving a supported snapshot cannot repeatedly stop at a
local whole-pass budget: its complete traversal fits by construction.

Scanning is ephemeral. The next pass starts fresh. Tree recovery never
synthesizes a transaction hash.

## Confirmation and atomicity

Hash confirmation requires trusted committed success and a supported event
shape for the exact identity and generation. Tree confirmation requires one
complete unique layout as defined above.

Immediately before confirmation, the lifecycle reloads and re-derives the
locked generation. It rejects changed choice, membership, order, nullifier,
commitment, or generation digest.

One immediate transaction atomically:

- transitions the authoritative row to `Confirmed`;
- records confirmation source and exact final positions;
- records the transaction hash for hash confirmation;
- updates bundle VAN and vote-commitment positions;
- updates the exact vote/delegation recovery rows;
- advances compatible helper plans; and
- advances any domain phase or status projections.

For a batch, every member update and the final VAN advancement commit together
or none does. Tree confirmation writes no transaction hash. CheckTx acceptance
alone writes no domain hash, position, or helper confirmation.

Validation of trusted committed success or of one complete unique tree layout
is the confirmation commit point. Cancellation is checked immediately before
crossing it. After it is crossed, confirmation persistence is non-cancellable
and runs to commit or storage error so known success is never hidden.

If the atomic transaction fails, it writes nothing. The durable state remains
`Tracking` or `Recovering`, so later reconciliation repeats validation and the
idempotent transaction; the host must not create a different generation merely
because local persistence failed.

Reapplying identical confirmation data is a no-op success. Different hashes,
sources, or positions for a confirmed generation are invariant errors. An
older confirmation cannot rewind a bundle that has advanced through a later
confirmed generation.

## Public results

Public lifecycle results are intentionally small:

```text
Confirmed
Pending(Tracking)
Pending(Recovering)
Rejected
Cancelled
```

`Confirmed` means the authoritative row and all applicable atomic
domain/helper updates are durable. It exposes a transaction hash as confirmed
only when confirmation source is `hash`; a candidate retained by tree
confirmation is never presented as the confirming transaction.
`Pending(Tracking)` always carries the candidate hash needed to continue.
For a bound row, `Pending(Recovering)` may carry a candidate hash and preserves
that tree recovery remains authorized. For an unbound legacy guard it carries
no candidate, uses the stable `RecoveryUnavailable` diagnostic, authorizes no
network recovery, and must not be automatically rescheduled until derivation
inputs change. `Rejected` means the durable row is terminally rejected.

`LegacyConfirmed` is returned publicly as `Confirmed` with source
`legacy_projection` and no transaction hash. This source is distinct from the
validated `legacy_import` source. Its positions are explicitly legacy domain
observations, not a re-derived or validated generation layout; it does not
imply that missing helper inputs or plans were reconstructed.

`Cancelled` is returned only when cancellation occurs before possible dispatch
and no stronger durable state exists. Cancellation never hides `Tracking`,
`Recovering`, `Confirmed`, `LegacyConfirmed`, or `Rejected`. A call cancelled
on entry loads the authoritative durable state under the normal lifecycle
locks. If it finds an abandoned `Submitting` row, it must atomically normalize
that row to `Recovering` and return `Pending(Recovering)`; the possibly
dispatched request is stronger evidence than the current call's cancellation.
This conservative normalization is the only write permitted on a
cancelled-entry path. If it cannot be persisted, the call returns an
operational storage failure that preserves the known possibly-dispatched
state; it never returns `Cancelled`. The path starts no POST, lookup, scan,
retry, or confirmation write.

There are no public outcomes for accepted-but-unjournaled hashes, evidence
precedence, hash provenance, tree receipts, or unapplied confirmation. The
reservation and single-row transitions make those separate result classes
unnecessary. Storage failure is reported alongside the strongest truthful
state already durable or known to the current call without inventing a durable
transition.

## Concurrency, generation locking, and cancellation

The lock order is:

1. account/round operation gate;
2. bundle lock when a VAN is consumed or advanced;
3. submission-identity lock;
4. database handle; and
5. immediate SQLite transaction.

The identity lock serializes lifecycle work for one row. The bundle lock
prevents two proposals from deriving successors from the same VAN. Different
bundles remain independent.

Once a singleton request is possibly dispatched, its proposal and choice are
locked. Once a batch is possibly dispatched, member proposals, choices, count,
order, and batch digest are locked. Delegation setup, nullifiers, proof inputs,
and VAN randomizer are likewise locked. Re-selecting the same generation is
idempotent; changing it is rejected.

Cancellation is checked before reservation, dispatch, retries, lookups, scan
requests, and the confirmation commit point. It has the following safety
effects:

- on entry, an abandoned `Submitting` row is durably normalized to
  `Recovering` before a public result is produced;
- before dispatch by the current call, no request is released and a fresh
  reservation may be removed;
- after possible dispatch, the row is or becomes `Recovering`; and
- after the confirmation commit point, cancellation cannot suppress the atomic
  write.

The captured host operation epoch is checked at the same pre-commit boundaries.

## Cleanup, pruning, and deletion

Ordinary cleanup and reset use the authoritative state under the same operation
and bundle locks. They preserve every `Submitting`, `Tracking`, `Recovering`,
`Confirmed`, and `LegacyConfirmed` row. For native generations they preserve
all domain material required to reconcile, retry, prove, or apply that
generation, including:

- delegation setup, proofs, nullifiers, and VAN randomizer;
- vote and batch recovery material;
- locked proposal choices, batch membership, and order;
- current and generation-specific VAN/VC positions; and
- helper plans and durable helper-delivery rows bound to the generation.

For `LegacyConfirmed`, cleanup preserves the marker, its legacy identity and
observed positions, the original domain projections, and any independently
durable helper records that already exist. It neither requires missing
recovery material nor creates, regenerates, or advances a generation-bound
helper plan. A digestless `Recovering` guard likewise preserves its original
evidence and any independently durable records.

There is no standalone recovery-clear operation. The destructive
`clear_recovery_state` primitive may remove recovery material, helper plans,
and all helper-delivery history, but it is private to explicit account
deletion. Account deletion invokes it only after closing the account operation
gate, preventing new entrants, draining active work, and retaining exclusive
access through deletion. Ordinary cleanup, reset, and round deletion never
invoke this primitive.

An unresolved row cannot be pruned merely because it has no candidate hash.
Hashless `Recovering` is exactly the case that requires preservation. Bundle
indexes are never renumbered, and imported capability bundle sets remain
indivisible.

Rejected generations may release their exact unused recovery material only when
no earlier unresolved generation or later dependent generation needs it.
Cleanup must not infer safety from legacy domain hashes independently of the
authoritative row.

Explicit round or account deletion is the destructive escape hatch. It closes
the matching operation gate before checking active work, prevents new entrants,
drains shared holders, and retains exclusive access through deletion. It
returns `Busy` while work remains. Deletion removes local evidence but cannot
undo a transaction that may already be on chain. Round deletion removes the
gated round directly; only account deletion may invoke
`clear_recovery_state`.

## Version 17 to version 18

Version 17 is the released migration source. Version 18 is unreleased, so its
schema is replaced in place:

- the final `chain_submissions` definition is part of `17 -> 18`;
- `001_init.sql` creates the same schema;
- `CURRENT_VERSION` remains 18; and
- there is no `18 -> 19` migration.

Migration constructs at most one authoritative row for each semantic generation
after re-deriving identity and expected layout from the version-17 domain and
recovery rows. When that derivation is impossible for an otherwise complete
legacy confirmation, it instead constructs one `LegacyConfirmed` marker for
the available legacy identity. When derivation is impossible but incomplete
chain evidence remains, it constructs a digestless `Recovering` guard.

Migration classifies version-17 rows in this order:

1. Validate non-null recovery JSON and group every provable atomic batch.
   Missing JSON is absence, not corruption; malformed or internally
   inconsistent non-null JSON aborts migration.
2. A reconstructable generation with complete, internally consistent
   confirmation positions becomes `Confirmed` with source `legacy_import`,
   its available canonical hash, and its exact positions.
3. A legacy-singleton-eligible vote with complete VAN and VC domain positions
   but absent recovery JSON becomes `LegacyConfirmed`. Migration records the
   checked observed positions, preserves the original projection columns, and
   leaves its digest and candidate hash absent. A delegation, marked batch,
   or shared-hash group cannot enter this class.
4. A reconstructable unconfirmed generation with a canonical hash becomes
   `Tracking`. A reconstructable generation with an unusable historical hash
   or with committed recovery material but no hash becomes `Recovering`
   without a candidate.
5. Incomplete singleton chain evidence with absent recovery JSON becomes a
   digestless `Recovering` guard with no candidate. Its original columns remain
   intact, but it performs no polling, scanning, retry, or confirmation until
   complete inputs can atomically bind it to a derived generation.
6. Any remaining shape, including an unprovable batch or delegation that
   cannot be represented without guessing, aborts migration atomically.

A row has chain evidence when at least one transaction hash or confirmation
position is present, or when committed recovery material exists. An ordinary
vote row with none of those creates no submission row or guard.

Migration safety note: in version 17, a missing hash is not evidence that no
POST was dispatched. Likewise, a canonical historical hash is not imported as
a candidate when the generation needed to validate its result cannot be
derived. An advanced current bundle VAN is never attributed to the original
delegation output. A reconstructable delegation with that shape follows the
`Tracking` or `Recovering` rules above; a shape that cannot be represented
without guessing aborts migration atomically and preserves the version-17
database.

Migration validates complete unique ownership of canonical hashes imported
into authoritative candidate or confirmation fields across wallet,
chain/network, round, kind, bundle, and proposal or batch. The only allowed
shared hash is the complete ordered membership of one valid atomic batch.
Historical hashes retained only in legacy-guard projection columns are not
treated as candidates or ownership evidence. Every exact reconstructed output
position and every legacy-observed VC position must fit the allowed range and
belong to only one commitment; duplicate ownership aborts migration. A
`LegacyConfirmed` bundle VAN is only an unvalidated current-domain observation,
so it is range-checked but excluded from output-ownership collision checks.
Valid batch outputs remain distinct; the batch exception applies only to
shared hash ownership. Cross-generation collisions, partial batches,
inconsistent positions, or malformed recovery metadata abort the whole
migration; migration does not guess, merge, or silently normalize ownership.

`Confirmed` imports validate the complete expected singleton or batch layout
and position ranges. `LegacyConfirmed` is the non-destructive exception for a
complete v17 domain confirmation that cannot be re-derived; it is never used
for new confirmations. Partial confirmation becomes `Recovering`, not
`Confirmed`, and uses a digestless guard when it also lacks derivation inputs.
Canonical imported candidates are normalized to lowercase. Unusable historical
values may remain byte-for-byte in legacy projection columns for compatibility,
but runtime lookup never treats them as candidates.

The migration inserts rows, validates identity, hash, and position collisions,
and changes `user_version` in one transaction. Failure preserves version 17,
`user_version`, and all original bytes; rollback followed by retry classifies
the same markers, and reopening a successful migration creates no duplicates.
Fresh and migrated version-18 schemas must match. A database created with an
older unreleased version-18 shape is rejected by schema fingerprint with an
explicit recreation error; it is not silently upgraded.

After migration, only `chain_submissions` is authoritative. Version-17 domain
hash and position columns remain confirmation projections and compatibility
data.

## Removed legacy APIs

The public API does not expose:

- caller-controlled transaction-hash recording;
- caller-controlled VAN or vote-commitment position recording;
- direct lifecycle-state mutation;
- hash provenance attachment;
- attempt insertion or retirement;
- recovery-descriptor or tree-receipt persistence; or
- scan-cursor or partial-match persistence; or
- standalone recovery clearing.

Delegation, vote, and batch lifecycle entry points are the only route to new
submission, polling, recovery, and confirmation. Event parsing, tree matching,
and domain/helper writes are private lifecycle mechanisms. Prelude and storage
facades must not re-export removed mutation APIs.

## Required conformance coverage

Conformance must be demonstrated by behavior, not source-layout assertions,
descriptor golden files, provenance matrices, or precedence tables.

### State and transport

Tests cover:

- reservation commits before any POST byte is released;
- reservation failure dispatches nothing;
- usable success hash produces `Tracking`;
- inconclusive hash polling has a bounded promotion to candidate-preserving
  `Recovering`;
- polling, diagnostics, and restart do not reset the durable tracking window;
- hash polling produces atomic `Confirmed`;
- definite rejection produces `Rejected`;
- definite pre-dispatch failure does not create ambiguity;
- every possibly-dispatched class produces `Recovering`;
- restart from `Submitting` produces `Recovering`;
- retry limits and endpoint failover are globally bounded; and
- retries cannot change semantic generation.

### Recovery

Tests cover:

- `Tracking` never invokes the tree client;
- `Recovering` polls its candidate before scanning;
- a later candidate hash does not remove sticky recovery;
- a pending or unreadable candidate blocks redispatch and cannot be
  overwritten;
- a committed-failure recovery candidate is cleared before a later
  same-generation POST may be reserved;
- committed failure of a recovery candidate still permits tree recovery;
- no match, delayed indexing, malformed pages, cancellation, and exhausted
  bounds remain `Recovering`;
- delegation, singleton, and batch exact layouts recover positions;
- partial, reordered, nonadjacent, and duplicate layouts do not confirm;
- scans use one validated fixed complete snapshot;
- a full `2^24`-leaf snapshot fits the `4,096`-request, `32 GiB`, `72`-hour,
  and streaming-memory ceilings without a smaller restart budget;
- invalid lower per-request configuration is rejected before submission or
  recovery is enabled;
- interrupted scans restart without durable cursors or partial evidence; and
- tree confirmation never invents a hash.

### Locking, atomicity, and cancellation

Tests cover:

- singleton choice and complete batch membership lock after possible dispatch;
- concurrent work cannot reserve two generations for one identity;
- bundle locking prevents two successors from consuming the same VAN;
- independent bundles continue while one is unresolved;
- hash and tree confirmation atomically update submission, domain, recovery,
  and helper state;
- injected failure at every atomic write point rolls back all updates;
- identical confirmation replay is idempotent and conflicting replay writes
  nothing;
- entry cancellation with no stronger durable state returns `Cancelled`
  without releasing bytes;
- entry cancellation with an abandoned `Submitting` row atomically normalizes
  it to `Recovering` and returns `Pending(Recovering)` without network work;
- entry cancellation over `Tracking`, `Recovering`, `Confirmed`,
  `LegacyConfirmed`, or `Rejected` returns the authoritative stronger result;
- cancellation after reservation but before dispatch releases no bytes and
  removes only the fresh definitely-unsent reservation;
- failure to persist that normalization reports an operational failure with
  the known possibly-dispatched state and never returns `Cancelled`;
- cancellation after dispatch preserves `Recovering`; and
- cancellation after the confirmation commit point cannot suppress
  persistence.

### Migration, cleanup, and API surface

Tests cover:

- each v17 import class: confirmed, legacy-confirmed without recovery material,
  canonical unconfirmed hash with and without recovery material, unusable hash,
  partial confirmation with and without recovery material, and hashless
  committed recovery material;
- `record_vc_position_without_recovery_json_updates_column`-shaped rows with
  complete recorded VAN and VC positions migrate to `LegacyConfirmed`, expose
  source `legacy_projection`, no confirmed hash or validated layout, block
  dispatch, and survive cleanup;
- digestless `Recovering` guards preserve incomplete v17 evidence, cannot
  dispatch or reconcile without first binding complete derivation inputs, and
  survive cleanup;
- an empty v17 vote row creates no submission row or guard;
- unbound `Pending(Recovering)` reports `RecoveryUnavailable`, authorizes and
  schedules no network work across restart, and atomically excludes a competing
  native row while binding or rolling back;
- schema constraints cover digest nullability, legacy sources, zero attempts,
  null candidates and tracking timestamps, required observed positions,
  partial guard uniqueness, and legacy/native identity exclusion;
- fixtures cover position zero, historical hash present or absent, absent
  versus malformed recovery JSON, duplicate legacy identities, position
  collisions under the exact ownership rules, each batch indicator,
  present-but-null batch fields as non-indicators, singleton/batch ambiguity,
  and unreconstructable delegation;
- an advanced v17 bundle VAN is never imported as the original delegation
  output position;
- rollback/retry and reopen are deterministic and create no duplicate guards;
- planning, cancellation, cleanup, deletion, and public results perform no
  derivation, network work, helper-plan creation, invented hash, or validated
  layout claim for `LegacyConfirmed`;
- a hashless v17 import remains `Recovering` after a retry receives a definite
  rejection, leaving exact tree recovery available for a possibly successful
  pre-upgrade POST;
- canonical-hash ownership collision rollback and the exact atomic-batch
  exception;
- fresh and migrated v18 schema equivalence and stale-v18 fingerprint
  rejection;
- ordinary cleanup and reset preserve every unresolved generation, its
  retry/recovery data, helper plan, and complete delivery history;
- standalone recovery clearing is not public or used by round cleanup, and the
  destructive primitive runs only under exclusive account deletion;
- partial pruning refuses protected ranges without renumbering bundles;
- deletion gates block new work and wait for active work;
- planners and recovery snapshots derive from the authoritative row; and
- removed legacy mutation APIs fail compile-time surface checks.

These tests are the review contract for changes to chain submission behavior.
