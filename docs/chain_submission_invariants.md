# Chain submission invariants

## Status and purpose

This document is the normative specification for vote-chain submission in
`zcash_voting`. Changes to the behavior described here must update this
document and its behavior-oriented conformance tests in the same change.

The design has one authoritative `chain_submissions` row for each semantic
generation, and every row is created by the lifecycle itself: no pre-upgrade
evidence is imported into it. The design does not use an attempt journal or a
durable scan workflow.

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
An implementation phase that cannot yet perform the complete candidate-first
recovery, fixed-snapshot tree scan, and authorized same-generation retry keeps
its submission entry points private. A partial public lifecycle that can enter
`Recovering` without advancing it is not conformant.
An implementation is not releasable unless it provides the complete recovery
lifecycle described by this specification and explicitly resolves a
proven-absent rejected generation that cannot succeed unchanged. A conformant
resolution may allow a replacement generation after a valid no-match pass or
may rely on a versioned chain contract that proves replacement is unnecessary.

Vote API and commitment-tree endpoints are trusted for chain status, events,
and validated snapshots. A malformed, incomplete, contradictory, or
unsupported response is not evidence. Routes that promise privacy fail closed,
mutation and lookup redirects are not followed, and production endpoints use
authenticated encryption.

The supported deployment topology has exactly one vote ledger for each
production `Network` value: one mainnet ledger and one testnet ledger. Every
Vote API endpoint, commitment-tree endpoint, and configured vote-chain id used
with a network, including replacements made across process restarts, must refer
to that same ledger and its canonical commitment tree. Endpoint rotation and
failover select replicas or gateways for the ledger; they are not a chain
migration mechanism.

Running two independent ledgers under the same `Network`, mixing endpoints from
different ledgers, or reusing a voting database after repointing that network
to another ledger is unsupported operator misconfiguration and is outside the
submission lifecycle's threat model. Each independent regtest ledger likewise
requires a fresh voting database or an otherwise separate storage namespace.
The lifecycle is therefore not required to persist an additional routing
identifier or reconcile an ambiguous attempt safely across independent
ledgers.

One process exclusively owns a voting database. Operations capture wallet,
round, submission identity, and host operation epoch once; an account switch
cannot retarget in-flight work.

## Identity and semantic generation

A submission identity contains:

```text
(wallet, network, round, kind, bundle, proposal-or-batch)
```

There is exactly one kind of submission identity. The configured vote-chain id
selects where a request is dispatched, not what it means, so it binds neither
the identity nor the generation digest and no request is built from it. Under
the single-ledger-per-network deployment precondition, one identity therefore
covers a wallet's round, bundle, and target across endpoint or chain-id
configuration changes that still address that network's same ledger.
Reconfiguration cannot reserve a second generation against the same persisted
VAN. A configuration change to an independent ledger is not supported.
An unresolved predecessor is bundle-wide. Only a terminal predecessor permits
later work in that bundle. The lifecycle admits no vote or batch while the
bundle's delegation row is unresolved, and it refuses a delegation reservation
once a confirmed vote or batch exists in the bundle, so an unresolved
delegation row can never sit beside a confirmed successor.

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
`identity.vote_round_id`, `identity.bundle_index`, and `identity.kind`,
followed by `identity.proposal_id` for a singleton or
`identity.ordered_batch_digest` for a batch. There is no vote-chain-id field.
Delegation fields then appear in this order:

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

- delegation: `e04a9aab05cc403c3e4fd5818c38439a49da4239f22726ba7a8331ac5dcd4145`;
- singleton vote: `e69db505c93cb02ab3e20c81322d1101da17e2c826ff167d1ae61a8d59551048`;
- ordered two-vote batch: `304c59580189347446783a472ef6489751f3fd100578a8667af97ef73bee7335`.

Their complete typed fixtures are maintained by
`generation_digest_v1_matches_frozen_vector`; changing a fixture or digest is
a generation-format version change, not an ordinary refactor.

