# Chain submission invariants

## Status and scope

This document records the vote-chain submission invariants implemented by
`zcash_voting`. It is the audit map for wallet integrators and reviewers of
delegation, singleton vote, atomic vote-batch, transaction lookup, durable
submission attempts, confirmation, and recovery.

The implementation is authoritative. A change to an invariant below SHOULD
update this document and the named regression tests in the same pull request.

The main implementation surfaces are:

- `chain`, which owns endpoint validation, mutation and transaction-status
  protocol mapping, bounded retry, response classification, and cancellation;
- `chain_submission`, which binds network work to durable voting identities,
  journals dispatch before POST, reconciles known transaction hashes, and
  applies confirmation;
- `confirmation`, which parses chain events and atomically records delegation,
  singleton-vote, and atomic-batch positions; and
- `storage`, which preserves submission-attempt evidence and delegation/vote
  recovery state across interruption and restart.

This SDK does **not** reproduce the vote chain's internal protobuf transaction
codec and does not predict chain transaction hashes. A chain hash becomes
known only through a validated server response.

## Terminology and state model

A **submission identity** is the wallet, round, transaction kind, bundle, and
proposal or batch digest to which one mutation belongs.

A **canonical submission** is a closed SDK wire type serialized once to the
canonical JSON accepted by the vote API. Arbitrary caller JSON is not a
supported lifecycle boundary.

A **payload digest** is SHA-256 of those canonical JSON bytes. It identifies
which locally constructed payload an attempt used. It is not the chain
transaction hash and MUST NOT be passed to the transaction-status endpoint.

An **attempting** submission has been durably reserved before its POST but does
not yet have a classified response. A process interruption leaves it
outcome-unknown.

An **outcome-unknown** submission may have reached the server but did not
produce a usable response.

An **accepted** submission returned CheckTx success and a validated chain
transaction hash. Acceptance is not commitment. The hash remains in the
attempt journal; it is not copied into the delegation or vote domain row until
the committed-success confirmation transaction also records the event-derived
position.

A **rejected** submission has definite failure evidence for that attempt. A
rejection does not erase stronger evidence or another attempt.

A **confirmed** submission has a committed successful transaction whose chain
events have been parsed and atomically applied to the voting database.

The durable transition is:

```text
validated durable generation
        |
        v
canonical JSON + local payload digest
        |
        v
Attempting persisted before POST
        |
        +-- definitely not dispatched --> reservation removed
        +-- response unavailable ------> OutcomeUnknown
        +-- CheckTx accepted ----------> Accepted(chain_tx_hash)
        \-- definite rejection --------> Rejected

Accepted or OutcomeUnknown with a known hash
        |
        +-- transaction absent --------> Pending
        +-- committed failure ---------> Rejected
        \-- committed success ---------> Confirmed + atomic domain update
```

Evidence precedence is **Confirmed > Accepted > OutcomeUnknown > Attempting**.
Weaker evidence cannot replace stronger evidence. Rejected is terminal only
for its exact attempt: a later attempt's definite rejection or failure cannot
disprove an earlier attempt in the same call whose outcome is unknown, so the
call reports `OutcomeUnknown` rather than a terminal result.

A durable confirmation belongs to the submission it was recorded for. A
confirmed atomic-batch member carries a transaction hash and a tree position
like any confirmed singleton, so the singleton shortcut checks the row's
persisted batch markers before treating those fields as its own confirmation;
otherwise a `cast-vote-batch` transaction would be reported as confirming a
singleton submission the dispatch path refuses to create.

A confirmation this lifecycle has already applied is durable, and it outranks
any later network answer. Reconciliation reports it as `AlreadyConfirmed`
instead of re-deriving it from a lookup, so a lagging or pruned endpoint cannot
downgrade a completed submission to "not yet committed" after a restart. That
outcome carries only the transaction hash: `bundles.van_leaf_position` is a
single pointer that a later vote or batch on the same bundle advances, so an
earlier transaction's own VAN position is not recoverable from storage and is
never synthesized from it. `Confirmed` remains the outcome that carries the
event data this call actually parsed.

Ambiguity is durable too. An `attempting` or `outcome_unknown` attempt means a
request may have reached the chain, and that holds whether or not a hash was
learned: a timeout, an unusable accepted response, or a process interruption all
leave one behind with no hash. Such an attempt keeps the submission
`OutcomeUnknown` across calls, so neither a later call's rejection nor a known
candidate's committed failure can be reported as terminal while it remains. A definitely pre-dispatch failure creates no such evidence, because
its reservation is removed, and neither does a rejected attempt.

Reporting that ambiguity is separate from **covering** the durable material an
attempt would be confirmed against. Coverage exists to protect confirmation, and
confirmation needs a chain transaction hash: the candidate set is the only route
to the transaction-status endpoint, and this SDK deliberately cannot predict a
hash or locate a transaction from its commitment. An attempt that has no hash and
can no longer be given one therefore protects nothing, while its row is one
nothing ever retires — no lookup can reach it and no rejection names it — so
treating it as coverage would freeze that proposal's recovery generation, ballot
intent, and bundle pruning for the life of the round.

`attempting` is the one hashless state that may still learn a hash, because a
POST that has not yet been classified can still return one, and the covered rows
are exactly what that response would be applied to. Stripping a live POST's
coverage is the failure to avoid here: its response could otherwise attach a hash
and event positions to a generation replaced in the meantime, which is exactly
the mismatch the cleanup guards exist to stop.

"A POST is in flight" is a claim with an expiry rather than a durable fact, so it
is decided every time a guard runs, by two tests in order of strength.

A reservation this process is waiting on is named exactly, by an in-memory
registry the submission loop holds from the moment it journals a reservation
until it classifies the response — and no longer. It is released before any
retry backoff: the answer is in by then, and holding it through a delay the host
chooses would keep cleanup, ballot-intent changes, and bundle pruning deferring
to a POST that is already over. That needs no clock, so no adjustment to the
system clock can make a live POST look abandoned and let cleanup erase the
material its response is about to be confirmed against.

The registry is keyed by the submission identity and wallet, not by the journal
row's id. Row ids restart per database file, so two handles on different files
mint the same id: an id-keyed registry would report one database's expired
reservation as live because another currently owns that number, and releasing
either guard would uncover the other. Keying by identity can instead carry a
registration into another database holding the same wallet and round, such as a
copy opened alongside, which over-protects a row rather than under-protecting
one.

A reservation this registry cannot know about — another process's, or one no
registry will ever hold because its process is gone — falls back to age. For that
to mean anything, `updated_at` has to record that the *owner was alive*, not
merely when the reservation was made: the two diverge exactly when the wall clock
steps forward. So an outstanding reservation refreshes it on a heartbeat far
shorter than the grace period, and only while it is still `attempting` and only
under the wallet that reserved it. That refresh never waits for the database: it
runs in the same task as the POST, so blocking would stop that task being polled,
and with it the very deadline the refresh exists to keep the reservation inside.
Neither wait counts — the handle's own mutex, and SQLite's write lock, which
another connection or process can hold and which the configured busy timeout
would otherwise sit on for seconds. A contended refresh is skipped instead.

The refresh stops when the POST is answered, and the classification that follows
needs the same connection. If a host holds `VotingDb::conn()` for longer than the
grace, that wait cannot be refreshed around — the refresh needs the very lock
being waited on — so a reservation can age out before its response is journaled.
No amount of heartbeating closes that; what closes it is not holding the shared
connection for minutes. This process is unaffected either way, because its
in-memory registry covers the whole call.

A reservation's timestamp is taken immediately before its insert, after the
connection has been acquired and the payload rebuilt. Both of those block, and a
reservation stamped on entry would arrive already part-spent against the grace
another process reads it by. A reservation is `attempting` only between its
journaling and the classification that follows its POST, so it cannot outlive
that call's request deadline by more than scheduling slop; one untouched for
longer than any configurable deadline, by an owner that would have refreshed it,
is not in flight. The grace period is derived from the largest deadline a host
may configure rather than chosen independently, and the client refuses a longer
one: the coverage query cannot see a per-call client configuration, so without
that cap a host could configure the distinction away.

