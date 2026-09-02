# Chain submission invariants

## Status and purpose

This document is the normative specification for vote-chain submission in
`zcash_voting`. Changes to the behavior described here must update this
document and its behavior-oriented conformance tests in the same change.

The design has one authoritative `chain_submissions` row for each semantic
generation, plus migration-only legacy guards for v17 chain evidence whose
generation cannot be reconstructed. A complete legacy confirmation uses
`LegacyConfirmed`; incomplete evidence uses a digestless `Recovering` guard.
Digestless guards remain permanently unbound. The design does not use an
attempt journal or a durable scan workflow.

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
durable recovery ambiguity. A completed valid recovery pass may authorize one
atomic candidate-retirement and same-generation retry reservation; retirement
is not evidence that the candidate failed or was never dispatched. Tree
recovery never runs for ordinary `Tracking`.

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

Generation digest version 1 is SHA-256 over the ASCII domain
`zcash_voting.chain_submission.generation.v1` followed by a NUL byte and a
canonical typed transcript. Each transcript field is encoded as a big-endian
`u16` tag length, the ASCII tag, a big-endian `u64` value length, and the value.
Integers use fixed-width big-endian encoding, booleans are one byte, and every
sequence includes a big-endian `u32` count followed by index-tagged elements.
The transcript hashes parsed typed values, never recovery JSON bytes or a final
signed request body. Field tags, ordering, and the frozen digest vector are
part of the durable version-18 compatibility contract.

The common identity field order is `identity.wallet_id`, `identity.network`,
`identity.vote_chain_id`, `identity.vote_round_id`, `identity.bundle_index`,
and `identity.kind`, followed by `identity.proposal_id` for a singleton or
`identity.ordered_batch_digest` for a batch. Delegation fields then appear in
this order:

```text
delegation.note_positions
delegation.note_identity_hashes
delegation.van_comm_rand
delegation.dummy_nullifiers
delegation.rho_signed
delegation.padded_note_data
delegation.nf_signed
delegation.cmx_new
delegation.alpha
delegation.rseed_signed
delegation.rseed_output
delegation.gov_comm
delegation.total_note_value
delegation.address_index
delegation.rk
delegation.gov_nullifiers
delegation.padded_note_secrets
delegation.pczt_sighash
delegation.tx1_effects
delegation.proof
```

Vote generations contain the `votes` sequence. Each `votes.<index>` member is
encoded in this order: `vote_round_id`, `bundle_index`, `proposal_id`,
`vote_decision`, `anchor_height`, `single_share`, `num_options`,
`van_nullifier`, `vote_authority_note_new`, `vote_commitment`, `proof`,
`shares_hash`, `r_vpk`, `alpha_v`, `vote_auth_sig`, `encrypted_shares`,
`share_blinds`, `share_comms`, and `batch`. An encrypted-share member is `c1`,
`c2`, `share_index`, `plaintext_value`, and `randomness`. A batch member appends
`batch.digest`, `batch.index`, and `batch.size`; a singleton encodes only the
literal `singleton` batch value. Every named sequence first emits
`<tag>.count`, then uses decimal zero-based indexes in its element tags.
Delegation padded-note-secret elements use `.rho` followed by `.rseed`.

The complete v1 frozen digests are:

- delegation: `41b0eecb59da7f911b94c7ae540f1674fb9b399feb2c71233024a297b2df5c63`;
- singleton vote: `bfb3ebc460d3300aa9f3943cf023ee30ccc3bfae5a93ba9b131b4f99fa7706b1`;
- ordered two-vote batch: `40e3bfefec14a5a21b9c11e6ee0140996fe1f9025140303de97dd7e89bf31b6c`.

Their complete typed fixtures are maintained by
`generation_digest_v1_matches_frozen_vector`; changing a fixture or digest is
a generation-format version change, not an ordinary refactor.

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
fresh singleton submission for that identity. They also block any batch that
contains the guarded `(bundle, proposal)` member; changing `kind` from `vote`
to `vote_batch` cannot bypass a legacy guard. A partial unique index permits at
most one digestless guard per legacy identity, and reservation checks every
applicable singleton or batch-member legacy identity under canonically ordered
submission-identity locks before deriving or inserting a generation. The same
transaction prohibits a legacy guard from coexisting with a native singleton
row or overlapping batch row.