Those three values are recorded outputs and cannot by themselves detect a
framing change, so the encoding is stated independently:
`generation_transcript_encodes_exact_framing_bytes` asserts the literal
transcript byte sequence, and `identity_transcript_binds_no_vote_chain_id`
asserts the complete identity prefix. Both must be satisfied before a recorded
digest is trusted.

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

There is exactly one `chain_submissions` row for a semantic generation. The
durable primary key is derived from the identity alone, and one total
uniqueness constraint covers the decoded identity columns, so two reservations
for one submission cannot coexist. Creation and every durable state or field
mutation occur in immediate transactions.

Every row carries the generation digest it was reserved for; the column is not
nullable. Reservation acquires every applicable batch-member identity lock in
canonical order before deriving or inserting a generation.

The row stores only:

- the typed identity;
- generation digest;
- durable state;
- one optional canonical candidate transaction hash;
- monotonic count of committed POST reservations;
- an optional immutable tracking-start timestamp;
- a bounded redacted diagnostic;
- final confirmation source;
- final VAN and vote-commitment positions in generation order; and
- creation and update timestamps.

The creation timestamp is fixed. Every lifecycle mutation clamps the update
timestamp against its durable value, including exact-tree confirmation and the
combined candidate-retirement-and-retry reservation, so wall-clock rollback
cannot make `updated_at` decrease.

The latest bounded lookup diagnostic for `Tracking` is durable operational
context. A reopen must return the same diagnostic until a later observation or
terminal transition replaces it; persistence cannot silently discard it.

Final ordered positions use a closed typed encoding, not descriptor JSON. All
positions are `u64` in the lifecycle and must fit SQLite's signed integer range.
Position zero is valid. Schema checks require a 32-byte digest for every row,
and the generation digest is immutable.

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
claim about the transaction's chain outcome. A historical
version-17 hash is never a candidate: it was never checked against a chain
result, and it remains only in its unchanged domain column.
One canonical candidate or hash-confirmed transaction belongs to at most one
native semantic generation. If POST acceptance returns a hash already owned by
another generation, the new reservation becomes hashless `Recovering` with an
invalid-protocol diagnostic and the lifecycle does not poll or confirm that
hash for the new generation. Confirmation rechecks ownership in the same
transaction as the terminal projection.
Diagnostics are bounded, valid UTF-8, escaped, and redacted before storage.
Raw response bodies and sensitive cryptographic material are never persisted in
diagnostics or emitted through ordinary logging.

Domain transaction hashes and positions are written by atomic confirmation.
Runtime planning and status reads begin with `chain_submissions` whenever a row
exists; domain columns cannot override its state.

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
- `Tracking`: a usable candidate hash is known and no durable recovery
  ambiguity exists. Reconciliation only polls that hash.
- `Recovering`: ordinary candidate polling is no longer sufficient to resolve
  the outcome because a request may have been dispatched without a usable hash
  or the bounded tracking window expired inconclusively. An optional candidate
  hash is polled before tree recovery. The state is sticky until confirmation
  or explicit deletion.
- `Confirmed`: chain success and all required local confirmation updates are
  durable. Its source records what the confirmation rests on: `hash` or
  `tree`, with the exact generation positions.
- `Rejected`: a compatibility-only terminal row retained for previously
  persisted lifecycle data. Current runtime rejection codes are diagnostic
  because they may depend on node state or chain time; new runtime work does
  not enter this state.

The normal transitions are:

```text
new -> Submitting
Submitting -> Tracking       usable success hash
Submitting -> Recovering     possible dispatch without usable hash
Submitting -> Recovering     chain rejection code
Tracking -> Tracking         hash still pending
Tracking -> Recovering       bounded tracking window expires inconclusively
Tracking -> Confirmed        committed success and atomic persistence
Tracking -> Recovering       committed failure, failed candidate cleared
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
                              |                           `-- committed failure --> Recovering
                              |-- possibly dispatched --> Recovering
                              |                           |-- candidate, scan, or retry --> Recovering
                              |                           `-- candidate success or exact tree layout
                              |                               --> Confirmed
                              |-- chain rejection code --> Recovering
                              `-- definitely unsent first attempt --> no row