This test still reads the wall clock, and the heartbeat bounds rather than closes
the window a clock step opens for a reader in another process. It is the weaker
of the two, which is why it is never the only one. Note also what the SDK does
*not* claim here: the lock that serializes one submission identity is
process-wide, not machine-wide, so two processes running the lifecycle for the
same identity are not serialized against each other at all, and a host that does
that owns the mutual exclusion.

Checking at query time rather than rewriting the row once is what makes the bound
hold. A downgrade pass performed when the database is opened would leave a
reservation abandoned by a crash *inside* the grace period untouched, and if the
restarted process keeps its handle open nothing would revisit the row once the
grace elapsed — restoring the permanent freeze for exactly the case the state is
meant to describe. It also means opening a database mutates no attempt state, so
a second handle or a second process cannot disturb a reservation the other is
waiting on.

Evidence is unaffected by any of this: `attempting` and `outcome_unknown` still
mean the same thing to every candidate and live-attempt query, so an expired
reservation keeps reporting its ambiguity as `OutcomeUnknown`. Only coverage
expires.

Dropping coverage cannot produce the mismatch the cleanup guards exist to
prevent. Attaching a transaction's hash and event-derived positions to a
generation it did not witness requires that hash, and a hashless attempt has
none. The transaction may still have committed; that stays visible as
`OutcomeUnknown`, and a replacement the chain later refuses surfaces as
`AlreadySpentUnresolved` rather than as success.

## Typed identity and payload invariants

1. The supported mutations are delegation, singleton cast-vote, and atomic
   cast-vote batch. A submission identity is constructible only through its
   kind-specific constructor, so its kind, proposal, and batch digest cannot
   disagree; the storage `CHECK` enforces the same pairing at rest.
2. A lifecycle call validates its exact wallet, round, bundle, proposal or
   batch digest, and durable recovery generation before any storage or network
   effect. The generation check is not a pre-flight read: the reservation
   transaction re-derives the canonical payload from durable state and requires
   it to hash to the same payload digest as the bytes about to be dispatched.
   The rebuild is given the reservation's own connection, so it cannot re-enter
   the shared database handle.
3. A recovered payload is bound to the storage row it was loaded from. Its
   embedded round, bundle, proposal, choice, and commitment must match the row
   the caller named, because the payload is serialized from the embedded
   identity while the attempt is journaled under the requested one. The
   reservation rebuild reproduces the same recovery, so it cannot catch this by
   itself. Every atomic-batch member is bound the same way: the batch digest,
   size, and ordering are all embedded in the same JSON, so they cannot witness
   a row that disagrees with the recovery stored on it.
4. Delegation accepts `SignedDelegationBundle`; singleton vote accepts a
   recovered `CommittedVote`; atomic vote accepts a recovered
   `SignedVoteBatch`. The SDK derives network fields from these types. A member
   of a persisted atomic batch is refused by the singleton path: dispatching one
   member to `cast-vote` could spend part of the batch independently, and its
   committed response could not be applied because confirmation rejects batch
   members.
5. The host cannot supply an arbitrary JSON map, a transaction hash to mark as
   accepted, or chain events to record outside the confirmation lifecycle.
6. Canonical JSON is serialized once for one call. Every retry in that call
   sends byte-identical content.
7. One chain transaction confirms one bundle. Recording a transaction hash
   against a bundle is refused when another bundle in the round already carries
   it: the VAN position it commits belongs to that bundle alone, and a stale or
   misbehaving endpoint returning one valid hash for two submissions would
   otherwise write a position onto a bundle the transaction never touched. An
   ownership is judged against the attempt journal as well as the domain columns,
   because CheckTx acceptance deliberately leaves those null and would otherwise
   let reconciliation order decide which identity receives the confirmation. An
   atomic batch legitimately records one transaction on every member of its own
   bundle, and only such a batch does: within a bundle the hash may be shared
   only by rows whose recovery carries the same batch digest, because a
   singleton `cast_vote` event has no proposal binding and accepting one
   proposal's transaction for another would mark it confirmed on evidence that
   never mentioned it. A row with no readable recovery belongs to no batch. An
   opaque identifier names no transaction, so it may repeat freely.
8. Server-returned transaction hashes must be exactly 32 bytes encoded as 64
   hexadecimal characters and are normalized to lowercase before persistence
   or lookup. Canonicalization is enforced at the storage boundary, not only in
   the client, so the older recording APIs cannot store one transaction under a
   casing that later reconciles as a second candidate. An accepted result whose
   hash is unusable is an unusable response, not a rejection; a rejected
   result's unusable hash is discarded while its code and log stay definite,
   because a rejected duplicate's hash never identified the earlier
   transaction. What counts as a transaction hash is one exact rule shared by
   the client and the storage boundary: exactly 64 hexadecimal characters, with
   no surrounding whitespace accepted on either side. A padded or otherwise
   non-conforming stored value stays opaque at rest and is skipped as a
   candidate, so it can never be confirmed into a conflict with itself.
9. The local payload digest is never accepted where a chain hash is required.

## Durable dispatch invariants

1. Every POST attempt is reserved in an immediate SQLite transaction before
   transport dispatch.
2. If reservation persistence fails, no request is dispatched.
3. Cancellation or a definitely pre-dispatch transport failure removes only
   the fresh, definitely-unsent reservation.
4. Timeout, failure after dispatch, response-body failure, unusable success,
   or process interruption preserves the attempt as outcome-unknown.
5. The response classification is persisted before another attempt begins, and
   the reconciliation between attempts applies the same dispatch gate as the
   preflight. A candidate that another writer records mid-call therefore stops
   further broadcasts, not only a confirmation. The reservation applies the same
   gate inside its own immediate transaction, which is what covers the window
   between the preflight and the first dispatch: the legacy recording APIs take
   no lifecycle lock, so only SQLite's own serialization orders one of their
   writes against this one. A candidate found there means the call reports what
   reconciliation settles rather than adding a duplicate broadcast, and a
   cancelled reconciliation at that gate reports an earlier broadcast's ambiguity
   rather than `Cancelled`, as at every other gate.

   This narrows the window; it does not close it, and is not meant to. A legacy
   write blocked behind the reservation can commit after it and before the POST
   starts, and holding SQLite's write lock across a network request to prevent
   that would stall every other database user for the request deadline. The
   bounded duplicate it leaves is the case this design already accepts: consensus
   nullifiers make at most one semantic action succeed, reconciliation retains
   every candidate, and the loser is retired.
6. Repeated byte-identical attempts share a payload digest but remain distinct
   ordered attempts. A re-signed payload has a different digest.
7. Accepted chain hashes and all earlier unknown attempts remain available for
   reconciliation until explicit round or account deletion. A definitively
   rejected attempt is not a reconciliation candidate: its transaction never
   entered the mempool, so leaving its hash in the candidate set would have
   lookup report it as pending and block the replacement payload forever. An
   attempt whose transaction is later found to have committed with a nonzero
   code is transitioned to rejected for the same reason, and so is the legacy
   domain hash it came from, which is also a reconciliation source. Every failed
   candidate a lookup discovers is retired, not only the one whose rejection is
   reported, or the remainder would keep blocking a replacement. That holds when
   the lookup also finds a success: adopting it must not leave a duplicate's
   proven failure live. Clearing a
   domain hash is scoped to an exact match on a row with no recorded
   confirmation position, so retirement can only remove a hash this
   reconciliation just proved failed.
