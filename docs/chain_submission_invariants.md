# Chain submission invariants

## Status and purpose

This document is the normative specification for vote-chain submission in
`zcash_voting`. Changes to the behavior described here must update this
document and its behavior-oriented conformance tests in the same change.

The design has one authoritative `chain_submissions` row for each semantic
generation. Locally prepared rows are created by the lifecycle before POST;
the one narrow exception is a structurally imported delegation capability,
whose package hash is lazily adopted into a poll-only row. No pre-upgrade
evidence is imported. The design does not use an attempt journal or a durable
scan workflow.

The normal path is:

```text
reserve Submitting -> POST -> store hash as Tracking -> poll -> Confirmed
```

If a POST may have been dispatched without a usable hash, the row first becomes
`Recovering` durably. The same advancement call then waits its configured
backoff, reserves another same-generation POST, and continues within its
invocation-local attempt budget without requiring tree reconciliation. Endpoint
selection follows the monotonic reservation ordinal modulo endpoint count, so
three attempts over endpoints A and B use A, B, A. A later invocation receives
a fresh bounded budget.

An accepted hash that is usable for this generation returns to ordinary
`Tracking`. A hash already owned by another generation is not usable and is
classified as possible dispatch without a hash. Exhausting the budget with
that collision or any other ambiguous final attempt is local uncertainty, not
chain evidence: the row stays hashless `Recovering` carrying the last attempt's
dispatch diagnostic, and a later invocation receives a fresh budget. Only chain
rejection code 2 received while the durable row still carries unresolved
dispatch evidence produces terminal `SubmittedWithoutHash`, and under
exact-tree advancement one complete no-match tree pass precedes even that.
This outcome is not confirmation and carries no hash or positions.
`Recovering` remains available for tracking-window expiry and exact-tree
recovery; tree recovery never runs for `SubmittedWithoutHash`.

Under exact-tree advancement a hashless dispatch-ambiguity row scans the tree
before any POST, so a generation that already landed is confirmed with its
positions rather than redispatched into a code-2 rejection. Only status-only
advancement, which has no scan, re-POSTs such a row directly.

## Scope and authority

The lifecycle covers local delegation, imported poll-only delegation,
singleton vote, and atomic vote-batch submission. It owns:

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
For a locally prepared delegation, the caller supplies only the 64-byte
SpendAuth signature produced at the wallet signing boundary. The caller does
not supply, reconstruct, or select the sighash used by chain submission. The
lifecycle loads the authoritative PCZT sighash and randomized verification key
from the locked bundle row and verifies the signature against both before any
reservation or network request.
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

Locally generated delegation proofs are single-flighted within that owning
process by `(wallet, round, bundle)`. A caller arriving while the same proof is
being generated waits, then reloads and validates the durable proof before any
chain-submission reservation. Failure releases the proof operation so a waiter
may retry from durable state. Different bundle identities remain independent
and may prove or advance concurrently. This coordination changes neither the
submission identity nor its generation digest, and it creates no durable
in-flight state of its own.
The delegation facade is the only public durable proof-generation workflow;
the database-level generator is crate-private and cannot bypass coordination.
The wallet is captured before lock admission and remains authoritative for
input validation, persisted-proof loading, PIR cache access, generation, and
proof persistence even if the host changes the database's selected wallet
while the operation is waiting or running. Supplied bundle notes and delegation
keys are validated against that captured wallet before any persisted proof is
accepted for reuse. Validation reproduces both the target-bound VAN commitment
and the zero-value output note commitment from the complete 43-byte Orchard
receiver. The latter binds the transmission key's encoded y-coordinate sign,
which the VAN's affine x-coordinate does not retain, so a same-network,
same-round target substitution cannot reuse another target's proof. Progress,
including the waiting notification, is delivered live and in emission order
from a dedicated delivery thread that the proof operation never waits on. A
reporter may therefore enter proof generation directly or dispatch it to
another thread; that work waits at most until the operation releases its lock,
which it does without waiting on the reporter. `ensure_proof` returns
only after every emitted event has been delivered. Different bundle identities
remain independent throughout.
Terminal submission rejection preserves and reuses the proof bound to the
rejected generation; proof preparation cannot replace it with new randomized
bytes.

The legacy round-wide phase is only a high-water display marker. If one bundle
has already advanced it to `VoteReady`, a different bundle may still complete
and persist its delegation proof. That proof transaction preserves the later
marker and commits the bundle-local proof; round phase never substitutes for
the bundle's durable proof and target-binding checks.

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
The full PCZT and caller-facing wire payload are not chain-submission
authority. They may be omitted after signing and are never parsed to recover a
sighash for submission. A present, absent, stale, or malformed returned PCZT
cannot override the bundle row; only the externally produced signature bytes
cross into the lifecycle request.

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

Imported delegation adoption uses the separate ASCII domain
`zcash_voting.chain_submission.imported_delegation.v1` followed by a NUL byte.
Its typed transcript contains the common delegation identity, the imported
`delegation.gov_comm`, and the package's canonical transaction hash. Adoption
is allowed only for the exact structural capability-import shape. The hash is
read from the locked database row, never supplied by the caller.

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
mutation occur in immediate transactions. An imported delegation's first
advance pass derives and inserts its `Tracking` row atomically with zero POST
reservations; every other new row starts with a pre-POST reservation.

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
another generation, the lifecycle does not poll or confirm that hash for the
new generation. It persists an invalid-protocol dispatch ambiguity as hashless
`Recovering` and, when another attempt remains in the current invocation,
continues through the same bounded retry loop, whether the colliding POST was
a fresh attempt or one reserved after exact-tree recovery. A collision that
consumes the final attempt leaves that hashless `Recovering` row for a later
invocation; it is never terminal. Confirmation rechecks ownership in the same
transaction as the terminal projection.
Diagnostics are bounded, valid UTF-8, escaped, and redacted before storage.
Raw response bodies and sensitive cryptographic material are never persisted in
diagnostics or emitted through ordinary logging.

Domain transaction hashes and positions are written by atomic confirmation.
Runtime planning and status reads begin with `chain_submissions` whenever a row
exists; domain columns cannot override its state.

A confirmation writes the witnesses its source has, and the two sources have
different ones. Hash confirmation writes `votes.tx_hash`; tree confirmation
writes `votes.vc_tree_position` and no hash, which the schema requires of it
(`CHECK (confirmation_source != 'tree' OR confirmed_transaction_hash IS
NULL)`). Neither witness is therefore sufficient alone, and a query asking
whether a vote reached the chain must accept either: `tx_hash IS NOT NULL OR
vc_tree_position IS NOT NULL`. Reading the hash alone answers a narrower
question — whether *this* confirmation came with a hash — and a tree-confirmed
vote answers it "no" permanently, because the hash it is missing does not
exist.

That distinction is load-bearing in both directions, and getting it wrong has
cost this repository seven defects. A completion check reading the hash alone
fails closed and stalls: the round waits forever for a witness that will never
appear. A *refusal* reading the hash alone fails open and permits what it
exists to prevent — rebuilding a vote already on chain into a competing
generation, accepting a ballot intent that disagrees with one, or overwriting
its choice and commitment. The second class is the more dangerous, because a
stalled round announces itself and a permitted rebuild does not.
`no_query_treats_a_transaction_hash_as_the_only_proof_a_vote_finished` scans
the crate for the shape all seven shared.