abandoned Submitting on restart -- normalize --> Recovering
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
confirmation data and explicit round or account deletion. Conflicting terminal
data is an invariant error and writes nothing.

## Reservation and transport classification

Before releasing any POST byte, the lifecycle:

1. derives the recovery-independent identity lock set from the request, then
   acquires the round/account gate, bundle lock where applicable, and those
   submission-identity locks in canonical order;
2. loads the authoritative row;
3. loads and locks the generation inputs and derives the generation digest and
   expected layout;
4. creates the `Submitting` row, or validates the existing same-generation
   `Recovering` row;
5. increments the attempt count for the request; and
6. commits the reservation.

For a recovery retry, steps 4 through 6 additionally consume the private
single-use authorization produced by the immediately preceding valid no-match
pass. They clear any inconclusive candidate and increment the attempt count in
the same immediate transaction. There is no standalone candidate-retirement
mutation and an empty candidate slot is not retry authorization.

Row lookup and insertion share the same canonically ordered identity locks and
immediate transaction, so a concurrent singleton or batch call cannot bypass or
replace an existing row.

If reservation fails, dispatch does not occur. A process-local in-flight guard
prevents cleanup, replacement, or deletion from racing response
classification.

Only transport code can classify a failure as `DefinitelyUnsent`, and only
before request bytes are released to a network stack that may deliver them.
Cancellation before that boundary is also definitely unsent.
The transport exposes that handoff through a one-way dispatch marker. The
coordinator may remove a fresh reservation on interruption only while the
marker remains clear; once marked, interruption is possibly dispatched even if
the request future is then cancelled.

For a first attempt, definitely-unsent failure removes the fresh `Submitting`
reservation; it does not create chain rejection or ambiguity. For a retry from
`Recovering`, it leaves the row hashless and `Recovering`, retains the
reservation in the monotonic attempt count, and does not restore either the
retired candidate or the consumed no-match authorization. Another retry
requires another valid no-match pass. Attempt count is diagnostic and is never
decremented, refunded, or used as a permanent retry gate.
When bounded endpoint failover follows a definitely-unsent fresh attempt in
the same invocation, the next committed reservation carries the next ordinal;
removing the definitely-unsent row does not cause a later committed
reservation in that invocation to be counted as attempt one again.

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

A chain rejection code transitions `Submitting` to bound hashless
`Recovering`. Numeric rejection codes are diagnostic rather than proof that a
generation can never succeed: validation can depend on the receiving node's
local anchor state or on chain time. A rejection while already `Recovering`
likewise cannot settle the earlier possible dispatch. Because the protocol does
not specify that an error hash identifies an earlier accepted transaction, a
hash in an error response is not confirmation evidence.
Until the release prerequisite above is implemented, this row blocks later
submissions that consume its unknown successor VAN. Same-generation retry alone
does not resolve a generation whose anchor has become permanently stale.

Within each lifecycle invocation, POST attempts, endpoints, body sizes, request
durations, and backoffs are bounded by configuration with safe finite maxima.
Redirects are not followed. A later invocation may reconcile and retry after
another valid no-match pass regardless of the monotonic historical attempt
count. A no-match recovery retry shares the current invocation's POST-attempt
budget: if an earlier POST in that invocation consumed the last attempt, the
scan leaves the row `Recovering` without another reservation or POST. If
another attempt remains, the coordinator waits the configured backoff for the
preceding invocation-local attempt before consuming the recovery authorization
and reserving the retry. A pre-existing `Recovering` row begins a later
invocation with a fresh bounded budget and needs no backoff before that
invocation's first POST. Retries are allowed only for the same semantic
generation.

## Reconciliation and retry