8. Routine session reset and recovery cleanup do not delete chain submission
   attempts, and they do not erase material a **covering** attempt needs, nor an
   unconfirmed domain hash that is itself a reconciliation candidate. A hash a
   pre-lifecycle host recorded has no journal row behind it, so clearing it would
   take the only handle on a transaction that may still commit together with the
   recovery a committed response needs. Only a real chain hash counts: an opaque
   legacy identifier is never a candidate, so treating one as coverage would
   freeze that row with nothing able to release it. On the delegation side the
   same rule also keeps `clear_unsigned_delegation_setup_fields` blocked, which
   would otherwise erase `van_comm_rand`. A candidate a lookup proves failed is
   retired, which clears the column and lets a later cleanup proceed.
   Coverage is scoped to the exact attempted submission: a singleton attempt
   covers its own proposal row, and a batch attempt covers exactly the rows
   whose recovery carries that batch digest. A row whose recovery cannot be read
   is covered conservatively while its bundle has any covering batch attempt.
   Two kinds of attempt confer no coverage, because neither can ever be
   confirmed and nothing ever deletes either row, so treating them as coverage
   would freeze a proposal's recovery state permanently: `rejected` attempts,
   whose rejection is definite for themselves, and attempts with no chain
   transaction hash that can no longer learn one — every state but `attempting`.
9. Replacing a vote's recovery generation while a covering attempt names its row
    is refused inside the vote persistence transaction. Re-preparing the same
    choice with different parameters does not go through ballot intent, so
    that guard alone cannot see it; without this the lifecycle could dispatch
    stale bytes and a later confirmation could attach the stale transaction's
    hash and positions to the replacement. Rewriting the identical generation
    stays idempotent.
10. A ballot-intent change that would erase material a covering attempt needs is
    refused. CheckTx acceptance deliberately leaves the legacy vote
   column null, so the attempt journal is the only evidence that a dispatched
   vote may still commit. Re-selecting the same choice is never refused.
11. Partial bundle deletion is refused while any bundle in the pruned range has a
    delegation attempt of any kind, a vote or batch attempt that can still be
    confirmed, or an unconfirmed domain hash that is itself a reconciliation
    candidate. A delegation attempt bars pruning even when it is rejected: that
    rejection is evidence about the one POST that received it, not proof that
    nothing this bundle dispatched ever reached the chain, and pruning cascades
    away a `van_comm_rand` no retry can resample. Round or account deletion
    stays the one supported way to remove attempted delegation setup, as the
    delegation invariants require. That last has no journal row behind it, so the attempt checks
    cannot see it, and deleting the bundle would take both the candidate and the
    recovery a committed response needs; as in cleanup, only a real chain hash
    counts and a candidate proved failed is retired, which reopens pruning.
    An attempt references its round, not its bundle, so pruning would
    cascade away the bundle, vote, proof, and recovery rows a transaction that
    later commits needs, while leaving its journal evidence behind. Delegation is
    the one kind barred whatever its evidence: pruning cascades away
    `van_comm_rand`, which no retry can resample, and the attempt may already
    have spent the bundle's governance nullifiers. Attempts for pruned bundles
    are removed with them.
12. CheckTx acceptance alone never updates the legacy delegation or vote
    submission columns. This prevents a later DeliverTx failure from pinning a
    domain row to a transaction that did not commit successfully.
13. A classification that cannot be persisted never turns a possibly committed
    request into a definite result. Its reservation is still durably
    `attempting`, so an ambiguous outcome stays `OutcomeUnknown`, and a definite
    rejection that could not be journaled is reported the same way rather than
    as terminal — the live reservation is what the evidence rules then see, and
    a storage failure is no evidence about the submission either way.
14. An accepted transaction hash is never discarded by a failure to journal it.
    The transaction is already in the mempool and may commit, and the hash is the
    only handle anything will ever have on it, so a storage failure after a usable
    accepted response returns `AcceptedButUnjournaled` carrying both the hash and
    the persistence error rather than the error alone. The host SHOULD retain the
    hash and record it once storage recovers, so a later reconciliation can
    confirm it.

These rules mirror helper submission's reservation-before-POST and
strongest-evidence behavior. Chain submissions deliberately differ by allowing
bounded duplicate POSTs: consensus nullifiers make at most one semantic action
successful, while reconciliation retains every known candidate hash.

## Delegation randomizer and recovery invariants

`van_comm_rand` is the randomly sampled blinding factor for the delegation VAN
commitment. The later vote proof needs the same value to open and spend the VAN
created by the confirmed delegation.

1. Once any delegation attempt is journaled, ordinary session reset, retry
   cleanup, and recovery cleanup MUST preserve `van_comm_rand`, `gov_comm`, the
   proof, PCZT sighash, nullifiers, signature inputs, and related setup fields.
   Delegation coverage is per bundle, matching the delegation attempt's own
   bundle index, and unlike vote coverage it does not depend on the attempt's
   evidence: `van_comm_rand` cannot be resampled, and a delegation attempt whose
   transaction hash was never learned may still have spent the bundle's
   governance nullifiers.
2. Delegation confirmation does not clear those fields.
3. Resubmission MUST reuse the persisted delegation setup and
   `van_comm_rand`; it MUST NOT rebuild the bundle by sampling another
   randomizer.
4. Explicit round or account deletion is the only supported operation that
   removes attempted delegation setup.
5. The lifecycle reconciles every known chain hash before another dispatch.
   The host SHOULD invoke reconciliation before requesting fresh software
   signing work after restart, and restart planning points it there: a bundle
   with a non-rejected journaled attempt carrying a chain hash reports the
   submitted phase even though CheckTx acceptance deliberately left the domain
   column null. Reading that column alone planned the delegation as still to be
   made, and a software signer asked to sign again produces a different
   transaction whose predecessor's hash may then be lost. Votes are planned the
   same way, and the singular phase getters agree with the plural ones, so a
   host using either is told the same thing. A rejected attempt names a
   transaction that never entered the mempool, so it does not report submitted.
   Every plan surface that names a transaction carries the whole candidate set,
   not one of them — the delegation status, and the delegation and vote polling
   work: the domain
   column and the journal can hold different transactions — a legacy writer
   recording one while a lifecycle POST journals another — and a host shown a
   single hash can wait on a transaction that never commits while the other
   does. `tx_hash` keeps its own meaning as what is recorded, including an
   opaque identifier nothing can look up, which the candidate set excludes.
   The set is every non-rejected attempt that carries a hash, not the newest one
   per row: concurrent processes can each be accepted with a different hash for
   one submission. Reconciliation remains the supported way to resolve them,
   because it checks all of them, adopts the winner, and retires the rest. For a vote, polling the
   wrong one of two candidates also stalls the helper-share delivery that follows
   confirmation.
   The phase and the hash it implies come from one lookup, everywhere a plan
   reports either: the recovery work, the canonical delegation statuses, and the
   submitted-batch validation. Deriving the phase from the journal while reading
   the hash from the domain column turns the very state this recovers into a
   planning failure, or into one plan describing a bundle two ways — submitted,
   with no transaction to point at.

The crash guarantee differs by signer and transaction type:

- singleton and atomic votes are exactly reconstructable from persisted vote
  recovery material;
- Keystone delegation is exactly reconstructable while its persisted
  signature remains available;
- software delegation can replay exact bytes during the live call, but its
  final signature is not durably retained by this lifecycle;
- after a crash, a software signer may create another valid signature over the
  same persisted setup. That payload may have a different transaction hash.

Consequently, a software-delegation POST can succeed while its response and
hash are lost. If a later semantic duplicate is rejected because its
nullifier is spent and no previously known hash confirms, the SDK returns
`AlreadySpentUnresolved`. It does not invent a transaction hash or VAN leaf
position, clear the randomizer, or claim that another wallet's transaction was
ours.

The current sparse vote-tree client cannot locate an unknown VAN position from
its commitment: witness retention requires the position before sync. A future
chain index or commitment-scanning recovery API may close this explicit gap.

## Transport and timeout invariants

### Default limits

| Limit | Default |
| --- | ---: |
| Complete request attempt | 10 seconds |
| POST attempts per lifecycle call | 3 |
| Retry backoffs | 2 seconds, then 4 seconds |
| Accepted response body | 256 KiB |