The row stores only:

- the typed identity;
- generation digest, nullable only for migration-only `LegacyConfirmed` and
  digestless `Recovering` guards;
- durable state;
- one optional canonical candidate transaction hash;
- monotonic count of committed POST reservations;
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
candidate, attempts, or tracking-start timestamp. It can never acquire a
generation digest or candidate and cannot scan, retry, or confirm.

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
current handle to poll; it is not confirmation, has no durable provenance
class, and need not be retained when a completed valid recovery pass directly
authorizes an atomic same-generation retry reservation. Retirement makes no
claim about the transaction's chain outcome. Legacy guards do not import a
candidate or confirmed hash; any historical hash remains only in its unchanged
legacy projection column.
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
  that guard remains permanently unbound and performs no network work. An
  ordinary generation-bound state is sticky until confirmation or explicit
  deletion; a digestless guard remains until explicit deletion.
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

The same lifecycle is shown as a text state machine below:

```text
new
 `-- reserve before POST --> Submitting
                              |-- usable success hash --> Tracking
                              |                           |-- pending --> Tracking
                              |                           |-- window expires --> Recovering
                              |                           |-- committed success --> Confirmed
                              |                           `-- committed failure --> Rejected
                              |-- possibly dispatched --> Recovering
                              |                           |-- candidate, scan, or retry --> Recovering
                              |                           `-- candidate success or exact tree layout
                              |                               --> Confirmed
                              |-- definite rejection --> Rejected
                              `-- definitely unsent first attempt --> no row

abandoned Submitting on restart -- normalize --> Recovering
unresolved legacy evidence ------ migrate ----> Recovering
complete legacy projection ------ migrate ----> LegacyConfirmed
```

The absence of `Recovering -> Tracking` and `Recovering -> Rejected` edges is
intentional. Once recovery ambiguity exists, only confirmation or explicit
deletion can resolve or remove it.

`Recovering` does not transition to `Tracking` or `Rejected`. In particular, a
later hash, rejection, committed-failure candidate, cancellation, or empty scan
cannot erase the original ambiguity. A pending or unreadable candidate is never
overwritten and normally blocks another POST. It may be atomically retired only
as part of the retry reservation directly authorized by candidate-first
reconciliation completing a valid full-tree pass with no complete match. A
definitively committed-failure candidate is atomically cleared without
requiring that pass, but another valid no-match pass is still required before
retry reservation. Either operation clears only the polling handle: the row
remains `Recovering`, the original ambiguity remains durable, and tree recovery
remains available.

Terminal rows are immutable except for idempotent replay of identical
confirmation data and explicit round or account deletion. `LegacyConfirmed` is
terminal, rejects every runtime transition and confirmation replay, and cannot
be promoted or replaced by a reconstructed generation. Only deterministic
reclassification during a retried atomic migration may recreate the same
marker. Conflicting terminal data is an invariant error and writes nothing.

## Reservation and transport classification

Before releasing any POST byte, the lifecycle:

1. acquires the round/account gate, bundle lock where applicable, and
   applicable submission-identity locks in canonical order;
2. derives the recovery-independent identity, plus every batch member's legacy
   singleton identity when applicable, and loads their authoritative rows;
3. returns `LegacyConfirmed` for a matching singleton, returns a digestless
   guard as pending with `RecoveryUnavailable`, or rejects an overlapping
   batch without dispatch, before requiring recovery material;
4. loads and locks the generation inputs and derives the generation digest and
   expected layout;
5. creates the `Submitting` row, or validates the existing same-generation
   `Recovering` row;
6. increments the attempt count for the request; and
7. commits the reservation.

For a recovery retry, steps 5 through 7 additionally consume the private
single-use authorization produced by the immediately preceding valid no-match
pass. They clear any inconclusive candidate and increment the attempt count in
the same immediate transaction. There is no standalone candidate-retirement
mutation and an empty candidate slot is not retry authorization.