One lifecycle facade provides typed delegation, vote, and batch entry points.
It owns submission, polling, recovery, and confirmation; planners do not
compose lower-level mutation APIs.

Reconciliation is state-driven:

- `Submitting` left by a crashed process becomes `Recovering`.
- `Tracking` polls its candidate hash and never scans the tree. If a configured
  finite tracking window expires without a definitive result, it atomically
  becomes `Recovering` while retaining the candidate hash.
- a `Recovering` row polls its candidate hash first. If hash polling does
  not confirm, the lifecycle may perform one bounded tree recovery pass.
- `Confirmed` and compatibility `Rejected` rows perform no network mutation,
  whatever the confirmation source.

A pending or temporarily unreadable candidate remains available for later
polling. In `Recovering`, it does not disable tree recovery and prohibits
another POST until it is either committed unsuccessfully or retired after a
completed valid no-match tree pass. A committed-success candidate proceeds to
confirmation. A `Tracking` candidate that is committed unsuccessfully becomes
hashless `Recovering` with the committed-failure diagnostic; a `Recovering`
candidate that is committed unsuccessfully is atomically cleared while the row
remains `Recovering`.

After candidate polling and a bounded no-match tree pass, the lifecycle
receives one private process-local authorization for the captured identity,
generation, host operation epoch, and continuously held round, bundle, and
identity locks. The authorization is not persisted, cloned, returned, or
reconstructed from a hashless row. It is consumed only by one immediate
transaction that validates the same `Recovering` row, atomically clears
any remaining inconclusive candidate, and increments the attempt count to
reserve one same-generation retry. The transaction consumes the no-match
authorization. The authorization expires on cancellation, error, return, lock
release, or process exit. A later usable hash fills the empty candidate slot,
but sticky recovery remains in force.

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
- `Recovering` schedules candidate-first reconciliation and tree
  recovery, then same-generation retry when permitted;
- `Confirmed` enables dependent domain and helper work;
- a compatibility `Rejected` row schedules no reconciliation; and
- absent rows permit fresh work if bundle causality allows it.

An unresolved generation blocks only later work that consumes its unknown
successor VAN. Independent bundles remain schedulable.

## Sticky recovery and tree matching

Tree recovery is authorized only by a durable `Recovering` row. It never runs
for ordinary `Tracking` even when its hash is pending, or for fresh unsubmitted
work. A pending candidate carried by a `Recovering`
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
The supported hash-confirmation event contains exactly one round attribute
under either supported alias and exactly one position attribute. Duplicate
attributes, both round aliases, conflicting values, multiple matching events,
malformed positions, or positions outside SQLite's range are not evidence.

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
`Pending(Recovering)` may carry a candidate hash and preserves that tree
recovery remains authorized. It may carry no candidate after an atomic
retirement-and-reservation or committed-failure clearing, without implying that
a retired transaction failed or that another retry is already authorized.
`Rejected` means a compatibility row is terminally rejected. New HTTP 422 and
committed-failure observations return `Pending(Recovering)` instead, preserving
the bound generation for exact-tree recovery and same-generation retry.

`Cancelled` is returned only when cancellation occurs before possible dispatch
and no stronger durable state exists. Cancellation never hides `Tracking`,
`Recovering`, `Confirmed`, or `Rejected`. A call cancelled
on entry loads the authoritative durable state under the normal lifecycle
locks. If it finds an abandoned `Submitting` row, it must atomically normalize
that row to `Recovering` and return `Pending(Recovering)`; the possibly
dispatched request is stronger evidence than the current call's cancellation.
For a batch, an existing authoritative batch row is loaded and returned before
consulting the caller's possibly stale member roster.
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
a batch, every member's singleton identity. The bundle lock prevents two
proposals from deriving successors from the same VAN. Different bundles remain
independent.
For active batch admission, the request's complete ordered roster supplies the
recovery-independent identity lock set. Under those locks, admission loads the
persisted roster and requires an exact ordered match before inserting a batch
row. A missing, truncated, reordered,
duplicated, or otherwise changed roster fails without touching an identity that
was not locked. The request rejects rosters above the 15-action protocol
maximum before allocating lock identities. No persisted roster is read before the operation
and identity locks.
The coordination authority is owned by the database authority, so constructing
multiple coordinators for one store cannot create disjoint lock registries.
Cleanup and deletion acquire the same round gate exclusively and treat an
active shared lifecycle lease as busy.