The timeout covers connection setup, response headers, and the complete body.
The client wraps custom transport futures in the deadline and validates the
body limit and `application/json` content type even when a custom transport
does not.

Caller-configurable durations must be nonzero and representable by Tokio's
monotonic clock. The request deadline additionally has an upper bound, because it
is what limits how long an attempt reservation can stay `attempting`, and the
interrupted-reservation downgrade is derived from that limit. Endpoint lists must be nonempty, canonical HTTP or HTTPS base
URLs without credentials, query, or fragment, and distinct after
canonicalization. Invalid endpoint configuration fails before storage or
network effects.

### Retry and failover

The client may retry or fail over after:

- a definitely pre-dispatch transport failure;
- timeout or another ambiguous transport failure;
- response-body failure or an unusable successful response;
- HTTP 429; or
- HTTP 500, 502, 503, or 504.

An accepted broadcast whose transaction hash is unusable is an unusable
response and keeps retry and failover going. A definite rejection is classified
from its code and log alone, so the spent-nullifier compatibility path still
runs when the returned hash is unreadable.

Other 4xx responses are terminal except the spent-nullifier compatibility
case. Other 5xx responses are outcome-unknown but non-retryable in the current
call. Before a replay, the lifecycle queries every known chain hash. Exhausted
ambiguity returns `OutcomeUnknown`, never definite rejection. Once any attempt
in a call is outcome-unknown, a later attempt's rejection or definite failure
is reported as `OutcomeUnknown` too, because it is definite only for itself.

The host owns the network route. A Tor or proxy transport MUST fail closed and
MUST NOT fall back to a direct connection. Hyper connection pooling must not
allow an old route to survive a policy change that forbids it.

## Spent-nullifier reconciliation invariants

The vote API currently exposes spent-nullifier detail through a compatibility
log rather than a structured reason code. The SDK owns one narrow classifier;
hosts do not parse the log.

1. A spent-nullifier response stops new related broadcasts in the current
   operation.
2. The lifecycle queries every previously learned chain hash for the submission
   identity across the configured endpoint set.
3. Exactly one committed successful candidate is adopted and confirmed. A
   terminal lookup error on an unrelated candidate is retained rather than
   returned, so it cannot discard a candidate that already committed; it is
   reported only when no candidate succeeded and nothing else may still commit.
   A lookup that could not be completed is an absence of evidence, not evidence
   of absence, so it ranks below everything that is evidence: a candidate
   another endpoint reported pending, one whose own answer was unreadable, and a
   dispatched attempt still awaiting a response. The candidate set is re-read
   there rather than taken from the snapshot the lookups began with, because
   another writer can journal one while they run and it arrives `accepted`.
   `reconcile` reaches that exit directly, without the submission loop's
   ambiguity handling around it, so the rule is applied there. Either confirmed outcome —
   freshly parsed or durable — is proof of success on this path.
4. No successful known candidate returns `AlreadySpentUnresolved` and
   preserves all attempts and recovery material. That outcome asserts the known
   candidates were checked and none had succeeded, so a reconciliation that
   settled nothing — cancelled, or unable to read its answers — is reported as
   it stands instead. Claiming otherwise would state as fact something the call
   never established, and bury the ambiguity underneath it.
5. More than one committed successful candidate is an invariant violation and
   produces no partial domain mutation.
6. A rejected duplicate's hash is not evidence that it identifies the earlier
   successful transaction.

## Transaction lookup and confirmation invariants

1. Transaction lookup accepts only normalized 32-byte hexadecimal hashes. A
   stored candidate that is not one, such as an opaque identifier recorded by a
   pre-lifecycle host, is skipped rather than failing reconciliation for that
   identity. Candidates are canonicalized before deduplication, so one
   transaction recorded in two casings is queried once and cannot be counted as
   two committed candidates.
2. HTTP 200 is a committed transaction response and parses height, code, log,
   and events.
3. Cancellation is observed as soon as each endpoint request returns, before
   the response is classified. `ChainClient` is a supported API in its own
   right, so this holds for a caller using it directly and not only for the
   lifecycle wrapped around it.
4. HTTP 404 means not yet committed and may fail over to another endpoint. A
   404 is protocol evidence, so it must satisfy the same body-size and
   content-type rules as any other response; an unusable one is evidence of
   nothing. Lookup gives each configured endpoint one attempt; it does not
   retry the same endpoint.
5. A structured HTTP 422 transaction result is committed failure. A 422 body
   reporting a success code contradicts its own status and is unusable, so an
   error response can never mutate confirmed voting state. The same rule applies
   to broadcast responses: a 422 claiming success must not journal an accepted
   attempt or stop retries for a transaction that was not accepted.
6. Invalid content type, oversized body, malformed JSON, missing required
   fields, or a committed success whose events do not bind to the submission
   being reconciled is an unusable response, not confirmation, and failover
   continues to the remaining endpoints. Binding is checked inside the lookup so
   it participates in failover: judging it only afterwards would let the first
   endpoint to answer about the wrong transaction end the search, and stable
   endpoint ordering would repeat that on every later call while another
   configured endpoint could serve the real confirmation. Only successes are
   judged this way — a nonzero code is definite evidence whatever its events say.
   The checks that compare against persisted recovery stay in the confirmation
   transaction, where they roll back with it. One malformed endpoint therefore cannot hide a
   committed result another endpoint can still serve. If no endpoint returns a
   usable transaction response, the unusable-response error is reported rather
   than `Pending`: an endpoint that answered about the transaction is stronger
   evidence than another endpoint's 404.
7. Delegation confirmation uses `confirm_delegation_submission`; singleton vote
   uses `confirm_vote_submission`; atomic batch uses
   `confirm_vote_batch_submission`.
8. A candidate whose status could not be read is reported as
   `OutcomeUnknown`, never as `Pending`. A broken or incompatible endpoint must
   stay distinguishable from a genuine 404, and an unresolved candidate blocks
   rebroadcast exactly as a pending one does, because it may still commit. Nor is
   `Pending` reported while a hashless attempt is still live: that attempt may
   already have committed under a hash nothing can locate, so saying the known
   candidates merely have not committed yet would overstate what this call
   established.
9. The candidate set is rebuilt after the lookups and before any answer is
   returned. The set read beforehand is stale in both directions by then:
   retirement has marked proven failures `rejected`, and another writer can
   journal a candidate while the requests run. Reporting the older set would
   hand back a transaction this call disproved, or omit one it should be
   polling — and for a vote, an omitted candidate that commits also stalls the
   helper-share delivery that follows confirmation.
10. A confirmation another writer applies while these lookups are in flight
   outranks every weaker answer. The entry shortcut runs before them, so the
   durable state is read again before reporting a terminal error, ambiguity,
   pending, or rejection: a lagging 404 would otherwise report `Pending`, and a
   different candidate's committed failure would report `Rejected`, for a
   submission that is now durably confirmed. Candidates proved failed are still
   retired on that path. A submission call applies the same rule to its own
   "still waiting" answers and to `Accepted`, once where they leave the attempt
   loop, because a POST can be in flight while another writer confirms.
   Acceptance is not commitment, so it too is weaker than a confirmation already
   applied; the hash the call learned stays in the journal either way.
11. Adopting a candidate this lookup proved committed clears any *different*
   unconfirmed hash in the domain column for that submission, inside the
   confirmation transaction and after the event validation. The domain writers
   refuse to overwrite a stored hash with a different one, so an opaque
   identifier a pre-lifecycle host recorded — which the version-18 migration
   preserves and candidate selection skips — or a hash a concurrent legacy
   recording call wrote would otherwise make the confirmation transaction fail,
   and fail identically on every later reconciliation, leaving the position unset
   for good. Clearing is sound because consensus nullifiers let at most one
   semantic action for an identity succeed, so a stored value that differs from a
   proven success either failed, never landed, or is the same transaction under an
   unrecognized encoding. It is scoped to rows with no recorded confirmation
   position, and a batch clears exactly its own member rows. Ordering it after
   validation matters: a confirmation whose events do not bind to this submission
   rolls the clearing back with it, so a faulty endpoint cannot destroy the only
   record of a competing candidate. The standalone recording APIs keep refusing a
   contradicting stored hash, because a host passing one is reporting a
   contradiction it should see.