Vote-tree sync reads this record for a different question and inherits its
distinctions. A bundle's delegation VAN is retired from the tree expectation
once a POST that may have spent it was released, so the durable row answers
where `votes.tx_hash` cannot — but only where the row can actually settle the
question, and terminality is what decides that.

`Rejected` settles it: no observation transitions out of that state and a
recovery retry may be reserved only from `Recovering`, so the row can never
acquire an outstanding POST after the fact, and a definite rejection spent
nothing. It is never deleted either, so counting it would retire the
expectation for the rest of the round and skip both the leaf-content and the
leaf-presence check — the fail-open direction again, one layer down
(`a_terminally_rejected_vote_keeps_its_bundles_van_expectation`).

A `Recovering` row carrying `ChainRejected` does **not** settle it, and is
therefore read as possibly-spent despite the same last classified outcome.
Exact-tree recovery reaches any `Recovering` row whatever its diagnostic, and
on a no-match it reserves a retry and releases a POST while leaving `state`
and `diagnostic_kind` unchanged; only the reservation counter moves. Between
that reservation and its classification the row is indistinguishable from a
quiescent rejection, so restoring the expectation would fail tree sync against
the successor leaf, aborting the cast that would classify the retry — the
self-sustaining deadlock this rule exists to break
(`a_rejection_still_in_recovery_retires_the_van_expectation`). Distinguishing
the two would require the retry dispatch to be journaled against the
reservation counter, which the row does not record today.

## Durable states

The only durable states are:

```text
Submitting
Tracking
Recovering
SubmittedWithoutHash
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
  hash is polled before tree recovery. A canonical accepted hash from a
  hashless retry resumes `Tracking`; otherwise the row remains `Recovering`
  until confirmation, bounded hashless submission, or explicit deletion.
- `SubmittedWithoutHash`: chain rejection code 2 proved this generation's
  nullifiers spent after unresolved dispatch, so an earlier dispatch landed,
  but no candidate hash or confirmation positions are available; under
  exact-tree advancement a complete tree pass found no layout first. It is
  terminal and schedules neither polling nor tree recovery. A generation that
  previously entered `Tracking` retains its immutable tracking-start timestamp.
- `Confirmed`: chain success and all required local confirmation updates are
  durable. Its source records what the confirmation rests on: `hash` or
  `tree`, with the exact generation positions.
- `Rejected`: a terminal row. Local POST rejection codes remain recovery
  diagnostics because they may depend on node state or chain time. A
  poll-only imported transaction enters `Rejected` when status proves that its
  one immutable candidate committed unsuccessfully.

The normal transitions are:

```text
new -> Submitting
Submitting -> Tracking       usable success hash
Submitting -> Recovering     possible dispatch without usable hash
Recovering -> Recovering     reserve and classify another ambiguous POST,
                             including the final one of an invocation
Recovering -> Tracking       hashless retry returns a usable success hash
Recovering -> SubmittedWithoutHash
                             retry code 2 after unresolved dispatch
                             (exact tree: after a no-match pass)
Submitting -> Recovering     chain rejection code
Tracking -> Tracking         hash still pending
Tracking -> Recovering       bounded tracking window expires inconclusively
Tracking -> Confirmed        committed success and atomic persistence
Tracking -> Recovering       local committed failure, failed candidate cleared
Recovering -> Recovering     candidate, retry, no match, or interruption
Recovering -> Confirmed      candidate success or exact tree layout

imported new -> Tracking     atomically adopt the stored package hash
imported Tracking -> Rejected
                             immutable candidate committed unsuccessfully
imported Recovering -> Rejected
                             immutable candidate committed unsuccessfully
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
                              |-- possible dispatch --> Recovering
                              |                           |-- candidate, scan, or retry --> Recovering
                              |                           |-- code 2 after unresolved dispatch
                              |                           |   (exact tree: after a no-match pass)
                              |                           |   --> SubmittedWithoutHash
                              |                           `-- candidate success or exact tree layout
                              |                               --> Confirmed
                              |-- chain rejection code --> Recovering
                              `-- definitely unsent first attempt --> no row