Guard lookup and native row insertion share the same canonically ordered
identity locks and immediate transaction, so a concurrent singleton or batch
call cannot bypass or replace the guard.

If reservation fails, dispatch does not occur. A process-local in-flight guard
prevents cleanup, replacement, or deletion from racing response
classification.

Only transport code can classify a failure as `DefinitelyUnsent`, and only
before request bytes are released to a network stack that may deliver them.
Cancellation before that boundary is also definitely unsent.

For a first attempt, definitely-unsent failure removes the fresh `Submitting`
reservation; it does not create chain rejection or ambiguity. For a retry from
`Recovering`, it leaves the row hashless and `Recovering`, retains the
reservation in the monotonic attempt count, and does not restore either the
retired candidate or the consumed no-match authorization. Another retry
requires another valid no-match pass. Attempt count is diagnostic and is never
decremented, refunded, or used as a permanent retry gate.

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

Within each lifecycle invocation, POST attempts, endpoints, body sizes, request
durations, and backoffs are bounded by configuration with safe finite maxima.
Redirects are not followed. A later invocation may reconcile and retry after
another valid no-match pass regardless of the monotonic historical attempt
count. Retries are allowed only for the same semantic generation.

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
polling. In `Recovering`, it does not disable tree recovery and prohibits
another POST until it is either committed unsuccessfully or retired after a
completed valid no-match tree pass. A committed-success candidate proceeds to
confirmation. A `Tracking` candidate that is committed unsuccessfully becomes
`Rejected`; a `Recovering` candidate that is committed unsuccessfully is
atomically cleared while the row remains `Recovering`.

After candidate polling and a bounded no-match tree pass, the lifecycle
receives one private process-local authorization for the captured identity,
generation, host operation epoch, and continuously held round, bundle, and
identity locks. The authorization is not persisted, cloned, returned, or
reconstructed from a hashless row. It is consumed only by one immediate
transaction that validates the same bound `Recovering` row, atomically clears
any remaining inconclusive candidate, and increments the attempt count to
reserve one same-generation retry. The transaction consumes the no-match
authorization. The authorization expires on cancellation, error, return, lock
release, or process exit. A later usable hash fills the empty candidate slot,
but sticky recovery remains in force. An unbound guard is never eligible.

A pending or unreadable candidate is never treated as committed failure.
Retirement during the authorized retry reservation likewise does not classify
the candidate as failed, absent, or definitely unsent. Endpoint disagreement,
malformed responses, and temporary lookup failure remain retryable diagnostics
rather than terminal evidence.

Same-generation redispatch after retirement is safe even if the retired
transaction later commits: every reconstruction is checked against the same
generation digest and consumes the same input nullifiers, so competing
transactions cannot both commit, and whichever transaction commits produces
the same exact expected output layout. Sticky tree recovery can therefore
confirm a retired transaction without retaining its hash. Retirement never
permits a different generation, does not bypass attempt or backoff bounds, and
does not authorize overwriting an occupied candidate slot.

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
  `LegacyConfirmed` or digestless `Recovering` guard exists, including for
  every member of a proposed batch.

An unresolved generation or legacy guard blocks only later work that consumes
its unknown successor VAN. Independent bundles remain schedulable.

## Sticky recovery and tree matching

Tree recovery is authorized only by a bound durable `Recovering` row. It never
runs for a digestless guard, ordinary `Tracking` even when its hash is pending,
or fresh unsubmitted work. A pending candidate carried by a bound `Recovering`
row is polled first and does not prevent the subsequent tree pass.

Before scanning, the lifecycle re-derives the generation digest and complete
expected layout from locked durable recovery rows. Missing or corrupt private
recovery material keeps the row `Recovering`, reports a stable bounded
diagnostic, and does not turn uncertainty into rejection.

Each recovery pass:

1. polls the current candidate hash, if any;
2. selects one fixed, complete, internally consistent tree snapshot whose
   validated metadata declares its final size;