Once a singleton request is possibly dispatched, its proposal and choice are
locked. Once a batch is possibly dispatched, member proposals, choices, count,
order, and batch digest are locked. Delegation setup, nullifiers, proof inputs,
and VAN randomizer are likewise locked. Re-selecting the same generation is
idempotent; changing it is rejected.

A compatibility terminal rejection does not release an atomic batch's member
locks. The rejected row still projects its phase through the signed recovery
rows that define its ordered roster, so a conflicting ballot-intent change must
fail before clearing vote recovery or helper-delivery records. A rejected
singleton does not depend on a recovery-derived roster and is not additionally
locked by this batch-roster rule.

Confirmation does not release those locks. A vote that a generation has already
placed on chain cannot have its ballot intent changed, whatever the confirmation
source. Independently, any vote carrying a version-17 transaction hash or
vote-commitment position locks its choice from the domain columns, so
pre-upgrade evidence is protected without a lifecycle row.

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
and `Confirmed` row. For bound generations they preserve
all domain material required to reconcile, retry, prove, or apply that
generation, including:

- delegation setup, proofs, nullifiers, and VAN randomizer;
- vote and batch recovery material;
- locked proposal choices, batch membership, and order;
- current and generation-specific VAN/VC positions; and
- helper plans and durable helper-delivery rows bound to the generation.

There is no standalone recovery-clear operation or
`clear_recovery_state` primitive. Ordinary cleanup and reset preserve recovery
material, helper plans, and all helper-delivery history while the owning round
remains live.

An unresolved row cannot be pruned merely because it has no candidate hash.
Hashless `Recovering` is exactly the case that requires preservation. Bundle
indexes are never renumbered, and imported capability bundle sets remain
indivisible.

Compatibility `Rejected` generations may release their exact unused recovery
material only when no earlier unresolved generation or later dependent
generation needs it. Cleanup must not infer safety from legacy domain hashes
independently of the authoritative row.

Explicit round or account deletion is the destructive escape hatch. It closes
the matching operation gate before checking active work, prevents new entrants,
drains shared holders, and retains exclusive access through deletion. It
returns `Busy` while work remains. Deletion removes local evidence but cannot
undo a transaction that may already be on chain. Round deletion removes the
gated round directly, and account deletion removes every gated round for the
account. Their foreign-key cascades remove the owned recovery and helper
records; neither operation selectively clears those records from a live round.
Round-wide reset may still clear independent unsigned setup bundles: its single
atomic update excludes exactly the bundle indexes owned by any submission row,
rather than letting evidence in one bundle block cleanup of every other bundle.
Version-17 round ids that cannot form a canonical native lifecycle identity
have no possible in-process lifecycle holder; pruning and explicit round
deletion skip that impossible gate and remain available.

## Version 17 to version 18

Version 17 is the released migration source. Version 18 is unreleased, so its
schema is replaced in place:

- the final `chain_submissions` definition is part of `17 -> 18`;
- `001_init.sql` creates the same schema;
- `CURRENT_VERSION` remains 18; and
- there is no `18 -> 19` migration.

The `17 -> 18` step is schema only. It creates `chain_submissions` and imports
nothing: every version-17 row of `votes`, `bundles`, and `rounds` is preserved
byte for byte, and no lifecycle row is created for any pre-upgrade evidence,
whatever its shape. The lifecycle therefore never owns a submission it did not
reserve itself.