abandoned Submitting on restart -- normalize --> Recovering
```

For locally constructed submissions, a hashless `Recovering` row may transition
back to `Tracking` when a same-generation retry returns a canonical accepted
hash. It may transition to `SubmittedWithoutHash` only under the numeric
code-2 rule above; exhausting an invocation's budget never does. Imported
poll-only delegation never POSTs and retains its existing status-only behavior.

Other local recovery observations do not transition to `Tracking` or
`Rejected`. In particular, a rejection, committed-failure candidate,
cancellation, or empty scan cannot erase ambiguity. A hashless `Recovering` row
may reserve the next bounded POST directly under status-only advancement when
its diagnostic came from the possibly-dispatched path, either
`AmbiguousDispatch` or `InvalidProtocolResponse`; exact-tree advancement
completes a tree pass first and reaches a POST only through no-match
authorization. A hashless row created by a definite rejection carries
`ChainRejected` and never reserves an ambiguous retry. A pending or
unreadable candidate is never overwritten and blocks another POST until it is
atomically retired by the existing candidate-first reconciliation and valid
full-tree no-match authorization. A definitively committed-failure candidate
is atomically cleared without requiring that pass. Either candidate-clearing
operation leaves the row `Recovering` and tree recovery available.

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
   expected layout; for delegation this includes loading the stored 32-byte
   PCZT sighash and randomized verification key and verifying the supplied
   SpendAuth signature against them;
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
the request future is then cancelled. The SDK's own POST deadline is classified
against the same marker: a timeout that expires while the marker is still clear
is definitely unsent, because the transport never released the request, and a
timeout after the marker is possibly dispatched. A transport that reports its
own failure kind is trusted as reported; the marker only decides timeouts the
SDK imposed itself.

For a first attempt, definitely-unsent failure removes the fresh `Submitting`
reservation; it does not create chain rejection or ambiguity. For a retry after
dispatch ambiguity, it leaves the row hashless and `Recovering` and retains the
reservation in the monotonic attempt count. If the invocation budget permits,
the next retry needs only the configured backoff and another durable
reservation; it does not require a no-match tree pass. Attempt count is
diagnostic and is never decremented, refunded, or used as a permanent retry
gate.
When bounded endpoint failover follows a definitely-unsent fresh attempt in
the same invocation, the next committed reservation carries the next ordinal;
removing the definitely-unsent row does not cause a later committed
reservation in that invocation to be counted as attempt one again.

Everything after the dispatch boundary is `PossiblyDispatched`, including:

- timeout or interruption once the dispatch marker is set;
- cancellation after dispatch;
- response-body or decoding failure;
- unusable or hashless success;
- HTTP 408 or 429 after POST;
- gateway or other ambiguous transport failure; and
- a process crash while a `Submitting` reservation exists.

Possible dispatch without a usable hash must durably transition `Submitting` to
`Recovering` before any retry. On restart, an abandoned `Submitting` row is
conservatively changed to `Recovering`; the new process cannot prove that
request bytes were never released. After backoff, every retry is reserved
durably and uses reservation ordinal modulo endpoint count.

A canonical success hash not owned by another generation transitions either a
first attempt or hashless dispatch-ambiguity retry to ordinary `Tracking`.
An atomic batch response must also contain the canonical lowercase batch digest
matching the submission identity, for both acceptance and rejection. A missing,
malformed, noncanonical, or mismatched response digest is not evidence and is
classified as possibly dispatched without accepting the response hash.

A rejection with numeric module code 2 transitions to `SubmittedWithoutHash`
only when dispatch ambiguity actually preceded it; classification never
inspects or retains the untrusted response log. Within one invocation the
coordinator knows whether an earlier attempt was possibly dispatched. Across
invocations the durable row is the carrier: a `Recovering` row whose stored
diagnostic kind is `AmbiguousDispatch`, `InvalidProtocolResponse`, or
`TrackingWindowExpired` carries unresolved dispatch evidence, because each of
those records a POST that left the wallet and never resolved. A diagnostic of
`ChainRejected` records a definite outcome, a rejected POST or a candidate that
committed unsuccessfully, which spent nothing; code 2 after such a row is
handled like any other definite rejection.

Under exact-tree advancement, code 2 after unresolved dispatch runs one tree
pass before anything is persisted: a match confirms the generation with its
positions; a completed valid no-match discards its retry authorization, which
never dispatches, and persists `SubmittedWithoutHash`; a pass that cannot
complete or is interrupted leaves the row `Recovering` with its dispatch
evidence, so the next pass converges. Status-only advancement has no scan and
persists `SubmittedWithoutHash` at once. Other deterministic rejections
surface an operational error while preserving earlier durable ambiguity. A
definite rejection with no earlier ambiguity may retain the compatibility
`Recovering` behavior. A hash in an error response is never confirmation
evidence.

Within each lifecycle invocation, POST attempts, body sizes, request
durations, and backoffs are bounded by configuration with safe finite maxima.
Redirects are not followed. Attempt limits may exceed endpoint count; endpoints
cycle by reservation ordinal. A pre-existing hashless dispatch ambiguity begins
a later invocation with a fresh bounded budget and a newly committed
reservation, after a tree pass under exact-tree advancement. Retries are
allowed only for the same semantic generation.

### Host request executors

`HyperTransport<R: RouteHttp>` applies this classification for every host
executor. The executor calls the dispatch hook immediately before request
bytes can reach a network stack; the SDK treats any failure or deadline it
observes after that call as `possibly_dispatched` and before it as
`definitely_unsent`. An executor's post-dispatch phase is honored even without
the hook; a `BeforeDispatch` phase reported after the hook is honored only
from an executor that declares `hook_precedes_connection_setup` (the SDK's
`DirectRoute`, whose Hyper client fuses connection setup with the first write
and reports connect failures distinctly). An executor must fail closed when
its route is unavailable and must never fall back to a direct connection.

A caller may bound connection setup more tightly than the request as a whole,
through the request's `connect_timeout`. Such a budget only ever tightens the
connect deadline, never relaxes it: the derived lead stays the upper bound, so
connection setup still gives up before the whole-request backstop can fire and
a stalled connect stays a definite pre-dispatch failure.

Only the executor can enforce that budget, because only the executor observes
connection setup; the SDK never does. An executor therefore declares whether it
bounds setup by `connect_timeout` through `RouteHttp::enforces_connect_timeout`,
and only for such an executor does the SDK read a pre-dispatch failure at or
after the budget as the budget expiring rather than as the endpoint's answer.
An executor that ignores the budget is bounded by `timeout` alone and has its
pre-dispatch failures read as the definite answers they are, so a refusal that
merely arrived late is never reinterpreted as a timeout. This mirrors
`hook_precedes_connection_setup`: a classification the SDK cannot verify is
drawn only from an executor that declares the property it depends on.

The declaration is a property of how an executor was built, not of its type.
The SDK's `DirectRoute` declares enforcement only when constructed by `new` or
`with_http_connector`, which install the connection-setup deadline around the
connector; `with_connector` takes a fully configured connector as given, adds
no deadline, and so declares nothing. An executor must not declare enforcement
it does not perform: doing so is the one way a definite pre-dispatch refusal
that merely arrived late gets repeated.

Enforcement covers the whole of connection setup, connector readiness included,
not only the connection attempt itself. A connector that withholds readiness
past the deadline and then reports a failure would otherwise produce a
pre-dispatch failure arriving after the budget from a route that claims to
enforce it. Past the deadline the answer is that connection setup ran out of
time, whatever the connector would have said next.

Readiness is bounded by racing the connector against a timer that registers the
caller's waker, not by sampling the clock when readiness happens to be polled.
The distinction is the difference between a bound and an observation: a
connector that returns pending decides when it is polled again, so a clock read
on entry may never run after the deadline, and the stall would be caught only
by the looser whole-request backstop. An executor that declares enforcement
must bound readiness this way.

A connect deadline that expires is reported to the SDK distinctly from the
backstop firing. Both are "no answer yet" rather than an answer, but only the
connect case is also definitely unsent. Neither declaration can make an
ambiguous failure look safe: only `BeforeDispatch` is ever eligible, and that
already means no request byte left.

### PIR request budget

A PIR request spends one finite budget of 60 seconds against one endpoint,
split across at most two attempts: the first bounded at 5 seconds of connection
setup and 15 seconds overall, the second at 10 seconds of connection setup and
whatever remains of the budget. The two attempts share the budget rather than
each getting their own, so the total is no longer than the single deadline this
replaced. That ceiling is the per-endpoint one; `PirFleet` failover multiplies
it by the endpoint count.

The split exists because a wall clock cannot distinguish an endpoint that never
answers from a link that is merely slow. Bounding connection setup separately
can: its duration says nothing about how much work remains, so abandoning it
early costs a slow-but-progressing transfer nothing, while a bound on the
request as a whole must stay generous or a large tier over a slow link becomes
unreachable rather than late — every attempt restarts the transfer from zero,
so a schedule that is uniformly too short never converges.

Only running out of time is retried, whether the whole-request backstop or the
connect deadline. Every other failure is a definite answer about this endpoint
— a refused connection, a protocol error, a non-success status — and is
returned rather than repeated.

Repeating is permitted for PIR where it is not for helper or chain POSTs. A PIR
query is an idempotent read, so a second attempt cannot double an effect, and
re-sending the identical encrypted query tells the server nothing it did not
already have: which item is being fetched is exactly what PIR hides. A retried
attempt must therefore carry a byte-identical body. A connect timeout is
definitely unsent and so is repeatable on any method; the backstop case is
repeatable because it repeats only a read. The dispatch classification itself
is unchanged — PIR acts on it, it is not weakened for PIR.

The budget applies per endpoint and reaches the executor on every attempt, so a
host executor that enforces it gets the same fast-fail as the built-in route. A
host executor that does not is bounded by the whole-request deadline alone,
which still ends each attempt and still permits the retry; it forfeits only the
early abandon, never correctness.

Tests cover: a stalled attempt retried under the remaining budget; a stalled
connect retried, which a whole-request deadline alone cannot see; the connect
budget reaching the executor on each attempt; a late refusal retried from an
executor that enforces the budget and *not* retried from one that ignores it;
an immediate refusal and a non-success status each ending the request on one
attempt; the budget finite and no longer than the deadline it replaced; a
retried POST carrying the identical body; and a connect budget only ever
tightening the deadline (`http_transport/tests/pir_retry.rs`). That only a
deadline-wrapped `DirectRoute` declares enforcement is covered by
`only_a_deadline_wrapped_direct_route_claims_to_enforce_the_connect_budget`;
that a stalled connector readiness is abandoned at the deadline rather than at
the backstop by `a_stalled_connector_readiness_is_abandoned_at_the_connect_deadline`,
which awaits readiness rather than re-polling it so only a registered timer can
end the wait; and that the bound does not pre-empt a connector merely not ready
yet by `connector_readiness_within_the_deadline_defers_to_the_inner_connector`
(`http_transport/tests/route.rs`).

## Reconciliation and retry

One lifecycle facade provides typed local delegation, imported-delegation,
vote, and batch entry points. It owns submission, polling, recovery, and
confirmation; planners do not compose lower-level mutation APIs.

Reconciliation is state-driven:

- `Submitting` left by a crashed process becomes `Recovering`.
- `Tracking` polls its candidate hash and never scans the tree. If a configured
  finite tracking window expires without a definitive result, it atomically
  becomes `Recovering` while retaining the candidate hash.
- a `Recovering` row polls its candidate hash first. If hash polling does
  not confirm, the lifecycle may perform one bounded tree recovery pass.
- an imported delegation remains candidate-preserving and status-only in both
  `Tracking` and `Recovering`; it never scans or retries, and a committed
  failure becomes terminal `Rejected`;
- `Confirmed` and `Rejected` rows perform no network mutation,
  whatever the confirmation source.

A pending or temporarily unreadable candidate remains available for later
polling. In `Recovering`, it does not disable tree recovery and prohibits
another POST until it is either committed unsuccessfully or retired after a
completed valid no-match tree pass. A committed-success candidate proceeds to
confirmation. A `Tracking` candidate that is committed unsuccessfully becomes
hashless `Recovering` with the committed-failure diagnostic; a `Recovering`
candidate that is committed unsuccessfully is atomically cleared while the row
remains `Recovering` and likewise adopts the committed-failure diagnostic, so
the row no longer carries unresolved dispatch evidence.

After candidate polling and a bounded no-match tree pass, the lifecycle
receives one private process-local authorization for the captured identity,
generation, host operation epoch, and continuously held round, bundle, and
identity locks. The authorization is not persisted, cloned, returned, or
reconstructed from a hashless row. It is consumed only by one immediate
transaction that validates the same `Recovering` row, atomically clears
any remaining inconclusive candidate, and increments the attempt count to
reserve one same-generation retry. The transaction consumes the no-match
authorization. The authorization expires on cancellation, error, return, lock
release, or process exit. A canonical accepted hash returned by that hashless
retry transitions the row to `Tracking` and resumes normal candidate polling.
An authorized retry that is itself possibly dispatched, or that returns a hash
owned by another generation, durably replaces the row's diagnostic with the new
dispatch ambiguity before anything else happens. If the invocation budget
permits, the remaining attempts continue through the same backoff and durable
ambiguous-retry reservation as any other ambiguous retry, without another tree
pass; if the budget is exhausted, the row is left hashless `Recovering` with
that diagnostic. A later status-only invocation may reserve the next
same-generation POST directly; a later exact-tree invocation scans first.

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
- `SubmittedWithoutHash` schedules no polling, recovery, or retry; its stored
  diagnostic is exposed read-only through `RoundPlan::delegation_statuses`
  and `RoundRecoverySnapshot` so a host restarted after the terminal
  transition can still surface it (`submitted_without_hash_delegation_blocks_without_pending_recovery`,
  `round_snapshot_exposes_terminal_submission_diagnostics`);
- `Confirmed` enables dependent domain and helper work;
- a `Rejected` row schedules no reconciliation;
- absent rows permit fresh work if bundle causality allows it.

Advancement steps for a lifecycle-owned (`SubmissionManaged`) or `Submitted`
vote are derived from the authoritative rows alone and never depend on a
recorded ballot intent: a transaction that may already be on the wire is the
wallet's whatever the host has recorded so far. A missing intent is not a
conflict for a singleton or a batch member; only a differing or skipped intent
conflicts, and that conflict is rejected before any step is planned
(`lifecycle_owned_vote_without_ballot_intent_still_yields_an_advance_step`,
`lifecycle_owned_batch_without_ballot_intent_still_yields_an_advance_step`).
Recovery work and the per-vote recovery snapshot report, for an in-flight
batch, the hash held by the batch's own authoritative row, looked up by ordered
batch digest, because batch members own no lifecycle row and their projection
columns stay empty until confirmation
(`in_flight_batch_reports_the_batch_row_candidate_hash`).

Hosts must execute local delegation, singleton-vote, and vote-batch advance
steps through the matching `*_with_recovery` entry point with
`ChainRecoveryMode::ExactTree`. The same mode is safe for `Tracking` and fresh
work because tree recovery activates only for a durable `Recovering` row.
Routing these steps exclusively through the plain status-only methods is not
conformant: a hashless `Recovering` row would never scan or receive private
retry authorization, and a dispatch-ambiguity row would be redispatched before
the tree is consulted. Exact-tree advancement never POSTs such a row before
scanning. Imported delegation advancement remains status-only because it
cannot scan or redispatch, and exact-tree advancement of an imported row never
scans.

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

The scanner validates snapshot identity, heights, roots, ranges, pagination
progress, final size, canonical field encodings, and response bounds. A
nonempty block's absolute start index must identify its first leaf. An empty
checkpoint has no first leaf, so its start index is ignored while its ordered
height and unchanged tree root remain mandatory. The vote-sdk REST encoder may
omit zero-valued protobuf scalars; omitted indexes and heights are interpreted
as zero before those validations. Recovery uses the following fixed ceilings:

- `16,777,216` leaves, the full `2^24` vote-commitment-tree capacity;
- `6,709` leaf-range requests under vote-sdk's `5,000`-leaf page target, with
  each block returned atomically even when the block exceeds that target;
- `8 MiB` per response and `53,680 MiB` across the complete pass, including
  the initial snapshot-metadata response;
- `60 seconds` per request and `120 hours` across the complete pass; and
- `16 MiB` working memory beyond the expected layout and transport buffers.

The tree's documented month-scale design point is approximately one million
leaves, so the leaf ceiling retains more than sixteen-fold headroom and also
covers every structurally valid tree size. Responses are processed as a stream;
the complete tree is never retained in memory. The request ceiling follows the
deployed greedy whole-block pagination contract: two consecutive non-final
pages contain at least `5,001` leaves, so a full tree requires at most `6,709`
leaf requests. A server that advances through empty pages or otherwise departs
from that contract remains bounded and fails closed if it exhausts the request
ceiling. Before reading leaves, validated snapshot metadata must show that a
complete traversal fits. There is no smaller whole-pass work budget that can
repeatedly truncate a supported snapshot. Metadata claiming more than `2^24`
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
Every supported hash-confirmation event contains exactly one round attribute
under either supported alias, and exactly one event of the expected type may
match that round. Delegation and singleton vote events contain exactly one
`leaf_index` position attribute; the singleton value contains the final VAN
position followed by its vote-commitment position.

An atomic batch confirms by hash only from one round-matching
`cast_vote_batch` event containing exactly one each of `batch_digest`,
`batch_size`, `final_van_leaf_index`, `vote_commitment_leaf_indices`,
`proposal_ids`, and `van_nullifiers`. The digest must be canonical lowercase
32-byte hex and equal the locked ordered batch digest. The size must be within
the protocol range and equal the locked action count. Proposal IDs must equal
the locked proposal roster in signed action order. VAN nullifiers must each be
canonical lowercase 32-byte hex and equal the locked nullifiers in that same
order. The vote-commitment position list must contain exactly the declared
number of entries, and entry `i` must equal
`final_van_leaf_index + 1 + i`; overflow is not evidence.

Duplicate required attributes, both round aliases, conflicting values,
multiple matching events, malformed values, incomplete or reordered batch
lists, nonadjacent positions, or positions outside SQLite's range are not
evidence.

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
SubmittedWithoutHash
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
`SubmittedWithoutHash` means bounded dispatch is durably complete without a
usable hash. It is idempotent and terminal for SDK advancement, but it is not
`Confirmed` and exposes no hash or positions.
`Rejected` means the row is terminally rejected. Local HTTP rejection and
committed-failure observations return `Pending(Recovering)`, preserving the
bound generation for exact-tree recovery and same-generation retry. An
imported poll-only candidate that commits unsuccessfully returns `Rejected`;
there is no signed request that this wallet could safely retry.

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

### Episodes

`ChainSubmissionClient::advance_until_terminal_in_epoch` composes bounded
passes into one finite episode under a `ChainAdvancePolicy`. It is
crate-internal: `RoundExecutor` runs episodes as part of a step, so a host
never drives one and cannot start one that the round lock does not cover.
Every iteration is one
`advance_*_with_recovery` pass, so every public result above still describes
exactly one pass. An episode re-polls after `Tracking`, escalates to
`ExactTree` at most once after `Recovering` and otherwise ends as
`StillPending`, never repeats a pass after `SubmittedWithoutHash`,
`Rejected`, or `Confirmed`, and checks cancellation between passes. Persisted
work starts with `ExactTree` (`ChainAdvancePolicy::for_persisted_work`), as
the resume planner requires.

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
was not locked. The request rejects rosters above the 50-action protocol
maximum before allocating lock identities. No persisted roster is read before
the operation and identity locks.
The coordination authority is owned by the database authority, so constructing
multiple coordinators for one store cannot create disjoint lock registries.
Cleanup and deletion acquire the same round gate exclusively and treat an
active shared lifecycle lease as busy.

Once a singleton request is possibly dispatched, its proposal and choice are
locked. Once a batch is possibly dispatched, member proposals, choices, count,
order, and batch digest are locked. Delegation setup, nullifiers, proof inputs,
and VAN randomizer are likewise locked. Re-selecting the same generation is
idempotent; changing it is rejected.

A terminal rejection does not release an atomic batch's member
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

### Round executor lock scopes

`RoundExecutor` serializes `Delegate` and `AdvanceDelegation` per
`(wallet, round, bundle)` and every other step per `(wallet, round)`. The
planner never emits vote work for a bundle whose delegation is still
in flight, so the two scopes do not overlap on one bundle's lifecycle rows.

Which facts the executor re-verifies inside each act's own transaction or
lock, after planning from a read snapshot, is tabulated under "Check-then-act"
in [`round_orchestration_invariants.md`](round_orchestration_invariants.md);
for chain work the act is this lifecycle's generation compare-and-swap.

## Cleanup, pruning, and deletion

Ordinary cleanup and reset use the authoritative state under the same operation
and bundle locks. They preserve every `Submitting`, `Tracking`, `Recovering`,
`SubmittedWithoutHash`, and `Confirmed` row. For bound generations they preserve
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

## Version 18 to version 19

Version 19 adds the `submitted_without_hash` durable state. Existing version-18
databases are upgraded incrementally by rebuilding `chain_submissions` inside
the migration transaction while preserving every row and reinstalling its
indexes and triggers. The rebuild also carries a version-18 database created
with the original `proposal_id BETWEEN 1 AND 15` bound to the current 1..=50
bound; it is the only upgrade path for such a database, because the current
`002_chain_submissions.sql` is applied only to version-17 databases and the
version-18 fingerprint check rejects the older DDL. `002_chain_submissions.sql`
and `001_init.sql` create the same final schema for fresh databases
(`v18_submission_rows_migrate_incrementally_to_v19`,
`migrate_from_launch_version_matches_a_fresh_schema`).

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
material is present, the session plan emits `AdvanceVote` for it (and
`AdvanceDelegation` for a delegation with a domain hash and no row), exactly as
for any other submitted vote
(`submitted_legacy_vote_without_a_lifecycle_row_yields_an_advance_step`).
Executing that step reserves a fresh generation and redispatches the same
transaction bytes; the nullifier argument keeps this safe, since a rejection
lands in `Recovering` and exact-tree recovery then confirms the earlier
transaction by layout. The specification makes no further guarantee about its
outcome. Version-17 hash values remain byte for byte in their domain columns;
runtime lookup never treats them as candidates.

Version-17 round ids that cannot form a canonical lifecycle identity remain
domain data that is prunable and explicitly deletable.

The migration changes `user_version` in the same transaction as the DDL, so a
process killed during migration leaves either the untouched source database or
a complete version 19. Fresh and incrementally migrated version-19 schemas must
match. A database claiming version 19 with another shape is rejected by schema
fingerprint. The fingerprint compares the complete canonical
`chain_submissions` table, every owned uniqueness index, and every
immutability/monotonicity trigger.

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

## Version 19 to version 20

Version 20 adds `round_immediate_share`: one row per wallet and round naming
the helper share submitted immediately. Version 19 carried that designation
only as an `immediate` marker inside the designated vote's persisted helper
plan, and every reader re-derived or rescanned it; a designated proposal that
later left the roster had to be special-cased so a plan for the remaining
roster would not name a second share. The row is written once, in the same
transaction as the designated vote's own plan, and is immutable
(`round_immediate_share_immutable`). It is voided with the undispatched
generation it was made for, on the same condition that clears the vote's
helper plan (`clear_round_immediate_share_on_vote_generation_change`), and
cascades with the vote row. Confirmation does not void it.

The `19 -> 20` step creates the table and its triggers and adopts the first
marked plan of each round as its designation, always at domain share index 0
(`v19_immediate_markers_backfill_to_v20`). Every other row is preserved
byte for byte. Fresh and incrementally migrated version-20 schemas must match
(`migrate_from_launch_version_matches_a_fresh_schema`). The planner reads the
row through its round snapshot and derives a designation from the ballot only
while no row exists; helper-plan preparation reads the row and never
re-derives once it exists
(`the_designated_votes_own_plan_writes_the_designation_and_every_plan_reads_it`,
`the_designation_is_voided_with_its_undispatched_generation_but_not_by_confirmation`,
`a_persisted_immediate_designation_survives_its_proposal_leaving_the_roster`).

## Version 21 to version 22

Version 22 adds nullable `bundles.delegation_pczt`. Existing rows are preserved;
only newly prepared delegation setups persist the exact finalized PCZT.
Keystone signing requests reload it and validate it against the durable signing
fields. The PCZT is not added to the semantic generation digest and does not
become submission authority. Legacy software signing and chain reconciliation
continue using their existing authoritative fields. A missing original PCZT
prevents creating a new Keystone request, rather than authorizing a reset.

Fresh and migrated schemas match, and the existing domain-evidence preservation
test includes the new null column. `delegate/tests/keystone.rs` covers request
reuse across restart and concurrent creation. Existing setup replacement and
clearing update the PCZT together with its signing fields. Migration adds
storage only; it does not repair legacy proofs, signatures, or setup.

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
- batch acceptance and rejection retain a usable hash only when the response
  carries the matching canonical batch digest;
- inconclusive hash polling has a bounded promotion to candidate-preserving
  `Recovering`;
- polling, diagnostics, and restart do not reset the durable tracking window;
- promotion alone does not retire the candidate or permit redispatch;
- hash polling produces atomic `Confirmed`;
- local delegation accepts only a 64-byte SpendAuth signature, loads its
  sighash and randomized verification key from the locked bundle, and rejects
  malformed stored context or a non-verifying signature before reservation or
  network I/O;
- local delegation advancement is independent of returned PCZT bytes and
  caller-facing wire payload fields other than the SpendAuth signature;
- imported capability adoption starts directly in candidate-preserving
  `Tracking` with zero POST reservations and no signer;
- imported `Tracking` and `Recovering` never dispatch, scan, or retry, and a
  committed failure becomes terminal `Rejected` without becoming hashless;
- rejection code 2 after durable ambiguity produces terminal
  `SubmittedWithoutHash` by numeric code alone under status-only advancement
  (`nullifier_spent_after_ambiguity_is_terminal_and_idempotent`), while other
  deterministic rejections surface an error and preserve earlier ambiguity;
- `nullifier_spent_recovery_retry_after_unresolved_dispatch_is_terminal`,
  `nullifier_spent_after_definite_rejection_stays_recoverable`, and
  `nullifier_spent_after_definite_committed_failure_stays_recoverable` prove
  that the code-2 terminal rule follows the durable dispatch evidence: an
  authorized retry after an expired tracking window ends hashless after one
  more no-match pass, while one after a rejected POST or a committed failure
  surfaces an error and leaves the row recoverable;
- `nullifier_spent_after_unresolved_dispatch_confirms_from_tree_under_exact_tree`,
  `nullifier_spent_recovery_retry_after_unresolved_dispatch_confirms_from_tree`,
  `nullifier_spent_after_unresolved_dispatch_with_no_match_is_terminal_under_exact_tree`,
  and `nullifier_spent_tree_pass_failure_keeps_unresolved_dispatch` prove that
  exact-tree advancement runs one tree pass before persisting the code-2
  terminal state: a match confirms with positions, a no-match ends hashless
  without dispatching the discarded authorization, and a failed pass leaves the
  row `Recovering` so the next pass confirms;
- `ambiguous_dispatch_row_scans_before_redispatch_under_exact_tree` and
  `ambiguous_dispatch_row_confirms_from_tree_without_redispatch_under_exact_tree`
  prove that exact-tree advancement of a dispatch-ambiguity row scans before
  any POST and confirms a landed generation without redispatch;
- `post_timeout_before_the_dispatch_marker_is_definitely_unsent` and
  `post_timeout_after_the_dispatch_marker_is_ambiguous` prove that the SDK's
  POST deadline is classified against the dispatch marker: a transport that
  never released the request removes the fresh reservation and reports a
  stateless transport failure, while one that did produces durable ambiguity;
- a tracked generation can complete as `SubmittedWithoutHash` while retaining
  its immutable first-tracking timestamp;
- local committed failure clears the tracked candidate into `Recovering`;
- definite pre-dispatch failure does not create ambiguity;
- every possibly-dispatched class is durably recorded before retry, and final
  ambiguous-attempt exhaustion leaves hashless `Recovering` for a later
  invocation
  (`single_ambiguous_attempt_stays_recovering_and_retries_on_the_next_advance`,
  `attempts_cycle_endpoints_and_exhaust_to_hashless_recovery`,
  `fresh_ambiguous_post_exhaustion_stays_recovering_without_scanning`,
  `ambiguous_exhaustion_stays_recovering_and_a_later_exact_tree_pass_confirms`,
  `cancellation_during_final_dispatch_persists_hashless_recovery`,
  `operation_epoch_change_during_final_dispatch_persists_hashless_recovery`);
- `ambiguous_retry_clock_failure_reports_the_durable_recovery_state` proves
  that a clock failure between a durable ambiguity and its retry reservation
  reports durable `Recovering` rather than a stateless failure;
- `nonfinal_candidate_hash_collision_uses_the_remaining_bounded_retry` proves
  that a collision before the final attempt continues through the same
  invocation's remaining retry budget;
- `final_candidate_hash_collision_stays_recovering_without_lookup` proves that
  a final accepted hash owned by another generation is never polled, leaves
  hashless `Recovering`, and is retried by a later invocation;
- `final_recovery_hash_collision_stays_recovering_until_the_next_scan` proves
  the same for a final POST reserved by exact-tree recovery, whose next
  exact-tree pass scans before retrying;
- restart from `Submitting` produces `Recovering`;
- retry limits and endpoint failover are bounded per lifecycle invocation,
  attempts may exceed endpoint count, and endpoint selection cycles by ordinal;
  and
- retries cannot change semantic generation.

- `episode_escalates_to_exact_tree_once`: one `Recovering` pass escalates
  the episode to `ExactTree`; a second ends it as `StillPending`.
- `episode_never_retries_terminal_outcomes`: `SubmittedWithoutHash`,
  `Rejected`, `Confirmed`, and `Cancelled` end the episode on the pass that
  produced them.
- `episode_ends_after_its_pass_budget_and_paces_tracking_polls`: the pass
  budget bounds an episode, a zero budget runs until the row leaves
  `Tracking`, and every `Tracking` pass but the last waits `pending_repoll`.
- `an_epoch_change_or_cancellation_between_passes_ends_the_episode`: an
  epoch change during the repoll wait, a stale entry epoch, or cancellation
  ends the episode as `Cancelled` without another pass.
- `a_pass_refused_for_a_stale_epoch_ends_the_episode_as_cancelled`: an epoch
  change admitted past the boundary check and refused inside the pass ends
  the episode as `Cancelled`, not as a failed step; a pass that fails while
  the host is still on the episode's epoch is reported as its failure.
- `chain_failures_classify_by_dispatch_and_mark_the_dispatch_handle` and
  `helper_failures_classify_by_dispatch_and_phase`: a `RouteHttp` failure
  before the dispatch hook is `definitely_unsent`; after it is
  `possibly_dispatched`.

### Recovery

Tests cover:

- `Tracking` never invokes the tree client;
- `Recovering` polls its candidate before scanning;
- a canonical accepted hash from a hashless retry resumes `Tracking`;
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
- `nonfinal_ambiguous_recovery_retry_continues_bounded_retries` and
  `nonfinal_recovery_hash_collision_continues_bounded_retries` prove that a
  possibly-dispatched or colliding authorized retry uses the invocation's
  remaining budget through the ordinary backoff and ambiguous-retry
  reservation instead of returning;
  `ambiguous_recovery_retry_qualifies_for_a_direct_retry_on_the_next_advance`
  proves that the new dispatch ambiguity is durable, so a later status-only
  invocation reserves the next POST directly, and
  `interruption_inside_a_continued_retry_loop_keeps_the_reservation` proves
  that interrupting the continued loop removes nothing;
- `final_ambiguous_recovery_retry_stays_recovering` proves that exhausting the
  budget on an authorized retry leaves hashless `Recovering`;
- `imported_delegation_never_scans_under_exact_tree` proves that exact-tree
  advancement of an imported delegation polls its candidate and never scans;
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
- omitted zero-valued tree metadata and first-block indexes match the live REST
  encoding without weakening continuity validation;
- an indivisible block above the `5,000`-leaf target remains recoverable within
  the fixed response and snapshot bounds;
- a full `2^24`-leaf snapshot under deployed whole-block pagination fits the
  `6,709` leaf-request, `53,680 MiB`, `120`-hour, and streaming-memory
  ceilings without a smaller restart budget;
- interrupted scans restart without durable cursors or partial evidence; and
- tree confirmation never invents a hash.

### Locking, atomicity, and cancellation

Tests cover:

- singleton choice and complete batch membership lock after possible dispatch;
- active singleton and batch generations reject conflicting ballot-intent
  changes before recovery or helper-delivery material is touched;
- concurrent work cannot reserve two generations for one identity;
- concurrent producers generate at most one delegation proof for one
  wallet/round/bundle while distinct bundles remain parallel;
- `a_late_bundle_proof_preserves_vote_ready_round_phase` proves that a
  bundle-local proof can commit after another bundle advances the lossy
  round-wide marker;
- reused delegation proofs still reject mismatched notes or keys, and an
  account switch cannot retarget a waiting proof operation or its PIR cache;
- a same-round hotkey substitution cannot reuse a persisted proof, and
  same-thread same- or cross-bundle progress callback reentry fails without
  deadlocking;
- waiting-callback reentry fails without recursion, and a terminally rejected
  delegation continues to reuse its generation-bound proof;
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
- cancellation after dispatch preserves `Recovering`, including cancellation
  or an operation-epoch change during an invocation's final dispatch; and
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
- a hashless `Recovering` row becomes `SubmittedWithoutHash` only for code 2
  after unresolved dispatch, after a no-match tree pass under exact-tree
  advancement, while another deterministic rejection preserves the ambiguity
  and errors;
- version-18 rows migrate incrementally to version 19, fresh and migrated
  schemas share one chain-submission fingerprint, a migrated database passes
  the current-schema check on reopen, and current-schema fingerprint rejection
  covers missing columns, indexes, and triggers;
- a usable hash that follows durable dispatch ambiguity clears the stored
  ambiguity diagnostic on entry to `Tracking`
  (`usable_hash_after_dispatch_ambiguity_clears_the_stored_diagnostic`);
- a batch row whose generation digest does not re-derive, a vote claimed by
  two batch rows, and a vote with both a singleton and a batch row are
  projection invariant errors rather than silently different phases
  (`batch_row_with_mismatched_generation_digest_is_an_invariant_error`,
  `vote_claimed_by_two_batch_rows_is_an_invariant_error`,
  `overlapping_singleton_and_batch_rows_are_an_invariant_error`);
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

The compile-time surface check is the `compile_fail` doctest set on the
`chain_submission` module. It covers every removed confirmation entry point,
transaction-hash and position recorder, payload builder, the private
chain-event vocabulary, and the raw storage writers.

Delegation proof coordination is anchored by
`identical_proof_work_waits_and_reuses_durable_completion`,
`different_bundles_enter_proof_work_concurrently`, and
`failed_leader_releases_the_waiting_retry`. Durable reuse and wallet capture
are anchored by `reused_proof_rejects_mismatched_notes`,
`reused_proof_rejects_mismatched_keys`,
`reused_proof_rejects_receiver_sign_flip`,
`reentrant_progress_reporter_reuses_after_lock_release`,
`cross_thread_reentrant_progress_reporter_reuses_after_lock_release`,
`progress_reaches_the_host_while_the_operation_is_still_running`,
`blocked_host_callback_does_not_stall_the_producer`,
`hammered_identities_never_overlap_and_every_caller_returns`,
`panicking_producer_releases_waiters_and_its_own_thread`,
`wait_callback_reentry_returns_busy`,
`rejected_delegation_reuses_persisted_proof`,
`wallet_switch_does_not_retarget_waiting_proof`, and
`pir_fetch_persists_under_captured_wallet`.

These tests are the review contract for changes to chain submission behavior.

Phase 6 recovery coverage is anchored by
`exact_recovery_confirms_without_a_hash_and_clamps_timestamp`,
`preexisting_tree_recovery_reserves_without_invocation_backoff`,
`recovering_candidate_is_polled_before_no_match_retry_reservation`,
`definitely_unsent_recovery_retry_keeps_reservation_and_requires_a_new_scan`,
`malformed_tree_after_candidate_first_poll_retains_candidate_and_never_retries`,
`first_block_accepts_omitted_zero_start_index_and_recovers_layout`,
`complete_no_match_authorizes_only_the_captured_generation_and_candidate`,
`duplicate_complete_layout_is_ambiguous_not_confirmation`,
`empty_snapshot_accepts_omitted_zero_metadata_without_a_leaves_request`,
`empty_checkpoint_ignores_start_index_after_earlier_leaves`,
`nonempty_block_with_discontinuous_start_index_is_rejected`,
`empty_checkpoint_with_contradictory_root_is_rejected`,
`incomplete_pagination_produces_no_authorization`,
`oversized_atomic_block_is_accepted_above_the_page_target`,
`full_tree_capacity_fits_the_fixed_request_and_byte_ceilings`, and
`tree_confirmation_is_atomic_clamps_timestamp_and_survives_reopen_without_a_hash`.

Phase 7 batch activation coverage is anchored by
`constructs_exact_atomic_vote_batch_url_and_json`,
`atomic_vote_batch_uses_shared_lifecycle_and_confirms_ordered_positions`,
`atomic_vote_batches_from_one_through_protocol_maximum_confirm`,
`reordered_batch_confirmation_leaves_tracking_authoritative`,
`exact_recovery_confirms_complete_ordered_batch_layout`,
`partial_nonadjacent_batch_tree_members_authorize_retry_without_confirmation`,
`same_batch_concurrency_releases_only_one_atomic_post`, and
`atomic_batch_tracking_and_confirmation_survive_reopen`.

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

Public-lifecycle engine coverage is anchored by
`delegation_advancement_accepts_exact_spend_auth_signature_bytes`,
`delegation_advancement_rejects_malformed_spend_auth_signature_bytes`,
`persisted_delegation_derives_and_rejects_a_forged_signature`,
`persisted_delegation_rejects_malformed_authoritative_sighash`,
`persisted_delegation_rejects_signature_for_another_sighash`,
`reservation_commits_before_post_and_accepted_hash_is_tracking`,
`delegation_uses_the_same_lifecycle_and_atomic_confirmation_path`,
`reservation_failure_dispatches_nothing_and_writes_nothing`,
`attempts_cycle_endpoints_and_exhaust_to_hashless_recovery`,
`ambiguous_retry_waits_for_configured_backoff`,
`malformed_accepted_response_reserves_the_next_bounded_retry`,
`invalid_protocol_ambiguity_is_retryable_on_a_later_advance`,
`definite_rejection_recovery_never_reserves_an_ambiguous_retry`,
`accepted_retry_short_circuits_to_normal_hash_tracking`,
`nullifier_spent_after_ambiguity_is_terminal_and_idempotent`,
`other_rejection_after_ambiguity_surfaces_error_and_preserves_ambiguity`,
`single_ambiguous_attempt_stays_recovering_and_retries_on_the_next_advance`,
`failed_post_classification_reports_known_possible_dispatch`,
`failed_tracking_reconciliation_reports_the_durable_state`,
`tracking_deadline_survives_polling_and_coordinator_restart`,
`chain_rejection_preserves_bound_recovery_and_redacts_diagnostics`,
`recovery_retry_rejection_hash_is_not_candidate_evidence`,
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
`fresh_current_schema_requires_a_bound_generation`,
`v18_submission_rows_migrate_incrementally_to_v19`,
`noncanonical_v17_round_ids_create_no_submission`,
`current_fingerprint_rejects_missing_columns_indexes_and_triggers`,
`tracked_recovery_completes_without_hash_and_retains_tracking_start`,
`submitted_without_hash_survives_reopen_without_domain_confirmation`,
`invalid_protocol_response_ambiguity_reserves_the_next_retry`,
`definite_rejection_recovery_refuses_an_ambiguous_retry`,
`hashless_submission_blocks_same_bundle_but_not_unrelated_bundles`,
`submitted_without_hash_schedules_no_chain_recovery_step`,
`submitted_without_hash_delegation_blocks_without_pending_recovery`,
`reset_voting_session_state_scopes_submission_protection_to_its_bundle`,
`reset_voting_session_state_preserves_proved_bundle_setup_fields`,
`noncanonical_legacy_round_ids_remain_deletable`,
`lifecycle_owned_delegation_and_vote_yield_typed_advance_steps`,
`bound_hashless_recovery_yields_a_typed_advance_step`,
`rejected_singleton_vote_never_yields_submit_or_poll_work`,
`rejected_vote_batch_never_reschedules_its_members`,
`rejected_delegation_never_yields_delegate_work`,
`round_snapshot_reports_lifecycle_owned_recovery`,
`lifecycle_owned_vote_locks_conflicting_intent`,
`public_vote_writers_reserve_before_validation_and_wait_on_contention`,
`cancellation_during_final_dispatch_persists_hashless_recovery`, and
`operation_epoch_change_during_final_dispatch_persists_hashless_recovery`.


## Combined delegation and cast batches

Fresh round execution uses `DelegateAndVoteBatch`, a distinct submission
identity and generation. Its endpoint is
`/shielded-vote/v1/delegate-and-cast-vote-batch`; the body contains `delegation`
and `batch`. There is no endpoint fallback. An existing standalone or imported
delegation continues through its original lifecycle.

The initial VAN is the locally authenticated delegation output. Every nested
cast uses anchor height zero and the synthetic predecessor witness. The cast
authorization binds the round, initial VAN, action count, ordered effects and
proposal IDs with `SVOTE_DELEGATE_AND_CAST_VOTE_BATCH_SIGHASH_V1`. The canonical
encoding is defined only in `delegate_and_vote_batch/authorization.rs`; its frozen SDK
vector is tested in `delegate_and_vote_batch/tests/authorization.rs`.

The signed delegation authorization and all vote recoveries are persisted in
one transaction. Preparation captures one wallet-scoped handle for both the
delegation and the votes. Persistence requires the authorization and every
vote to share the same wallet, round and bundle, even if their cryptographic
material is identical. `combined_authorization_requires_the_same_wallet_round_and_bundle`
and `combined_persistence_rejects_mixed_storage_scopes_before_writing` pin this
boundary. `combined_authorization_refuses_a_wallet_switch_with_duplicate_setup`
checks a wallet switch with byte-identical, validly signed delegation setups.
Schema version 23 stores the authorization and its delegation
generation digest. The version-22 upgrade preserves existing PCZTs, proofs,
signatures and chain submission rows, and reopening is idempotent
(`v22_upgrades_combined_storage_to_the_fresh_schema`). Recovery requires that
generation to remain unchanged and never needs the delegation signing key. Ordinary batch identities refuse these
recoveries. The persisted combined unit excludes standalone delegation
admission, and fresh combined admission excludes any delegation submission
evidence. Retiring an undispatched batch clears its authorization after its
last member is cleared.

A combined transaction has one reservation, candidate hash, retry history and
terminal result. Confirmation must match the combined event, digest, complete
ordered proposal/nullifier roster, final VAN and contiguous vote leaves. The
initial delegation VAN is not a chain leaf. In one storage transaction,
confirmation advances the bundle to the final successor VAN and records every
vote position and the shared transaction hash. Tree recovery follows the same
final-VAN-plus-vote-leaves layout and does not invent an initial VAN position.

Conformance coverage: `combined_lifecycle_confirms_delegation_and_every_vote_together`
checks 1, 2 and 50 members and replay; `ordinary_batch_identity_cannot_dispatch_combined_signatures`
checks endpoint separation; `changed_delegation_generation_refuses_combined_recovery`
checks delegation generation binding.

`combined_confirmation_rejects_wrong_kind_partial_roster_and_wrong_layout`
checks confirmation binding, and
`a_combined_confirmation_storage_failure_rolls_back_every_projection` checks
transactional rollback. `combined_authorization_and_envelope_survive_database_reopen`
checks recovery without a live signer. The release-only
`combined_batch_builds_real_proofs_and_binds_the_initial_delegation_van` builds
two real ZKP2 proofs and checks their composite signatures and VAN binding.

The crash harness follows the fresh combined order: delegation proof, terminal
ballot, delegation signing, cast proofs, combined persistence, helper planning,
and one combined POST. `AfterTreeSync` is excluded from this fresh matrix.
`make recovery-conformance-unit` runs the harness's hermetic tests;
`make recovery-conformance` remains the separate live staging matrix.