12. The winning hash and event-derived positions are committed together by the
   existing confirmation transaction. Attempt evidence is retained separately;
   a crash on either side is recoverable because the attempt hash and the
   domain record are both idempotent and neither overwrites conflicting state.
13. Atomic-batch members advance together or not at all.
14. Event round, bundle, proposal order, batch digest, and nullifier bindings
    are validated by the existing confirmation parser before writes.

## Concurrency and cancellation invariants

1. A process-wide asynchronous lock serializes operations for one submission
   identity. Unrelated submissions remain independent. The wallet identity is
   captured once, where that lock is keyed, and every durable read, journal
   write, and reservation in the operation uses that captured wallet. A host
   that switches accounts mid-flight therefore cannot lose an accepted hash to a
   zero-row update, nor leave a definitely-unsent reservation behind; the
   confirmation write is performed under the captured wallet as well: the
   `confirm_*_for_wallet` entry points take it as an argument rather than
   re-reading mutable state, so there is no window between checking the wallet
   and persisting under it.
2. The identity lock registry holds weak references. Concurrent operations on
   one identity still share a mutex, because a second acquisition upgrades the
   entry the first is holding, but an identity with no live operation becomes
   reclaimable. A long-lived wallet moves through many rounds and proposals, so
   the registry must stay bounded by active identities rather than by every
   identity the process has ever seen.
3. After acquiring the lock, the lifecycle re-reads the exact durable recovery
   generation. A stale handle fails before network dispatch.
4. The in-flight guard for an identity is taken before its reservation is
   journaled, not after that transaction commits. The row is durable the instant
   it commits and coverage decided by age can already read it as stale, so a
   guard taken afterwards leaves a window in which cleanup may erase the
   generation the dispatched bytes were built from while the call goes on to
   POST them. Registering first can only name an identity whose reservation then
   fails, which over-protects its rows until the guard drops.
5. A reservation that cannot be taken is a failure definite only for its own
   attempt. If an earlier attempt in the call is outcome-unknown, a journaled
   attempt may still commit, or a candidate hash is known, the call reports
   `OutcomeUnknown` rather than the persistence error: a concurrent change to the
   now-uncovered generation is exactly what makes a rebuild fail as stale, and
   reporting only the error would invite the host to treat the replacement as
   safe to submit.
6. Attempt insertion, its round and owner validation, and the payload rebuild
   that proves the generation is unchanged all share one immediate SQLite
   transaction. A generation replaced or cleared by another connection after
   the payload was serialized is therefore caught before dispatch, not merely
   before the lock was taken.
7. Cancellation is checked on entry to reconciliation and before reservation,
   dispatch, retry, failover, backoff, and confirmation application, and again
   as soon as each transaction-status request returns, after the state reads on
   the no-candidate path, which has no lookup loop behind it to check again, and
   both before and after failed candidates are retired, because that retirement is a durable mutation
   of its own and its wait on SQLite widens the gap before the submission is
   classified. The entry check covers
   the no-candidate fast path, so a cancelled operation is never reported to the
   host as actively pending; the post-request check covers every result variant,
   not only a committed success, because classifying a 404 or an error reports an
   outcome and classifying a committed failure retires evidence. A cancelled
   operation does neither: the candidate stays journaled, so the next
   reconciliation re-derives whatever this one was about to conclude.
8. No failure inside a submission call is reported as that call's result while
   the call already knows the transaction may commit. Every step there is
   fallible — a lookup, a journal write, a reservation, the cleanup of an unsent
   one — and each failure is definite only for the step that raised it, so the
   rule is applied once where those failures leave the attempt loop rather than
   at each of them. A completed ambiguous broadcast, a known candidate, or a
   live attempt all outrank it, and the call reports `OutcomeUnknown`. The
   durable-confirmation reread on the way out is supplementary in the same sense,
   so a failure there returns the outcome already established rather than
   discarding an accepted hash. A
   completed ambiguous broadcast is held in memory and needs no storage read to
   be true, so it is honoured even when the database has become unreadable —
   which is one of the things that gets us here; the candidate list is
   supplementary once that decision is made, and failing to read it does not
   undo it. The one exception is a CheckTx acceptance that could not be
   journaled: it already carries the transaction hash, which is strictly more
   than the ambiguity it implies.
9. A reconciliation that fails to settle does not overwrite what this call
   already knows. That covers both a `Cancelled` result, which concluded
   nothing, and a terminal lookup error, which is evidence about someone else's
   candidate rather than about this call's earlier attempt: with a completed
   ambiguous broadcast outstanding, either reports that broadcast's ambiguity
   instead. It applies wherever a reconciliation's result is adopted, including
   the between-retry gate and the rejection path.
10. Cancellation observed after a broadcast completes does not replace that
   broadcast's result. A call cancelled while a dispatched attempt may still
   commit reports `OutcomeUnknown`; `Cancelled` is reserved for calls with no
   completed ambiguous broadcast.
11. Cancellation before a fresh reservation dispatches removes the definitely
   unsent reservation. Cancellation after dispatch retains uncertainty.
12. A deleted or replaced generation cannot receive a delayed transport or
   confirmation result. Cancellation is re-checked immediately before the
   confirmation transaction, so a session invalidated while a status request
   was in flight does not have voting state mutated underneath it. This narrows
   the window rather than closing it, which is safe because the winning hash
   stays in the attempt journal and confirmation is idempotent: a suppressed
   confirmation is re-derived by the next reconciliation.

Vizor implements host cancellation with a monotonically increasing operation
epoch. Account/session invalidation advances the epoch synchronously; every
SDK callback compares the captured epoch before reservation, dispatch, retry,
and confirmation application.

## Persistence and compatibility invariants

Schema version 18 adds `chain_submission_attempts`. Launch-version databases
migrate in place and retain all existing round, delegation, vote, and helper
state. A newer unsupported schema remains rejected.

Opening a database mutates no attempt state. Whether an `attempting` reservation
is still in flight is decided by its age wherever a guard needs to know, so
neither a second handle on the same file nor a second process can disturb a
reservation the other is waiting on, and a reservation an interrupted process left
behind stops conferring coverage as soon as the grace period elapses rather than
at the next open.

Each attempt stores wallet, round, kind, bundle, proposal sentinel or batch
digest, ordered attempt number, local payload digest, optional server chain
hash, evidence state, and timestamps. It stores neither the canonical request
body nor a locally predicted chain hash. Deleting the owning round cascades to
its attempts.

Version 18 also canonicalizes transaction hashes already stored by earlier
recording APIs: a `bundles.delegation_tx_hash` or `votes.tx_hash` of exactly 64
hexadecimal characters is lowercased once, and anything else is left untouched.

Existing public submission-recording functions remain compatible. The new
high-level lifecycle is the supported boundary for SDK-owned network
submission and confirmation, and `prelude` is its supported import path.

## Host responsibilities and trust boundaries

The host wallet owns:

1. the direct, proxy, or Tor route and fail-closed route behavior;
2. authenticated environment configuration and endpoint mapping;
3. application timers, lock/account/round invalidation, and cancellation;
4. invoking restart reconciliation before requesting new signing work; and
5. presenting `AlreadySpentUnresolved` as preserved but unresolved state rather
   than success or a safe restart.

The SDK owns typed mutation serialization, durable attempt reservation,
bounded retry and failover, response classification, spent-nullifier
reconciliation, transaction lookup, chain-event parsing, and durable voting
state transitions.

## Reviewer checklist

- Does every POST have a committed attempt reservation first?
- Can failure to persist a reservation still dispatch a request?
- Can interruption or cancellation erase a possibly dispatched attempt?
- Can ordinary cleanup erase `van_comm_rand` or delegation setup after an
  attempt?