Completed pre-upgrade rounds stay displayable through the domain columns. A
vote with no `chain_submissions` row projects its workflow phase from
`tx_hash`, `vc_tree_position`, and the presence of recovery material:
`Confirmed` when all three are present, `Submitted` with a hash, `Committed`
with recovery material only, and `Prepared` otherwise. Round snapshots expose
the stored choice, hash, and positions from the same columns. Whenever an
authoritative row exists it wins: any unresolved row reports
`SubmissionManaged` and a confirmation reports `Confirmed`.

Upgrading a database that holds an in-flight version-17 submission is
unsupported. Such a vote still projects as `Submitted` and, when its recovery
material is present, the session plan still emits the host-driven legacy poll
step, exactly as version 17 did, but
the specification makes no guarantee about its outcome and the lifecycle
neither owns nor reconciles it. Version-17 hash values remain byte for byte in
their domain columns; runtime lookup never treats them as candidates.

Version-17 round ids that cannot form a canonical lifecycle identity remain
domain data that is prunable and explicitly deletable.

The migration changes `user_version` in the same transaction as the DDL, so a
process killed during migration leaves either the untouched version-17
database or a complete version 18. Fresh and migrated version-18 schemas must
match. A database created with an older unreleased version-18 shape is rejected
by schema fingerprint with an explicit recreation error; it is not silently
upgraded. The fingerprint compares the complete canonical `chain_submissions`
table, every owned uniqueness index, and every immutability/monotonicity
trigger.

A vote's authoritative row is its own singleton row or the `vote_batch` row that
contains it. Because a batch binds once and owns no per-member rows, membership
is not read from a stored roster: the projection re-derives the batch
generation from the persisted signed members and requires the persisted
generation digest to match before applying the batch's phase to any member.
Those signed members remain immutable while any authoritative batch row,
including a rejected row, still relies on them. A mismatch, a noncanonical
round id, or a vote claimed by two batches is an invariant error rather than a
silently different phase. Session plans emit no submit, poll, or reconstruction
step for a lifecycle-owned or terminal row.

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
- a chain rejection code produces bound hashless `Recovering` with a redacted
  diagnostic;
- committed failure clears the tracked candidate into `Recovering`;
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
  limits even when a fresh ambiguous POST immediately proceeds through a
  no-match tree scan; later invocations may independently reconcile and retry;
- restart after the combined transaction conservatively requires a new
  completed valid no-match pass before retry;
- an originally hashless `Recovering` row likewise cannot POST before a
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
  committed-failure clearing neither claims failure nor authorizes retry;
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
- active singleton and batch generations reject conflicting ballot-intent
  changes before recovery or helper-delivery material is touched;
- concurrent work cannot reserve two generations for one identity;
- bundle locking prevents two successors from consuming the same VAN;
- a confirmed vote or batch refuses a later delegation reservation for its
  bundle before derivation or dispatch;
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
- cancelled batch entry returns an existing authoritative batch before checking
  a stale caller roster;
- active admission also normalizes an abandoned batch `Submitting` row before
  attempting roster derivation, so missing or corrupt recovery bytes cannot
  strand it in `Submitting`;
- entry cancellation over `Tracking`, `Recovering`, `Confirmed`,
  `Confirmed`, or `Rejected` returns the authoritative stronger result;
- cancellation after reservation but before dispatch releases no bytes and
  removes only the fresh definitely-unsent reservation;
- failure to persist that normalization reports an operational failure with
  the known possibly-dispatched state and never returns `Cancelled`;
- cancellation after dispatch preserves `Recovering`; and
- cancellation after the confirmation commit point cannot suppress
  persistence.

### Migration, cleanup, and API surface

Tests cover:

- version-17 domain evidence of every shape (confirmed vote, hash-only vote,
  delegation-only bundle, committed recovery material) migrates with every
  domain column byte-identical and creates no submission row;
- migrated version-17 votes project `Confirmed`, `Submitted`, and `Committed`
  from the domain columns, and the round snapshot exposes their stored choice,
  hash, and positions;
- the version-18 schema rejects a null or short generation digest and every
  legacy confirmation source;
