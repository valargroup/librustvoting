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
for its exact attempt.

## Typed identity and payload invariants

1. The supported mutations are delegation, singleton cast-vote, and atomic
   cast-vote batch.
2. A lifecycle call validates its exact wallet, round, bundle, proposal or
   batch digest, and durable recovery generation before any storage or network
   effect.
3. Delegation accepts `SignedDelegationBundle`; singleton vote accepts a
   recovered `CommittedVote`; atomic vote accepts a recovered
   `SignedVoteBatch`. The SDK derives network fields from these types.
4. The host cannot supply an arbitrary JSON map, a transaction hash to mark as
   accepted, or chain events to record outside the confirmation lifecycle.
5. Canonical JSON is serialized once for one call. Every retry in that call
   sends byte-identical content.
6. Server-returned transaction hashes must be exactly 32 bytes encoded as 64
   hexadecimal characters and are normalized to lowercase before persistence
   or lookup.
7. The local payload digest is never accepted where a chain hash is required.

## Durable dispatch invariants

1. Every POST attempt is reserved in an immediate SQLite transaction before
   transport dispatch.
2. If reservation persistence fails, no request is dispatched.
3. Cancellation or a definitely pre-dispatch transport failure removes only
   the fresh, definitely-unsent reservation.
4. Timeout, failure after dispatch, response-body failure, unusable success,
   or process interruption preserves the attempt as outcome-unknown.
5. The response classification is persisted before another attempt begins.
6. Repeated byte-identical attempts share a payload digest but remain distinct
   ordered attempts. A re-signed payload has a different digest.
7. Accepted chain hashes and all earlier unknown attempts remain available for
   reconciliation until explicit round or account deletion.
8. Routine session reset and recovery cleanup do not delete chain submission
   attempts.
9. CheckTx acceptance alone never updates the legacy delegation or vote
   submission columns. This prevents a later DeliverTx failure from pinning a
   domain row to a transaction that did not commit successfully.

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
2. Delegation confirmation does not clear those fields.
3. Resubmission MUST reuse the persisted delegation setup and
   `van_comm_rand`; it MUST NOT rebuild the bundle by sampling another
   randomizer.
4. Explicit round or account deletion is the only supported operation that
   removes attempted delegation setup.
5. The lifecycle reconciles every known chain hash before another dispatch.
   The host SHOULD invoke reconciliation before requesting fresh software
   signing work after restart.

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
monotonic clock. Endpoint lists must be nonempty, canonical HTTP or HTTPS base
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

Other 4xx responses are terminal except the spent-nullifier compatibility
case. Other 5xx responses are outcome-unknown but non-retryable in the current
call. Before a replay, the lifecycle queries every known chain hash. Exhausted
ambiguity returns `OutcomeUnknown`, never definite rejection.

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
3. Exactly one committed successful candidate is adopted and confirmed.
4. No successful known candidate returns `AlreadySpentUnresolved` and
   preserves all attempts and recovery material.
5. More than one committed successful candidate is an invariant violation and
   produces no partial domain mutation.
6. A rejected duplicate's hash is not evidence that it identifies the earlier
   successful transaction.

## Transaction lookup and confirmation invariants

1. Transaction lookup accepts only normalized 32-byte hexadecimal hashes.
2. HTTP 200 is a committed transaction response and parses height, code, log,
   and events.
3. HTTP 404 means not yet committed and may fail over to another endpoint.
4. A structured HTTP 422 transaction result is committed failure.
5. Invalid content type, oversized body, malformed JSON, or missing required
   fields is an unusable response, not confirmation.
6. Delegation confirmation uses `confirm_delegation_submission`; singleton vote
   uses `confirm_vote_submission`; atomic batch uses
   `confirm_vote_batch_submission`.
7. The winning hash and event-derived positions are committed together by the
   existing confirmation transaction. Attempt evidence is retained separately;
   a crash on either side is recoverable because the attempt hash and the
   domain record are both idempotent and neither overwrites conflicting state.
8. Atomic-batch members advance together or not at all.
9. Event round, bundle, proposal order, batch digest, and nullifier bindings are
   validated by the existing confirmation parser before writes.

## Concurrency and cancellation invariants

1. A process-wide asynchronous lock serializes operations for one submission
   identity. Unrelated submissions remain independent.
2. After acquiring the lock, the lifecycle re-reads the exact durable recovery
   generation. A stale handle fails before network dispatch.
3. The exact generation is re-read while holding the process identity lock.
   Attempt insertion and its round/owner validation then share one immediate
   SQLite transaction. The host advances cancellation before destructive
   session cleanup, so a cleanup race is observed before dispatch.
4. Cancellation is checked before reservation, dispatch, retry, failover, and
   backoff.
5. Cancellation observed after a request completes does not replace its result.
6. Cancellation before a fresh reservation dispatches removes the definitely
   unsent reservation. Cancellation after dispatch retains uncertainty.
7. A deleted or replaced generation cannot receive a delayed transport or
   confirmation result.

Vizor implements host cancellation with a monotonically increasing operation
epoch. Account/session invalidation advances the epoch synchronously; every
SDK callback compares the captured epoch before reservation, dispatch, retry,
and confirmation application.

## Persistence and compatibility invariants

Schema version 18 adds `chain_submission_attempts`. Launch-version databases
migrate in place and retain all existing round, delegation, vote, and helper
state. A newer unsupported schema remains rejected.

Each attempt stores wallet, round, kind, bundle, proposal sentinel or batch
digest, ordered attempt number, local payload digest, optional server chain
hash, evidence state, and timestamps. It stores neither the canonical request
body nor a locally predicted chain hash. Deleting the owning round cascades to
its attempts.

Existing public submission-recording functions remain compatible. The new
high-level lifecycle is the supported boundary for SDK-owned network
submission and confirmation.

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
- Are retries byte-identical within one live call?
- Is the software-delegation crash recovery gap still explicit?
- Can a spent-nullifier response discard known hashes or recovery material?
- Can an unresolved nullifier be reported as success?
- Can weaker evidence overwrite accepted or confirmed evidence?
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
- `chain_submission::tests::check_tx_acceptance_is_journaled_without_domain_mutation`
  covers the CheckTx/committed-domain separation.
- `chain_submission::tests::known_pending_hash_is_reconciled_without_another_post`
  covers reconcile-before-replay for accepted candidates.
- `chain_submission::tests::committed_failure_rejects_without_pinning_domain_hash`
  covers DeliverTx failure classification without a partial domain write.
- `storage::operations::tests::attempted_delegation_cleanup_preserves_van_randomizer`
  covers the post-attempt `van_comm_rand`, legacy hash, and Keystone-signature
  cleanup prohibition.
- `storage::operations::tests::attempted_vote_cleanup_preserves_exact_recovery_generation`
  covers post-attempt vote payload recovery across generic recovery cleanup.
- storage migration tests cover version 18 fresh and in-place schemas.
- Vizor's `voting_providers_test.dart` covers account-switch cancellation,
  bounded spent-nullifier reconciliation, delayed transaction indexing,
  restart confirmation, and stale confirmation suppression through the FFI
  adapter.