- Can any retry sample a new `van_comm_rand`?
- Is a local payload digest ever treated as a chain hash?
- Can arbitrary caller JSON or a caller-supplied accepted hash reach the
  lifecycle?
- Can a stale recovery generation reach storage or network work?
- Is the generation proven unchanged inside the reservation transaction, or
  only before it?
- Can a ballot-intent change erase material an accepted-but-uncommitted
  submission still needs?
- Can a rejected attempt permanently freeze recovery state or ballot intent?
- Can an attempt that can never be looked up or confirmed permanently freeze
  recovery state, ballot intent, or bundle pruning?
- Is a reservation left behind by an interrupted process still treated as a POST
  that may return a transaction hash?
- Can opening a second database handle, or a second process, strip an in-flight
  POST's coverage?
- Is a reservation abandoned inside the grace period ever revisited once it
  elapses, without waiting for the database to be reopened?
- Can a confirmation that fails validation still have destroyed a competing
  candidate's hash?
- Can a wall-clock adjustment expire a reservation whose POST is still in flight?
- Can two database handles collide in the in-flight registry, so one's expired
  reservation reads as live or one's release uncovers the other?
- Is the in-flight guard held before a reservation becomes durable, or only
  after its transaction commits?
- Can a reservation failure after an ambiguous dispatch be reported as a plain
  error, hiding an attempt that may still commit?
- Can recovery cleanup or bundle pruning erase an unconfirmed domain hash that is
  still the only reconciliation candidate for its row?
- Can a cancelled reconciliation let this attempt's rejection be reported as
  terminal while a known candidate may still commit, or replace an earlier
  ambiguous broadcast's result at the between-retry gate?
- Is a confirmation another writer applies during the candidate lookups allowed
  to be downgraded by a lagging 404 or another candidate's failure?
- Can a classification that fails to persist turn a possibly committed request
  into a definite error, or into a terminal rejection?
- Is every failed candidate retired on every way out of a lookup, including the
  mixed ones where another candidate returns first?
- Can a terminal lookup error for someone else's candidate replace this call's
  own ambiguity?
- Is that rule applied to every fallible step in a submission call, or only to
  the ones a reviewer happened to name?
- Does in-memory ambiguity survive a database that has become unreadable?
- Does restart planning see a journaled accepted transaction, or only the domain
  columns CheckTx acceptance leaves null?
- Do the singular and plural phase getters agree about it?
- Does a step that says "poll" always have a hash to poll, from the same lookup
  that decided the phase?
- Does every field of one `RoundPlan` describe a bundle the same way, or can the
  statuses and the recovery work disagree?
- Does every plan surface that names a transaction expose the whole candidate
  set, or select one of several?
- Is that set every non-rejected attempt carrying a hash, or one per row?
- Can the reservation heartbeat block the request deadline it protects, on
  either the handle's mutex or SQLite's own lock?
- Can one chain transaction confirm two bundles, or two singleton proposals in
  one bundle, including when its only record is the attempt journal?
- Is a reservation stamped before or after the blocking work that precedes it?
- Is the in-flight guard released once the response is classified, or held
  through the retry backoff?
- Can `Pending` be reported while a hashless attempt may already have committed?
- Can a supplementary durable read, failing, discard an accepted hash?
- Is the candidate set a reconciliation reports rebuilt after its lookups, or
  taken from the snapshot they began with?
- Can a committed failure be reported as terminal while a candidate journaled
  during the lookup may still commit?
- Is cancellation observed by the public lookup client, not just by the
  lifecycle?
- Can a confirmed atomic-batch member be reported as a singleton confirmation?
- Do an atomic batch's persisted proposal and nullifier bindings participate in
  endpoint failover?
- Does an outstanding reservation keep proving its owner is alive, rather than
  only recording when it was made?
- Can one endpoint's confirmation for the wrong submission end a lookup that
  another endpoint could still answer?
- Is cancellation observed after a lookup returns, for every result variant and
  not only a committed success?
- Can an accepted transaction hash be lost when journaling it fails?
- Can a configurable request deadline outlive the reservation grace period?
- Can an unconfirmed hash already in a domain column block a proven success from
  ever being applied?
- Can a candidate another writer recorded mid-call be missed by a terminal
  rejection or a terminal transport failure?
- Does dropping coverage for a hashless attempt still report its ambiguity?
- Is attempt-based cleanup protection scoped to the attempted proposal or batch
  digest, rather than the whole bundle?
- Can one malformed endpoint hide a committed transaction from lookup?
- Can an unusable transaction response be reported as "not yet committed"?
- Can an unreadable server hash turn a definite rejection into a replay, or
  bypass the spent-nullifier classifier?
- Can one transaction stored in two casings reconcile as two candidates?
- Do the client and the storage boundary agree exactly on what a transaction
  hash is?
- Can a member of an atomic batch be dispatched as a singleton vote?
- Can a rejected or committed-failure candidate block a replacement payload
  forever?
- Does a rejected delegation attempt still bar partial pruning, as the
  delegation invariants require?
- Are candidates retired during a lookup still reported back to the host as
  pending?
- Can bundle pruning delete the state a possibly-committed transaction needs?
- Can an account switch mid-flight lose an accepted hash or write to the wrong
  wallet, including between a wallet check and the write it guards?
- Can a failed candidate survive in a legacy domain column after retirement?
- Is cancellation observed even when there is nothing to look up?
- Is the identity lock registry bounded by active identities?
- Does `prelude` expose the lifecycle the crate documentation recommends?
- Are retries byte-identical within one live call?
- Is the software-delegation crash recovery gap still explicit?
- Can a spent-nullifier response discard known hashes or recovery material?
- Can `AlreadySpentUnresolved` be reported when the candidate lookups never
  settled?
- Can an unresolved nullifier be reported as success?
- Can weaker evidence overwrite accepted or confirmed evidence?
- Can a later attempt's rejection hide an earlier attempt that may still commit?
- Can a durable confirmation be downgraded by a later lookup, or missed by the
  spent-nullifier path?
- Can a terminal lookup error on one candidate discard another that committed,
  another that is merely pending or unreadable, or a dispatched attempt that may
  still commit?
- Does a submission call reread the durable state before reporting that it is
  still waiting, or that its POST was accepted?
- Is every failed candidate retired, including when a success is adopted in the
  same lookup?
- Can a vote's recovery generation be replaced while an attempt covers it?
- Can an HTTP 422 body claiming success be applied as confirmation, or journaled
  as an accepted broadcast?
- Can cancellation erase a completed broadcast whose outcome is unknown?
- Is any event-derived value synthesized from a shared mutable pointer?
- Does a hashless dispatched attempt survive as ambiguity across calls?
- Is a definitely pre-dispatch failure ever recorded as ambiguity?
- Does a candidate recorded mid-call stop the remaining retries, and one
  recorded between the preflight and the first dispatch stop that dispatch?
- Are candidates re-read before a terminal lookup error is propagated?
- Can an unusable 404 be accepted as protocol evidence?
- Is a recovered payload bound to the storage row it was journaled against?
- Can late cancellation replace a completed request result?
- Can confirmation partially update an atomic batch?
- Are endpoint identity, request deadlines, body limits, JSON content type,
  retry count, and failover bounded by the SDK?
- Does a custom or Tor transport retain the host's fail-closed route policy?
- Does each invariant name its enforcement surface and regression coverage?

## Regression map

- `chain::tests::retries_send_byte_identical_canonical_json` covers canonical
  byte reuse, bounded retry, and endpoint rotation.
- `chain::tests::endpoint_set_rejects_duplicate_canonical_identity` covers
  endpoint identity validation.
- `chain::tests::spent_nullifier_classifier_is_narrow_and_case_insensitive`
  covers the compatibility classifier boundary.
- `chain_submission::tests::an_unrelated_confirmation_fails_over_to_the_next_endpoint`
  covers submission-specific event binding participating in endpoint failover.