- a proved but unsubmitted version-17 delegation migrates with no submission
  row and remains fresh delegate work;
- a version-17 round id that cannot form a canonical identity creates no
  submission row and leaves its domain evidence untouched;
- a hashless `Recovering` row remains `Recovering` after a retry receives
  a definite rejection, leaving exact tree recovery available;
- fresh and migrated v18 schema equivalence and stale-v18 fingerprint
  rejection, including missing columns, indexes, and triggers;
- ordinary cleanup and reset preserve every unresolved generation, its
  retry/recovery data, helper plan, and complete delivery history;
- round reset preserves a protected bundle while clearing an independent
  unsigned bundle, and noncanonical legacy round ids remain prunable and
  explicitly deletable;
- compatibility hash, VAN, VC, and confirmation writers acquire an immediate
  write transaction before testing lifecycle ownership, so admission cannot
  commit in a precheck-to-write gap;
- no standalone recovery-clear API or storage primitive exists, and only
  exclusive round or account deletion removes owned recovery and helper rows;
- partial pruning refuses protected ranges without renumbering bundles;
- deletion gates block new work and wait for active work;
- planners and recovery snapshots derive from the authoritative row; and
- removed legacy mutation APIs fail compile-time surface checks.

These tests are the review contract for changes to chain submission behavior.

Phase 6 recovery coverage is anchored by
`exact_recovery_confirms_without_a_hash_and_clamps_timestamp`,
`no_match_recovery_waits_for_backoff_and_clamps_reservation_timestamp`,
`recovering_candidate_is_polled_before_no_match_retry_reservation`,
`definitely_unsent_recovery_retry_keeps_reservation_and_requires_a_new_scan`,
`malformed_tree_after_candidate_first_poll_retains_candidate_and_never_retries`,
`exact_vote_layout_recovers_adjacent_ordered_positions`,
`complete_no_match_authorizes_only_the_captured_generation_and_candidate`,
`duplicate_complete_layout_is_ambiguous_not_confirmation`,
`incomplete_pagination_produces_no_authorization`,
`full_tree_capacity_fits_the_fixed_request_and_byte_ceilings`, and
`tree_confirmation_is_atomic_clamps_timestamp_and_survives_reopen_without_a_hash`.

Generation and confirmation coverage is anchored by
`generation_transcript_encodes_exact_framing_bytes`,
`identity_transcript_binds_no_vote_chain_id`,
`generation_digest_v1_matches_frozen_vector`,
`generation_digest_binds_semantics_and_ignores_confirmation_positions`,
`batch_generation_digest_and_layout_preserve_action_order`,
`expected_layouts_follow_signed_action_order`,
`persisted_vote_generation_survives_confirmation_projection`,
`typed_confirmation_uses_the_full_sqlite_position_range`,
`typed_batch_confirmation_rolls_back_when_a_later_member_conflicts`,
`records_vote_confirmation_atomically`, and
`records_vote_batch_confirmation_replay_and_helper_positions`.

