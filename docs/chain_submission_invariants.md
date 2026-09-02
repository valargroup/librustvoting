# Chain submission invariants

## Status and purpose

This document is the normative specification for vote-chain submission in
`zcash_voting`. Changes to the behavior described here must update this
document and its behavior-oriented conformance tests in the same change.

The design has one authoritative `chain_submissions` row for each semantic
generation. It does not use an attempt journal or a durable scan workflow.

The normal path is:

```text
reserve Submitting -> POST -> store hash as Tracking -> poll -> Confirmed
```

If a request may have been dispatched but no usable hash is available, the row
becomes `Recovering`. Recovery polls a candidate hash first when one exists,
then may search the commitment tree for the generation's exact output layout.
`Recovering` is sticky: later responses and retries may add a candidate hash,
but they do not erase the earlier hashless ambiguity. Tree recovery never runs
for ordinary `Tracking`.

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
Creation and all transitions occur in immediate transactions.

The row stores only:

- the typed identity;
- generation digest;
- durable state;
- one optional canonical candidate transaction hash;
- attempt count;
- a bounded redacted diagnostic;
- final confirmation source;
- final VAN and vote-commitment positions in generation order; and
- creation and update timestamps.

Final ordered positions use a closed typed encoding, not descriptor JSON. All
positions are `u64` in the lifecycle and must fit SQLite's signed integer range.
Position zero is valid.

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
Rejected
```

Their meanings are:

- `Submitting`: the first POST is durably reserved, but its response has not
  been durably classified.
- `Tracking`: a usable candidate hash is known and no hashless possibly
  dispatched request exists. Reconciliation only polls that hash.
- `Recovering`: at least one request may have been dispatched without a usable
  hash. An optional candidate hash is polled before tree recovery. The state is
  sticky until confirmation or explicit deletion.
- `Confirmed`: chain success and all required local confirmation updates are
  durable. New confirmations record `hash` or `tree` as their source and record
  the exact generation positions. Migrated version-17 confirmations record
  `legacy_import`.
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
cannot erase the original ambiguity. A failed candidate is cleared or replaced
only under the same-generation recovery rules; the row remains `Recovering`
and tree recovery remains available.

Terminal rows are immutable except for idempotent replay of identical
confirmation data and explicit round or account deletion. Conflicting terminal
data is an invariant error and writes nothing.

## Reservation and transport classification

Before releasing any POST byte, the lifecycle:

1. acquires the round/account gate, bundle lock where applicable, and
   submission-identity lock;
2. loads and locks the generation inputs;
3. derives and validates identity, generation digest, and expected layout;
4. creates the `Submitting` row, or validates the existing same-generation
   `Recovering` row;
5. increments the attempt count for the request; and
6. commits the reservation.

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
may replace an absent or already-failed recovery candidate and is polled before
the next tree scan. Because the protocol does not specify that an error hash
identifies an earlier accepted transaction, the hash is only a recovery handle.

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
- `Recovering` polls its candidate hash first. If hash polling does not confirm,
  the lifecycle may perform one bounded tree recovery pass.
- `Confirmed` and `Rejected` perform no network mutation.

A pending or temporarily unreadable candidate remains available for later
polling. In `Recovering`, it does not permanently disable tree recovery. A
committed-success candidate proceeds to confirmation. A `Tracking` candidate
that is committed unsuccessfully becomes `Rejected`; a `Recovering` candidate
that is committed unsuccessfully leaves the row `Recovering`.

After candidate polling and a bounded no-match tree pass, `Recovering` may retry
POST only for the same generation. The retry reservation increments the row's
attempt count while preserving `Recovering`. A later usable hash is stored as
the candidate, but sticky recovery remains in force.

A pending or unreadable candidate is never treated as committed failure.
Endpoint disagreement, malformed responses, and temporary lookup failure
remain retryable diagnostics rather than terminal evidence.

Restart plans are derived from the authoritative row:

- `Tracking` schedules hash polling;
- `Recovering` schedules candidate-first reconciliation and tree recovery, then
  same-generation retry when permitted;
- `Confirmed` enables dependent domain and helper work;
- `Rejected` schedules no reconciliation; and
- absent rows permit fresh work if bundle causality allows it.

An unresolved generation blocks only later work that consumes its unknown
successor VAN. Independent bundles remain schedulable.

## Sticky recovery and tree matching

Tree recovery is authorized only by durable `Recovering`. It never runs for
ordinary `Tracking`, merely pending hashes, or fresh unsubmitted work.

Before scanning, the lifecycle re-derives the generation digest and complete
expected layout from locked durable recovery rows. Missing or corrupt private
recovery material keeps the row `Recovering`, reports a stable bounded
diagnostic, and does not turn uncertainty into rejection.

Each recovery pass:

1. polls the current candidate hash, if any;
2. selects one fixed, complete, internally consistent tree snapshot;
3. scans that snapshot under per-request and whole-pass bounds;
4. compares leaves locally without transmitting expected commitments; and
5. accepts only one complete unique ordered layout.

The scanner validates snapshot identity, heights, roots, ranges, absolute
indexes, pagination progress, final size, canonical field encodings, and
response bounds. It has finite limits for request count, bytes, leaves, elapsed
time, and memory. Cancellation is checked between requests and before the
confirmation commit point.

Finding one member is insufficient. Singleton outputs must be adjacent and in
order. A batch must contain the final successor VAN followed immediately by
every vote commitment in signed action order. Partial, reordered, overlapping,
or independently located members do not confirm.

The entire selected snapshot is checked even after a match, so a second
complete match rejects the result. Duplicate or partial matches, malformed
pages, delayed indexing, cancellation, endpoint exhaustion, and any bound being
reached leave the row `Recovering` with no partial position write.

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

`Confirmed` means the authoritative row and all atomic domain/helper updates
are durable. `Pending(Tracking)` carries the candidate hash needed to continue.
`Pending(Recovering)` may carry a candidate hash and a bounded diagnostic, but
always preserves the fact that tree recovery remains authorized. `Rejected`
means the durable row is terminally rejected.

`Cancelled` is returned only when cancellation occurs before possible dispatch
and no stronger durable state exists. Cancellation never hides `Tracking`,
`Recovering`, or `Confirmed`. A call cancelled on entry may perform the minimum
read-only state load needed to report the authoritative durable result, but it
starts no POST, lookup, scan, retry, or confirmation write.

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
requests, and the confirmation commit point. It has only three safety effects:

- before dispatch, no request is released and a fresh reservation may be
  removed;
- after possible dispatch, the row is or becomes `Recovering`; and
- after the confirmation commit point, cancellation cannot suppress the atomic
  write.

The captured host operation epoch is checked at the same pre-commit boundaries.

## Cleanup, pruning, and deletion

Ordinary cleanup and reset use the authoritative state under the same operation
and bundle locks. They preserve every `Submitting`, `Tracking`, `Recovering`,
and `Confirmed` row and all domain material required to reconcile, retry, prove,
or apply that generation, including:

- delegation setup, proofs, nullifiers, and VAN randomizer;
- vote and batch recovery material;
- locked proposal choices, batch membership, and order;
- current and generation-specific VAN/VC positions; and
- helper plans bound to the generation.

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
undo a transaction that may already be on chain.

## Version 17 to version 18

Version 17 is the released migration source. Version 18 is unreleased, so its
schema is replaced in place:

- the final `chain_submissions` definition is part of `17 -> 18`;
- `001_init.sql` creates the same schema;
- `CURRENT_VERSION` remains 18; and
- there is no `18 -> 19` migration.

Migration constructs at most one authoritative row for each semantic generation
after re-deriving identity and expected layout from the version-17 domain and
recovery rows.

Version-17 state imports as follows:

- a generation with complete, internally consistent confirmed domain positions
  becomes `Confirmed` with source `legacy_import`, its available hash, and its
  exact positions;
- a canonical unconfirmed transaction hash becomes `Tracking`;
- an unusable, malformed, opaque, or otherwise non-pollable historical hash
  becomes `Recovering` with no candidate hash; and
- committed recovery material with no submission evidence creates no row and
  remains eligible for fresh submission.

Migration validates complete unique ownership of canonical hashes across
wallet, chain/network, round, kind, bundle, and proposal or batch. The only
allowed shared hash is the complete ordered membership of one valid atomic
batch. Cross-generation collisions, partial batches, inconsistent positions,
or malformed recovery metadata abort the whole migration; migration does not
guess, merge, or silently normalize ownership.

Confirmed imports validate the complete expected singleton or batch layout and
position ranges. Partial confirmation becomes `Recovering`, not `Confirmed`.
Canonical imported candidates are normalized to lowercase. Unusable historical
values may remain byte-for-byte in legacy projection columns for compatibility,
but runtime lookup never treats them as candidates.

The migration is atomic: failure preserves version 17, `user_version`, and all
original bytes. Fresh and migrated version-18 schemas must match. A database
created with an older unreleased version-18 shape is rejected by schema
fingerprint with an explicit recreation error; it is not silently upgraded.

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
- scan-cursor or partial-match persistence.

Delegation, vote, and batch lifecycle entry points are the only route to new
submission, polling, recovery, and confirmation. Event parsing, tree matching,
and domain/helper writes are private lifecycle mechanisms. Prelude and storage
facades must not re-export removed mutation APIs.

## Conformance tests

Conformance is demonstrated by behavior, not source-layout assertions,
descriptor golden files, provenance matrices, or precedence tables.

### State and transport

Tests cover:

- reservation commits before any POST byte is released;
- reservation failure dispatches nothing;
- usable success hash produces `Tracking`;
- inconclusive hash polling has a bounded promotion to candidate-preserving
  `Recovering`;
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
- committed failure of a recovery candidate still permits tree recovery;
- no match, delayed indexing, malformed pages, cancellation, and exhausted
  bounds remain `Recovering`;
- delegation, singleton, and batch exact layouts recover positions;
- partial, reordered, nonadjacent, and duplicate layouts do not confirm;
- scans use one validated fixed complete snapshot;
- scans are bounded in requests, bytes, leaves, elapsed time, and memory;
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
- cancellation before dispatch returns `Cancelled` without releasing bytes;
- cancellation after dispatch preserves `Recovering`; and
- cancellation after the confirmation commit point cannot suppress
  persistence.

### Migration, cleanup, and API surface

Tests cover:

- each v17 import class: confirmed, canonical unconfirmed hash, unusable hash,
  partial confirmation, and never-submitted recovery material;
- canonical-hash ownership collision rollback and the exact atomic-batch
  exception;
- fresh and migrated v18 schema equivalence and stale-v18 fingerprint
  rejection;
- cleanup preserves every unresolved generation and its retry/recovery data;
- partial pruning refuses protected ranges without renumbering bundles;
- deletion gates block new work and wait for active work;
- planners and recovery snapshots derive from the authoritative row; and
- removed legacy mutation APIs fail compile-time surface checks.

These tests are the review contract for changes to chain submission behavior.