- `chain::tests::unusable_lookup_response_fails_over_to_next_endpoint` and
  `chain::tests::unusable_lookup_response_is_not_reported_as_pending` cover
  lookup failover past an unusable response and the refusal to downgrade one to
  "not yet committed".
- `chain::tests::the_hash_rule_is_exact_and_shared_with_storage` covers the one
  shared, whitespace-exact transaction-hash rule.
- `chain::tests::a_malformed_404_is_not_accepted_as_protocol_evidence` covers
  404 response validation.
- `chain::tests::accepted_result_with_unusable_hash_is_ambiguous` and
  `chain::tests::rejected_result_with_unusable_hash_stays_definite_and_keeps_its_log`
  cover the split classification of an unreadable server-returned hash.
- `chain_submission::tests::check_tx_acceptance_is_journaled_without_domain_mutation`
  covers the CheckTx/committed-domain separation.
- `chain_submission::tests::known_pending_hash_is_reconciled_without_another_post`
  covers reconcile-before-replay for accepted candidates.
- `chain_submission::tests::committed_failure_rejects_without_pinning_domain_hash`
  covers DeliverTx failure classification without a partial domain write.
- `chain_submission::tests::reservation_rejects_a_changed_durable_generation`
  and `chain_submission::tests::reservation_accepts_the_matching_durable_generation`
  cover the in-transaction payload rebuild, including that a mismatch dispatches
  nothing and journals nothing.
- `chain_submission::tests::cancellation_after_lookup_suppresses_the_confirmation_write`
  covers the cancellation checkpoint immediately before confirmation, and
  `cancellation_after_a_lookup_stops_before_retiring_evidence` covers the
  post-request checkpoint on a non-success result variant.
- `chain_submission::tests::an_accepted_hash_survives_a_failure_to_journal_it`
  covers `AcceptedButUnjournaled`, and
  `an_ambiguous_outcome_survives_a_failure_to_journal_it` covers the same rule
  for an ambiguous classification.
- `chain_submission::tests::a_cancelled_retry_reconciliation_preserves_earlier_ambiguity`
  covers the between-retry gate, and
  `a_confirmation_applied_during_lookup_outranks_a_lagging_404` covers the
  durable-confirmation re-read after the candidate lookups.
- `chain_submission::tests::a_batch_confirmation_with_wrong_members_fails_over`
  covers an atomic batch's recovery bindings participating in endpoint failover,
  and `a_confirmed_batch_member_is_not_a_singleton_confirmation` covers the
  singleton shortcut refusing a confirmed member.
- `chain_submission::tests::a_failed_candidate_is_retired_even_on_a_mixed_status_exit`
  covers retirement on a lookup that exits through another candidate's unusable
  response, and `cancellation_after_all_failure_retirement_is_observed` covers
  the cancellation check that follows it.
- `chain_submission::tests::in_memory_ambiguity_survives_an_unreadable_database`
  covers ambiguity held in memory outliving the storage reads that describe it.
- `phases::tests::an_accepted_journal_attempt_reports_a_submitted_phase`,
  `a_rejected_journal_attempt_does_not_report_submitted`, and
  `the_singular_getter_agrees_with_the_plural_one` cover restart planning
  reading the attempt journal, its rejected-attempt boundary, and the two phase
  APIs agreeing.
- `session::tests::a_journal_only_acceptance_plans_polling_with_its_hash` covers
  a polling step and the canonical delegation status both finding the hash the
  phase was derived from, `two_different_candidates_are_both_exposed` and
  `two_different_vote_candidates_are_both_exposed` cover the status and the vote
  polling work carrying both when the two sources disagree, and
  `chain_submission::tests::every_journaled_candidate_survives_for_one_submission`
  covers two accepted attempts for one submission both surviving.
- `chain_submission::tests::a_committed_failure_yields_to_a_candidate_journaled_during_the_lookup`
  covers a rejection deferring to a candidate that arrived mid-lookup, and
  `a_retired_candidate_is_not_reported_as_pending` covers the reported candidate
  list being rebuilt after retirement.
- `chain::tests::a_lookup_cancelled_in_flight_reports_cancelled` covers the
  public lookup client observing cancellation before it classifies a response.
- `chain_submission::tests::a_retry_reconciliation_error_preserves_earlier_ambiguity`,
  `a_rejection_that_cannot_be_journaled_stays_unknown`,
  `a_rejection_path_lookup_error_preserves_earlier_ambiguity`, and
  `a_failed_unsent_cleanup_preserves_earlier_ambiguity` cover failures at four
  different steps of a submission call, all reaching the one rule that keeps a
  completed ambiguous broadcast's result.
- `chain_submission::tests::one_transaction_recorded_in_two_casings_is_looked_up_once`
  and `chain_submission::tests::a_legacy_opaque_hash_does_not_break_reconciliation`
  cover candidate canonicalization and non-normalizable legacy hashes.
- `chain_submission::tests::identities_cannot_pair_a_kind_with_the_wrong_key`
  covers the constructor-only submission identity.
- `chain_submission::tests::a_batch_member_is_never_dispatched_as_a_singleton_vote`
  covers the batch-member refusal on the singleton path.
- `chain_submission::tests::a_rejected_attempt_hash_is_not_a_reconciliation_candidate`
  and `a_committed_failure_retires_its_attempt_and_frees_the_next_submission`
  cover attempt retirement and the unblocking of a replacement payload.
- `chain_submission::tests::an_unreadable_lookup_is_reported_unknown_and_still_blocks_rebroadcast`
  covers the `OutcomeUnknown` reconciliation outcome and its rebroadcast bar.
- `chain_submission::tests::outcomes_are_journaled_under_the_wallet_that_reserved_them`
  and `confirmation_persists_under_the_captured_wallet` cover wallet capture
  across an account switch, for both the journal and the confirmation write.
- `chain_submission::tests::a_committed_failure_also_clears_the_legacy_domain_hash`
  and `retirement_never_clears_a_confirmed_domain_hash` cover domain-hash
  retirement and its confirmed-row boundary.
- `chain_submission::tests::adopting_a_success_clears_a_conflicting_unconfirmed_domain_hash`
  covers adopting a proven success past an opaque legacy identifier,
  `a_confirmation_that_fails_validation_keeps_the_competing_hash` covers the
  validation-before-mutation ordering that keeps a competing candidate when the
  events do not bind, and
  `chain::tests::request_timeout_is_bounded_so_a_reservation_cannot_outlive_the_grace`
  covers the deadline cap the reservation freshness bound depends on.
- `chain_submission::tests::cancellation_is_observed_before_the_no_candidate_fast_path`
  and `cancellation_during_the_no_candidate_reads_is_observed` cover the
  cancellation checks on either side of that path's state reads.
- `chain_submission::tests::the_identity_lock_registry_is_bounded_by_live_operations`
  covers shared locking for concurrent operations and reclamation afterwards.
- `chain_submission::tests::a_padded_legacy_hash_is_treated_as_opaque` covers the
  whitespace-exact candidate rule.
- `chain_submission::tests::an_earlier_unknown_attempt_survives_a_later_rejection`
  covers evidence precedence across attempts within one call.
- `chain_submission::tests::a_durable_confirmation_is_not_downgraded_by_a_lagging_endpoint`
  covers the durable-confirmation short circuit and its hash-only outcome.
- `chain_submission::tests::a_hashless_unknown_attempt_survives_across_calls`
  covers durable ambiguity with no learned hash.
- `chain_submission::tests::a_definite_pre_dispatch_failure_is_not_recorded_as_ambiguity`
  covers the boundary that keeps a definite rejection terminal, and
  `a_reservation_failure_after_an_ambiguous_post_stays_unknown` covers a retry
  whose reservation fails while an earlier attempt may still commit.
- `chain_submission::tests::a_candidate_recorded_between_attempts_stops_further_dispatch`
  covers the between-retry dispatch gate, and
  `an_accepted_candidate_recorded_mid_call_is_not_overridden_by_a_rejection`
  covers the same gate on the rejection path, where the candidate's `accepted`
  state is invisible to the live-attempt query, and
  `a_cancelled_reconciliation_does_not_make_a_rejection_terminal` covers the
  candidate keeping that outcome non-terminal when the reconciliation settled
  nothing.