Private-coordinator lifecycle coverage is anchored by
`reservation_commits_before_post_and_accepted_hash_is_tracking`,
`delegation_uses_the_same_lifecycle_and_atomic_confirmation_path`,
`reservation_failure_dispatches_nothing_and_writes_nothing`,
`definitely_unsent_failover_is_bounded_and_ambiguity_stops_it`,
`possible_dispatch_is_sticky_recovery_and_never_redispatches`,
`failed_post_classification_reports_known_possible_dispatch`,
`failed_tracking_reconciliation_reports_the_durable_state`,
`tracking_deadline_survives_polling_and_coordinator_restart`,
`chain_rejection_preserves_bound_recovery_and_redacts_diagnostics`,
`committed_failure_moves_tracking_to_recovery_and_clears_recovery_candidate`,
`hash_confirmation_updates_submission_and_projection_atomically`,
`failed_confirmation_rolls_back_submission_and_projection`,
`cancellation_after_confirmation_commit_point_cannot_suppress_persistence`,
`changed_generation_is_rejected_before_reconciliation`,
`rejected_recovery_accepts_only_the_same_generation`,
`same_identity_concurrency_releases_only_one_post`,
`same_bundle_blocks_a_successor_until_the_predecessor_is_authoritative`,
`active_predecessor_blocks_the_next_bundle_generation`,
`atomically_confirmed_predecessor_allows_the_next_bundle_generation`,
`confirmed_predecessor_allows_the_next_bundle_generation`,
`confirmed_vote_refuses_a_later_delegation_reservation`,
`confirmed_successor_refuses_delegation_reservation`,
`independent_bundles_progress_while_another_post_is_blocked`,
`coordinators_for_one_store_share_the_same_lock_authority`,
`exclusive_round_access_is_busy_until_lifecycle_work_finishes`,
`batch_roster_mismatch_never_creates_a_reservation`,
`batch_request_supplies_its_complete_recovery_independent_lock_set`,
`batch_request_enforces_protocol_action_bounds`,
`persisted_batch_roster_mismatch_never_checks_unlocked_members`,
`abandoned_batch_normalizes_before_roster_derivation`,
`exclusive_round_gate_prevents_batch_roster_reads`,
`active_batch_admission_requires_a_persisted_roster`,
`omitted_batch_member_fails_before_its_unlocked_row_is_read`,
`request_kind_must_match_the_identity_target`,
`candidate_hash_reuse_across_generations_fails_closed_without_lookup`,
`duplicate_candidate_hash_becomes_hashless_recovery`,
`tracking_and_atomic_confirmation_survive_reopen`,
`ambiguous_confirmation_attributes_leave_tracking_authoritative`,
`event_round_requires_exactly_one_supported_attribute`,
`confirmation_attribute_requires_exactly_one_value`,
`matching_event_for_round_must_be_unique`,
`cancelled_entry_normalizes_abandoned_submitting_without_network_work`,
`cancelled_batch_entry_requires_no_recovery_or_roster_derivation`,
`cancelled_batch_entry_returns_authoritative_batch_before_stale_roster`,
`cancellation_after_reservation_before_dispatch_removes_fresh_reservation`,
`failed_cancelled_entry_normalization_reports_possible_dispatch`,
`failed_active_entry_normalization_reports_possible_dispatch`,
`failed_abandoned_normalization_reports_possible_dispatch`,
`lifecycle_timestamps_clamp_when_wall_clock_moves_backward`,
`active_singleton_generation_locks_intent_and_recovery_material`,
`active_batch_generation_locks_every_member_intent`,
`delete_skipped_bundles_preserves_chain_submission_evidence_atomically`,
`v17_projectionless_proved_delegation_remains_fresh_work`,
`v17_domain_evidence_is_preserved_and_creates_no_submission_rows`,
`migrated_v17_votes_project_domain_phases`,
`fresh_v18_schema_requires_a_bound_generation`,
`noncanonical_v17_round_ids_create_no_submission`,
`v18_fingerprint_rejects_missing_columns_indexes_and_triggers`,
`reset_voting_session_state_scopes_submission_protection_to_its_bundle`,
`reset_voting_session_state_preserves_proved_bundle_setup_fields`,
`noncanonical_legacy_round_ids_remain_deletable`,
`lifecycle_owned_delegation_and_vote_yield_no_legacy_steps`,
`bound_hashless_recovery_is_contained_until_tree_recovery_lands`,
`rejected_singleton_vote_never_yields_submit_or_poll_work`,
`rejected_vote_batch_never_reschedules_its_members`,
`rejected_delegation_never_yields_delegate_work`,
`round_snapshot_reports_lifecycle_owned_recovery`,
`lifecycle_owned_vote_locks_conflicting_intent`,
`public_vote_writers_reserve_before_validation_and_wait_on_contention`,
`cancellation_during_dispatch_persists_recovery`, and
`operation_epoch_change_during_dispatch_persists_recovery`.