3. scans that snapshot under per-request and whole-pass bounds;
4. compares leaves locally without transmitting expected commitments;
5. confirms only one complete unique ordered layout; and
6. if the valid complete scan instead finds no complete layout, produces one
   private authorization that may be consumed immediately to atomically retire
   the inconclusive candidate, if any, and reserve a same-generation retry.

A no-match authorization requires successful traversal of the entire selected
snapshot. Timeout, cancellation, malformed or incomplete pagination,
contradictory snapshot metadata, endpoint exhaustion, or multiple complete
matches produce no authorization and do not retire a candidate. Partial,
reordered, nonadjacent, or otherwise incomplete occurrences are not
confirmation and do not prevent authorization after the otherwise valid
complete scan.

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
complete match rejects the result and retains any candidate. Partial,
reordered, nonadjacent, or otherwise incomplete occurrences leave the row
`Recovering` with no partial position write but permit the private authorization
after the valid complete scan. Malformed pages, cancellation, endpoint
exhaustion, and transport interruption do not complete a pass, produce no
authorization, and retain the candidate. Delayed indexing may produce a valid
no-match pass and therefore permits the combined retirement-and-reservation;
the same-generation and nullifier rules make a later commit safe. A responsive
endpoint serving a supported snapshot cannot repeatedly stop at a local
whole-pass budget: its complete traversal fits by construction.

Candidate retirement, its diagnostic update, and retry reservation are one
immediate transaction. If that transaction fails, the candidate remains
authoritative and the attempt count does not advance; for an already hashless
row, the attempt count likewise does not advance. Redispatch remains
prohibited. If cancellation or failure occurs after the transaction commits
but before dispatch, the normal definitely-unsent retry rule leaves the row
hashless and `Recovering` and retains the committed reservation count. A crash
at that boundary has the same durable shape. In either case, because no scan
authorization is durable, the next invocation must complete another valid
no-match pass before reserving a retry. The row never becomes fresh unsubmitted
work.

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
that tree recovery remains authorized. It may carry no candidate after an
atomic retirement-and-reservation or committed-failure clearing, without
implying that a retired transaction failed or that another retry is already
authorized. For an unbound legacy guard it also carries no candidate, uses the
stable `RecoveryUnavailable` diagnostic, authorizes no network recovery, and
is never automatically rescheduled.
`Rejected` means the durable row is terminally rejected.

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
3. applicable submission-identity locks in canonical identity order;
4. database handle; and
5. immediate SQLite transaction.

The identity locks serialize lifecycle work for the authoritative row and, for
a batch, every member's legacy singleton key. The bundle lock prevents two
proposals from deriving successors from the same VAN. Different bundles remain
independent.

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

There is no standalone recovery-clear operation or
`clear_recovery_state` primitive. Ordinary cleanup and reset preserve recovery
material, helper plans, and all helper-delivery history while the owning round
remains live.

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
gated round directly, and account deletion removes every gated round for the
account. Their foreign-key cascades remove the owned recovery and helper
records; neither operation selectively clears those records from a live round.

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
   intact, but it remains permanently unbound and performs no polling,
   scanning, retry, or confirmation.
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

Permanently guarding an affected legacy identity can block that identity and
dependent work whether its pre-upgrade transaction committed, failed, was
never dispatched, or never committed. Progress then requires explicit
destructive round or account deletion. This availability risk is accepted as a
product decision because the required combination of independently persisted
incomplete version-17 chain evidence and absent recovery JSON is expected to
occur with low probability; that expectation is not based on measured
telemetry. Later recovery inputs are not accepted as proof of the original
generation: without a durable cryptographic anchor, they may describe a
different input nullifier or output layout.

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
- promotion alone does not retire the candidate or permit redispatch;
- hash polling produces atomic `Confirmed`;
- definite rejection produces `Rejected`;
- definite pre-dispatch failure does not create ambiguity;
- every possibly-dispatched class produces `Recovering`;
- restart from `Submitting` produces `Recovering`;
- retry limits and endpoint failover are bounded per lifecycle invocation; and
- retries cannot change semantic generation.

### Recovery

Tests cover:

- `Tracking` never invokes the tree client;
- `Recovering` polls its candidate before scanning;
- a later candidate hash does not remove sticky recovery;
- a pending or unreadable candidate blocks redispatch and cannot be
  overwritten before a completed valid no-match pass;