- `vote::tests::batch_recovery_is_bound_to_the_row_that_supplied_it` covers
  per-member row binding for atomic batches.
- `vote::tests::recovery_replacement_is_refused_while_an_attempt_covers_the_row`
  covers the persistence-transaction guard and its idempotent rewrite.
- `chain::tests::a_422_lookup_claiming_success_is_unusable` and
  `a_422_broadcast_claiming_success_is_unusable` cover the status/body
  contradiction on both the lookup and mutation paths.
- `chain_submission::tests::cancellation_after_an_ambiguous_post_preserves_the_ambiguity`
  and `cancellation_before_any_dispatch_is_reported_as_cancelled` cover the
  boundary between the two cancellation outcomes.
- `chain_submission::tests::a_committed_failure_does_not_override_hashless_ambiguity`,
  `every_committed_failure_candidate_is_retired`,
  `a_successful_candidate_survives_a_terminal_lookup_error`,
  `a_terminal_lookup_error_does_not_downgrade_durable_ambiguity`,
  `a_pending_candidate_outranks_another_candidates_lookup_error`,
  `a_candidate_journaled_during_a_failing_lookup_outranks_its_error`,
  `a_candidate_recorded_before_the_reservation_stops_the_dispatch`,
  `a_cancelled_reservation_race_preserves_earlier_ambiguity`,
  `a_confirmation_applied_during_the_post_outranks_acceptance`,
  `a_confirmation_applied_during_the_final_post_is_not_downgraded`,
  `adopting_a_success_still_retires_the_failed_candidates`, and
  `a_spent_nullifier_response_accepts_a_durable_confirmation` cover the
  reconciliation precedence and retirement rules.
- `chain_submission::tests::a_spent_nullifier_with_an_unsettled_lookup_stays_unknown`
  and `a_spent_nullifier_cancelled_mid_lookup_is_not_reported_unresolved` cover
  the spent-nullifier path reporting a reconciliation that settled nothing as it
  stands.
- `chain_submission::tests::a_recovery_row_identity_mismatch_is_refused_before_dispatch`
  covers binding a recovered payload to its storage row.
- `round::tests::delete_skipped_bundles_refuses_to_prune_an_attempted_bundle`,
  `delete_skipped_bundles_prunes_past_a_rejected_attempt_and_its_journal_row`,
  `delete_skipped_bundles_still_refuses_a_hashless_delegation_attempt`,
  `delete_skipped_bundles_prunes_past_a_hashless_vote_attempt`,
  `delete_skipped_bundles_refuses_a_legacy_domain_candidate`,
  `delete_skipped_bundles_prunes_past_an_opaque_legacy_identifier`, and
  `delete_skipped_bundles_refuses_even_a_rejected_delegation_attempt` cover the
  bundle-pruning guard, its rejected-attempt boundary, and the split between
  delegation's unconditional bar and a vote attempt that can never be confirmed.
- `session::tests::ballot_intent_change_is_refused_while_a_vote_attempt_is_live`,
  `session::tests::reselecting_the_same_choice_is_not_refused_by_a_live_attempt`,
  `session::tests::a_rejected_attempt_does_not_refuse_a_ballot_intent_change`,
  `session::tests::a_hashless_unknown_attempt_does_not_refuse_a_ballot_intent_change`,
  and `session::tests::an_in_flight_reservation_still_refuses_a_ballot_intent_change`
  cover the ballot-intent guard and both of its anti-deadlock boundaries.
- `storage::operations::tests::attempted_delegation_cleanup_preserves_van_randomizer`
  covers the post-attempt `van_comm_rand`, legacy hash, and Keystone-signature
  cleanup prohibition.
- `storage::operations::tests::attempted_vote_cleanup_preserves_exact_recovery_generation`
  covers post-attempt vote payload recovery across generic recovery cleanup.
- `storage::operations::tests::batch_attempt_protection_is_scoped_to_its_own_digest`,
  `storage::operations::tests::unparseable_vote_recovery_is_protected_while_a_batch_attempt_is_live`,
  and `storage::operations::tests::a_rejected_batch_attempt_does_not_freeze_recovery_state`
  cover digest-scoped cleanup protection, its conservative unreadable-recovery
  case, and the rejected-attempt boundary.
- `chain_submission::tests::a_hashless_unknown_attempt_stops_covering_its_vote_row`
  and `an_accepted_hash_still_covers_its_vote_row` cover the coverage rule end to
  end, including that the call still reports the ambiguity it dropped coverage
  for.
- `storage::operations::tests::a_hashless_unknown_vote_attempt_does_not_freeze_recovery_state`
  covers the same rule at the recovery-cleanup boundary, and
  `a_reservation_still_in_flight_protects_its_recovery_generation` and
  `a_reservation_older_than_any_deadline_protects_nothing` cover the freshness
  bound on a hashless reservation, the second without reopening the database.
  `chain_submission::tests::a_reservation_this_process_awaits_survives_a_wall_clock_jump`
  covers the in-memory registry taking precedence over that bound, and its
  release afterwards.
  `chain_submission::tests::a_heartbeat_marks_only_an_outstanding_reservation_as_still_owned`
  covers the refresh that makes `updated_at` mean "the owner was alive", its
  state and wallet scoping, and its margin against the grace period.
  `chain_submission::tests::in_flight_coverage_does_not_cross_databases` covers
  the identity keying that keeps two handles minting the same row id apart, and
  `the_in_flight_guard_is_held_before_the_reservation_is_journaled` covers the
  guard being in place while the reservation transaction is still open.
  `session::tests::a_stale_reservation_does_not_refuse_a_ballot_intent_change`
  covers the same bound at the ballot-intent guard.
- `storage::operations::tests::recovery_cleanup_preserves_a_legacy_candidate_hash_and_its_recovery`,
  `recovery_cleanup_preserves_a_legacy_delegation_candidate`, and
  `recovery_cleanup_still_clears_an_opaque_legacy_identifier` cover an
  unconfirmed domain hash as coverage and the opaque-identifier boundary that
  keeps it from freezing a row.
- `storage::operations::tests::one_transaction_cannot_confirm_two_bundles`,
  `one_transaction_cannot_confirm_two_singleton_votes`,
  `atomic_batch_members_still_share_one_transaction`, and
  `an_opaque_legacy_identifier_may_repeat_across_bundles`,
  `a_hash_journaled_for_another_bundle_cannot_be_confirmed_here`, and
  `a_hash_journaled_for_this_submission_still_confirms` cover one transaction
  confirming one submission, the batch exception, the opaque-identifier
  boundary, and ownership read from the attempt journal.
- `chain_submission::tests::a_reservation_is_stamped_after_its_blocking_validation`
  and `the_in_flight_guard_is_released_before_the_backoff` cover the reservation
  timestamp and the guard's lifetime.
- `chain_submission::tests::a_hashless_attempt_outranks_a_merely_pending_candidate`
  and `an_accepted_hash_survives_an_unreadable_final_reread` cover the two
  precedence rules above, and
  `a_candidate_journaled_during_a_pending_lookup_is_reported` covers the
  candidate set being rebuilt after the lookups.
- `storage::operations::tests::mixed_case_tx_hashes_are_stored_lowercase_and_replay_stays_idempotent`
  covers storage-boundary hash canonicalization and idempotent replay.
- storage migration tests cover version 18 fresh and in-place schemas, including
  `storage::migrations::tests::v18_canonicalizes_existing_hex_tx_hashes_and_keeps_legacy_identifiers`.
- The `prelude` doc test covers the documented import path reaching the chain
  lifecycle surface.
- Vizor's `voting_providers_test.dart` covers account-switch cancellation,
  bounded spent-nullifier reconciliation, delayed transaction indexing,
  restart confirmation, and stale confirmation suppression through the FFI
  adapter.