- a completed valid no-match pass produces one private single-use
  authorization bound to the identity, generation, operation epoch, and
  continuously held locks;
- one immediate transaction consumes that authorization, atomically retires an
  inconclusive candidate without classifying it as failed, and reserves only a
  same-generation retry;
- there is no standalone retirement mutation, and a hashless `Recovering` row
  cannot itself authorize retry;
- cancellation, error, return, lock release, and process exit invalidate an
  unconsumed authorization;
- timeout, cancellation, malformed or incomplete pagination, endpoint
  exhaustion, contradictory snapshot metadata, and multiple complete matches
  produce no authorization and do not retire the candidate;
- partial, reordered, nonadjacent, and otherwise incomplete occurrences do not
  confirm but permit authorization after the valid complete scan;
- failed retirement-and-reservation persistence leaves the candidate
  authoritative, does not increment the attempt count, and blocks redispatch;
- cancellation or definitely-unsent failure after the combined transaction
  leaves the monotonic reservation count unchanged, leaves hashless sticky
  recovery, and requires a new completed valid no-match pass before retry;
- attempt count never decreases, is diagnostic rather than a permanent retry
  gate, and cannot underflow or be reopened by callback ordering;
- each lifecycle invocation enforces independent finite attempt and backoff
  limits even though later invocations may reconcile and retry;
- restart after the combined transaction conservatively requires a new
  completed valid no-match pass before retry;
- an originally hashless bound `Recovering` row likewise cannot POST before a
  completed valid no-match pass, so retry permission never depends on
  non-durable scan history;
- a retired transaction that later commits is confirmed by its exact tree
  layout, while nullifier conflict prevents both it and its same-generation
  retry from committing;
- a committed-failure recovery candidate is cleared before a later
  same-generation POST may be reserved;
- committed failure of a recovery candidate still permits tree recovery;
- no match, delayed indexing, malformed pages, cancellation, and exhausted
  bounds remain `Recovering`;
- candidate-less `Pending(Recovering)` after retirement-and-reservation or
  committed-failure clearing neither claims failure nor authorizes retry and
  remains distinct from an unbound guard's `RecoveryUnavailable`;
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
- digestless `Recovering` guards preserve incomplete v17 evidence, remain
  permanently unbound, cannot dispatch or reconcile, reject every runtime
  transition, and survive cleanup;
- after migrating a digestless guard, adding complete recovery inputs and
  invoking planning or advancement leaves every guard field unchanged,
  including its null digest, null candidate, and zero attempts; inserts no
  native row; and performs no network call;
- an empty v17 vote row creates no submission row or guard;
- unbound `Pending(Recovering)` reports `RecoveryUnavailable`, authorizes and
  schedules no network work across restart, and atomically excludes a competing
  native row or attempted replacement;
- singleton planning and batch planning both check every applicable legacy
  identity, and a batch containing a `LegacyConfirmed` or digestless guarded
  `(bundle, proposal)` member dispatches nothing and cannot insert an
  overlapping native batch row;
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
- no standalone recovery-clear API or storage primitive exists, and only
  exclusive round or account deletion removes owned recovery and helper rows;
- partial pruning refuses protected ranges without renumbering bundles;
- deletion gates block new work and wait for active work;
- planners and recovery snapshots derive from the authoritative row; and
- removed legacy mutation APIs fail compile-time surface checks.

These tests are the review contract for changes to chain submission behavior.

Generation and confirmation coverage is anchored by
`generation_digest_v1_matches_frozen_vector`,
`generation_digest_binds_semantics_and_ignores_confirmation_positions`,
`batch_generation_digest_and_layout_preserve_action_order`,
`expected_layouts_follow_signed_action_order`,
`persisted_vote_generation_survives_confirmation_projection`,
`typed_confirmation_uses_the_full_sqlite_position_range`,
`typed_batch_confirmation_rolls_back_when_a_later_member_conflicts`,
`records_vote_confirmation_atomically`, and
`records_vote_batch_confirmation_replay_and_helper_positions`.
