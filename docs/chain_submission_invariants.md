# Chain submission specification

## Status and purpose

This document defines the target vote-chain submission design for
`zcash_voting`. Implementation will follow this design. Until the implementation
and its named tests land, the document describes intended behavior rather than
current conformance.

The design deliberately follows Vizor's existing submission semantics:

1. A successful broadcast normally returns a transaction hash.
2. A known hash is reconciled through the transaction-status endpoint.
3. A request with no usable response or hash is retried using the same durable
   semantic generation.
4. A retry rejected because its exact input nullifier is already spent ignores
   the retry transaction's hash. That hash identifies the newly rejected retry,
   not the earlier transaction that consumed the nullifier.
5. If the generation's output positions are not already stored, the SDK
   reconciles every independently sourced candidate and then finds the expected
   outputs in the round commitment tree.

The high-level flow is:

```text
submit
├─ success + hash
│  └─ journal hash → poll hash → event confirmation
├─ spent nullifier
│  ├─ positions already stored → continue
│  ├─ recovery material present → reconcile independent candidates
│  │                              → scan tree if still unresolved
│  │                              → position confirmation
│  └─ recovery material missing → affected bundle is unrecoverable
└─ timeout or unusable response
   └─ preserve ambiguity → retry the same generation
```

Simplicity is an explicit requirement. The implementation uses one durable
attempt journal plus the existing bundle and vote rows. It does not introduce a
general workflow engine, scan epochs, durable scan cursors, a second normalized
recovery state machine, or a second table for historical hash ownership.

Changes to this design must update this document and the conformance tests in
the same pull request.

## Scope

The specification covers:

- delegation, singleton vote, and atomic vote-batch submission;
- durable reservation before every POST;
- retries, endpoint failover, and cancellation;
- transaction-hash reconciliation;
- structured spent-nullifier handling;
- bounded-memory, per-request-bounded, cancellable commitment-tree recovery
  after spent evidence when positions are missing;
- confirmation and position persistence;
- restart planning and per-bundle causal ordering;
- recovery cleanup and partial pruning; and
- schema version 18 rollout from version 17.

The principal implementation surfaces are:

- `chain`, for transport, endpoint validation, response decoding, transaction
  lookup, structured rejection parsing, limits, and cancellation;
- `chain_submission`, for durable attempts, retry gates, known-hash
  reconciliation, and tree-recovery orchestration;
- `confirmation`, for event confirmation and hashless position confirmation;
- `vote_commitment_tree_client`, for complete round-tree scans;
- `session`, `phases`, and `recovery`, for restart planning; and
- `storage`, for the attempt journal, bundle and vote positions, and cleanup
  guards.

## Trust and operating assumptions

### Trusted endpoints

Vote API and commitment-tree endpoints are trusted by design. The SDK validates
response syntax, internal consistency, and binding to local recovery material,
but it does not independently verify consensus proofs, transaction inclusion
proofs, or a quorum between endpoint operators.

Trusted endpoints are authoritative for:

- transaction commitment and execution code;
- structured rejection reasons;
- whether a nullifier is already in committed chain state;
- transaction events;
- round creation height when provided; and
- commitment-tree snapshots, blocks, roots, and leaf indexes.

Malformed, incomplete, contradictory, or unsupported responses are not accepted
as evidence merely because their endpoint is trusted.

### Single-process ownership

One process has exclusive access to a voting database at a time. Multiple tasks
inside that process are supported.

This is an enforced precondition, not an assumed convention.
`VotingDb::open` acquires an operating-system-backed exclusive ownership lock
for the canonical database path before migration or any voting-state access and
holds it for the lifetime of the owning database instance. Clones inside the
owner process share that ownership. An independent open while the lock is held
returns `Busy` and performs no migration, read, or write.

Separately copied database files have different paths and cannot be detected by
that lock. Concurrently operating such copies remains outside the supported
model and is a host error.

### Chain semantics

The design assumes:

1. Vote-chain transactions are atomic.
2. A committed nullifier can be consumed only once.
3. Structured `nullifier_already_spent` evidence refers to committed chain
   state, not merely a mempool or admission cache.
4. Round commitment trees are append-only within the chain's finality model.
5. The tree output layouts below are stable for the supported protocol version.

A spent-nullifier response proves that the named input was consumed by a
committed transaction. It does not alone prove which valid transaction consumed
it. The SDK confirms the local generation only after a known hash's events bind
to it or its complete expected output layout is found in the tree.

### Existing round authentication

`RoundAuthPayloadV2` remains unchanged. Tree recovery adds no round-auth field,
version, signature, or encoding.

The scan uses the trusted vote-chain round creation height when the configured
endpoint supplies it. If it is unavailable, the scan starts at height zero.
These values are transport bounds, not new authenticated voting inputs.

### Routing and privacy

The host owns direct, proxy, or Tor routing. A privacy route fails closed and
never falls back to a direct connection. Route changes cannot reuse pooled
connections created under an older policy.

Production endpoints use HTTPS or another authenticated encrypted route. Plain
HTTP is limited to explicit local or regtest configuration. Redirects are not
followed for mutation, transaction lookup, or tree scanning.

Retries and failover can expose the same signed submission and timing to more
than one endpoint operator. Tree recovery downloads public round-tree pages and
matches locally; it does not send the expected commitment as a lookup key.

## Submission identities and generations

### Submission identity

A submission identity is:

```text
(wallet, round, kind, bundle, proposal-or-batch)
```

Supported kinds are:

- delegation;
- singleton vote; and
- atomic vote batch.

Kind-specific constructors are the only way to create identities. A singleton
identity carries one proposal. A batch identity carries its complete ordered
batch digest. Storage constraints enforce the same pairing.

### Canonical submission

A canonical submission is a closed SDK wire type serialized to the exact JSON
accepted by the vote API:

- `DelegationSubmissionWire`;
- `VoteCommitmentWire`; or
- `VoteCommitmentBatchWire`.

The host cannot supply arbitrary JSON to the lifecycle.

Every retry in one live call sends byte-identical JSON. Singleton and batch
votes are exactly reconstructable after restart. Keystone delegation is exactly
reconstructable while its stored signature is available.

Software delegation does not durably retain its final SpendAuth signature. A
restart may therefore produce different request bytes and a different
transaction hash. It must still preserve the same semantic generation:
delegation setup, randomizer, input nullifiers, and VAN commitment.

### Payload digest

The payload digest is SHA-256 of one concrete canonical request body. It
distinguishes differently signed software-delegation attempts. It is never a
transaction hash and is never passed to the transaction-status endpoint.

### Recovery descriptor

Before the first POST, the SDK derives this closed descriptor:

```json
{
  "format": "zcash_voting_recovery_descriptor_v1",
  "wallet_id": "<exact wallet identifier>",
  "round_id": "<64 lowercase hexadecimal characters>",
  "kind": "delegation | vote | vote_batch",
  "bundle_index": 0,
  "proposal_id": null,
  "batch_digest": null,
  "input_nullifiers": ["<64 lowercase hexadecimal characters>"],
  "expected_outputs": [
    {
      "kind": "delegation_van | successor_van | vote_commitment",
      "commitment": "<canonical padded standard Base64>"
    }
  ],
  "votes": [
    {
      "proposal_id": 1,
      "choice": 0
    }
  ],
  "recovery_material_digest": "<64 lowercase hexadecimal characters>"
}
```

Fields appear in exactly the order shown and are serialized without optional
omission or insignificant whitespace. Delegation uses null `proposal_id` and
`batch_digest` and an empty `votes` array. Singleton uses one proposal, a null
batch digest, and one vote entry. Batch uses null `proposal_id`, its 32-byte
digest, and all vote entries in signed action order.

The generation digest is:

```text
SHA-256("zcash-shielded-vote:recovery-generation:v1" ||
        canonical_descriptor_json)
```

`canonical_descriptor_json` is the SDK's canonical serialization of that closed
type. Unknown fields, duplicate members, unsupported versions, noncanonical
encodings, or reordered batch members are rejected.

`recovery_material_digest` is SHA-256 of a typed canonical generation view:

- Delegation includes round, bundle, address index, total note value, hotkey
  raw address, `van_comm_rand`, `nf_signed`, ordered governance nullifiers,
  `gov_comm`, `cmx_new`, `rk`, `alpha`, proof, TX1 effects, and PCZT sighash. It
  excludes the nondurable SpendAuth signature, transaction hash, tree position,
  and timestamps.
- Singleton is the canonical persisted `VoteRecoveryBundle` with
  confirmation-only `vc_tree_position` normalized to null. It includes the
  vote signature, proof, encrypted shares, choice, nullifier, successor VAN,
  vote commitment, and helper-recovery inputs.
- Batch is the length-prefixed concatenation of those normalized singleton
  generation views in signed action order, preceded by the batch digest and
  member count. Counts and byte lengths are unsigned 32-bit little-endian
  integers.

The same typed encoders are used when creating and revalidating a descriptor.
Golden vectors for all three kinds prevent field-order or normalization drift.

Expected commitments are re-derived from these immutable inputs before every
scan and final write. A cached commitment is comparison evidence, not the source
of truth. In particular:

- delegation VAN derivation includes hotkey address coordinates, quantized
  ballot weight, round ID, and `van_comm_rand`; and
- successor VAN derivation includes the hotkey spending key, address index,
  total note value, `van_comm_rand`, round ID, proposal ID, and prior authority
  state.

### Input and output sets

The descriptor binds:

- Delegation inputs: signed-note nullifier and ordered governance nullifiers.
- Delegation output: exact `van_cmx`.
- Singleton input: exact VAN nullifier.
- Singleton outputs: successor VAN commitment followed by vote commitment.
- Batch inputs: VAN nullifiers in signed action order.
- Batch outputs: final successor VAN followed by vote commitments in signed
  action order.

The current chain layout is:

```text
delegation:  [VAN]
singleton:   [successor VAN, VC]
batch N:     [final successor VAN, VC[0], ..., VC[N-1]]
```

Atomic batch size is in `1..=15`. Intermediate batch VANs are proof-chain values
and are not appended to the global tree.

## Durable storage model

### One attempt journal

Version 18 uses `chain_submission_attempts` as its only new lifecycle table.
Its normative definition is:

```sql
CREATE TABLE chain_submission_attempts (
    id                       INTEGER PRIMARY KEY AUTOINCREMENT,
    round_id                 TEXT NOT NULL,
    wallet_id                TEXT NOT NULL DEFAULT '',
    kind                     TEXT NOT NULL
                                 CHECK (kind IN ('delegation','vote','vote_batch')),
    bundle_index             INTEGER NOT NULL CHECK (bundle_index >= 0),
    proposal_id              INTEGER NOT NULL DEFAULT -1,
    batch_digest             BLOB NOT NULL DEFAULT X'',
    payload_digest           BLOB NOT NULL CHECK (length(payload_digest) = 32),
    generation_digest        BLOB NOT NULL CHECK (length(generation_digest) = 32),
    recovery_descriptor_json TEXT NOT NULL
                                 CHECK (
                                     json_valid(recovery_descriptor_json)
                                     AND length(
                                         CAST(recovery_descriptor_json AS BLOB)
                                     ) <= 65536
                                     AND json_type(
                                         recovery_descriptor_json,
                                         '$.format'
                                     ) = 'text'
                                     AND json_extract(
                                         recovery_descriptor_json,
                                         '$.format'
                                     ) = 'zcash_voting_recovery_descriptor_v1'
                                 ),
    chain_tx_hash            TEXT,
    state                    TEXT NOT NULL
                                 CHECK (
                                     state IN (
                                         'attempting',
                                         'outcome_unknown',
                                         'accepted',
                                         'rejected'
                                     )
                                 ),
    rejection_code           INTEGER
                                 CHECK (
                                     rejection_code IS NULL
                                     OR rejection_code BETWEEN 1 AND 4294967295
                                 ),
    rejection_reason         TEXT
                                 CHECK (
                                     rejection_reason IS NULL
                                     OR length(rejection_reason) <= 64
                                 ),
    spent_nullifier          BLOB
                                 CHECK (
                                     spent_nullifier IS NULL
                                     OR length(spent_nullifier) = 32
                                 ),
    tree_recovery_json       TEXT
                                 CHECK (
                                     tree_recovery_json IS NULL
                                     OR (
                                         json_valid(tree_recovery_json)
                                         AND json_type(
                                             tree_recovery_json,
                                             '$.format'
                                         ) = 'text'
                                         AND json_extract(
                                             tree_recovery_json,
                                             '$.format'
                                         ) = 'zcash_voting_tree_recovery_v1'
                                         AND length(
                                             CAST(tree_recovery_json AS BLOB)
                                         ) <= 16384
                                     )
                                 ),
    diagnostic               TEXT NOT NULL DEFAULT ''
                                 CHECK (length(CAST(diagnostic AS BLOB)) <= 4096),
    created_at               INTEGER NOT NULL CHECK (created_at >= 0),
    updated_at               INTEGER NOT NULL CHECK (updated_at >= 0),
    FOREIGN KEY (round_id, wallet_id)
        REFERENCES rounds(round_id, wallet_id) ON DELETE CASCADE,
    CHECK (
        (kind = 'delegation' AND proposal_id = -1 AND length(batch_digest) = 0)
        OR
        (kind = 'vote' AND proposal_id >= 0 AND length(batch_digest) = 0)
        OR
        (kind = 'vote_batch' AND proposal_id = -1 AND length(batch_digest) = 32)
    ),
    CHECK (
        chain_tx_hash IS NULL
        OR (
            length(chain_tx_hash) = 64
            AND chain_tx_hash = lower(chain_tx_hash)
            AND chain_tx_hash NOT GLOB '*[^0-9a-f]*'
        )
    ),
    CHECK (
        (state = 'accepted'
            AND chain_tx_hash IS NOT NULL
            AND rejection_code IS NULL
            AND rejection_reason IS NULL
            AND spent_nullifier IS NULL)
        OR
        (state = 'rejected'
            AND chain_tx_hash IS NULL
            AND rejection_code IS NOT NULL
            AND rejection_reason IS NOT NULL)
        OR
        (state IN ('attempting','outcome_unknown')
            AND chain_tx_hash IS NULL
            AND rejection_code IS NULL
            AND rejection_reason IS NULL
            AND spent_nullifier IS NULL)
    ),
    CHECK (
        spent_nullifier IS NULL
        OR (
            state = 'rejected'
            AND rejection_reason = 'nullifier_already_spent'
        )
    ),
    CHECK (
        tree_recovery_json IS NULL
        OR (state = 'rejected' AND spent_nullifier IS NOT NULL)
    )
);

CREATE INDEX chain_submission_attempts_identity
    ON chain_submission_attempts (
        round_id,
        wallet_id,
        kind,
        bundle_index,
        proposal_id,
        batch_digest,
        generation_digest,
        id
    );

CREATE INDEX chain_submission_attempts_candidates
    ON chain_submission_attempts (wallet_id, chain_tx_hash)
    WHERE chain_tx_hash IS NOT NULL;

CREATE INDEX chain_submission_attempts_spent
    ON chain_submission_attempts (
        round_id,
        wallet_id,
        kind,
        bundle_index,
        proposal_id,
        batch_digest,
        generation_digest
    )
    WHERE spent_nullifier IS NOT NULL;

CREATE UNIQUE INDEX chain_submission_attempts_one_tree_receipt
    ON chain_submission_attempts (
        round_id,
        wallet_id,
        kind,
        bundle_index,
        proposal_id,
        batch_digest,
        generation_digest
    )
    WHERE tree_recovery_json IS NOT NULL;
```

The storage API parses both JSON columns into closed typed values before every
insert or update. It requires descriptor identity fields to equal the SQL
identity columns, requires the receipt generation digest to equal
`generation_digest`, and writes a receipt only to the lowest-ID matching spent
attempt. It also checks conversion of SQL integers into their domain types,
exact wallet identity, and canonical encodings of round, digest, and hash
values. These cross-field and range rules are enforced in the same immediate
transaction; SQL's shape checks are corruption guards, not the primary
validator.

The SQL format checks are deliberately two-part. `json_type(..., '$.format') =
'text'` rejects a missing, JSON-null, or non-text tag before exact value
comparison. This is required because SQLite accepts a `CHECK` expression whose
result is null; `json_extract(..., '$.format') = ...` alone would therefore
accept an object with no `format` member.

Each row represents one POST attempt and stores:

- existing identity columns;
- ordered attempt ID;
- payload digest;
- generation digest;
- versioned recovery descriptor JSON;
- optional normalized transaction hash;
- attempt state;
- optional structured rejection code and reason;
- optional canonical spent nullifier;
- optional versioned tree-recovery receipt JSON;
- bounded diagnostic text; and
- creation and update timestamps.

Attempt state remains simple:

- `attempting`;
- `outcome_unknown`;
- `accepted`; or
- `rejected`.

Tree recovery does not add another attempt state. A rejected retry may carry
spent-nullifier evidence and, later, a tree-recovery receipt proving the earlier
semantic transaction.

Attempts for one generation repeat the same generation digest and descriptor.
Different payload digests are allowed only for generation-equivalent software
delegation signatures.

### Derived outcome precedence

The lifecycle derives one chain-evidence result from all attempts and domain
rows for the generation. Classification applies the following rows from top to
bottom; the first matching row is the result. `known_tx_hashes` is the complete
normalized, deduplicated, lexicographically sorted set established by the final
snapshot and by stronger in-memory evidence from the current call.

| Priority | Predicate | Outcome |
| ---: | --- | --- |
| 1 | A bound hash-event confirmation is durable at the final snapshot. | `Confirmed` if this call committed it; otherwise `AlreadyConfirmed { source: hash_events }` |
| 2 | A tree-recovery receipt is durable at the final snapshot and no hash-event confirmation wins above. | `RecoveredByTree` if this call committed it; otherwise `AlreadyConfirmed { source: commitment_tree }` |
| 3 | Exact spent-nullifier evidence exists without a durable position result. | `SpentPositionPending` |
| 4 | A fresh accepted hash could not be journaled and there is no independent unsettled candidate or hashless ambiguity. | `AcceptedButUnjournaled` |
| 5 | Any hashless possibly dispatched attempt, unreadable or contradictory candidate, unjournaled accepted hash accompanied by independent unsettled evidence, or other evidence that commitment remains unknown exists. | `OutcomeUnknown` |
| 6 | At least one live candidate has a usable pending lookup result and no stronger row matches. | `Pending` |
| 7 | The current POST produced a journaled accepted hash, no other candidate is pending or unknown, and no stronger row matches. | `Accepted` |
| 8 | At least one attempt is definitely rejected and every other attempt is definitely rejected, definitely unsent, or proven committed-failed in this pass. | `Rejected` |
| 9 | Cancellation occurred before possible dispatch and no stronger row matches. | `Cancelled` |

An unjournaled accepted hash in priority 5 is included in
`OutcomeUnknown.known_tx_hashes`; its bounded storage error is included in the
message. Thus combining it with older ambiguity does not hide either item of
evidence. If both durable confirmation sources exist, priority 1 makes
`hash_events` deterministic. `Pending` outranks `Accepted` because it reports
the complete candidate set rather than selecting the most recently accepted
hash.

`Rejected` is selected only when every attempt is definitely rejected,
definitely unsent, or proven committed-failed in the current pass and there is
no older ambiguity, unsettled candidate hash, spent evidence, confirmed
position, or tree receipt. A later rejection can therefore never hide an
earlier `outcome_unknown` attempt. Its public `code` and `reason` come from the
qualifying rejected attempt with the lowest journal `id`. Every rejection
learned during the current pass is associated with its reserved attempt ID
before aggregation. Definitely-unsent attempts contribute no rejection
provenance and cannot displace that lowest-ID rejection. If no attempt supplies
a structured rejection, priority 8 does not match. This rule is independent of
endpoint, candidate, and attempt iteration order.

`Cancelled` is selected only when cancellation occurred before possible
dispatch and no stronger durable evidence exists.

`UnrecoverableBundleStatus` is not a rung in this ordering. It describes local
recovery capability, while the ordering describes chain evidence. Exact spent
evidence with missing private material therefore remains
`SpentPositionPending` and also carries an unrecoverable-bundle status. Pending
or unreadable candidates and known hashless ambiguity remain visible alongside
that status. Loss of local recovery material never turns unsettled chain
evidence into a terminal result.

### Monotonic evidence

Evidence is monotonic within a call and across durable state transitions. Once
memory or durable state establishes possible dispatch, a candidate hash, exact
spent evidence, a tree receipt, or confirmation, no later cancellation, lookup
error, persistence error, cleanup failure, or supplementary read may return an
outcome that denies or omits that evidence.

This rule applies at operation boundaries, not only to named branches:

- a failure while journaling or deleting an attempt cannot erase the known
  result of a POST;
- a later retry rejection cannot erase an earlier ambiguous dispatch;
- cancellation cannot replace a completed broadcast's result;
- a terminal error for one endpoint or candidate cannot settle another
  candidate;
- a failed supplementary read cannot discard an accepted hash or in-memory
  ambiguity already established by the call; and
- a stale network answer cannot downgrade durable confirmation.

Errors may accompany stronger evidence, but they do not replace it. An accepted
hash that cannot be journaled remains `AcceptedButUnjournaled`; other
post-dispatch persistence failures return the strongest established chain
outcome with bounded diagnostic context.

### Final classification snapshot

After network work and every durable retirement it was allowed to complete,
reconciliation takes one final read-transaction snapshot of:

- durable event or tree confirmation;
- the complete canonical candidate set;
- live hashless `attempting` or `outcome_unknown` attempts;
- exact spent evidence and any tree receipt; and
- local recovery capability for the generation.

For a non-cancelled pass, candidate retirement completes before this snapshot,
so returned candidate sets never include a hash the same call proved failed.
If cancellation arrives before retirement, the write is deferred: the
classification overlays the pass's in-memory committed-failure evidence on the
snapshot and does not report the failed hash as live, while durable mutation
guards continue treating the unretired row as covering until a later
reconciliation persists the retirement. Candidates or confirmation written by
another task during lookup are included. Cancellation is sampled after the
snapshot; it can suppress new work or writes but cannot demote the evidence the
snapshot contains.

The snapshot is the call's classification linearization point. Evidence
committed after its read transaction begins is observed by the next call.
A returned terminal outcome is not authority to bypass storage guards: every
generation replacement, ballot change, cleanup, and pruning transaction
re-reads current evidence after taking its write lock.

No classification branch performs another blocking database read after that
cancellation sample. If the final snapshot itself cannot be read, the call
still preserves all stronger evidence already held in memory and reports the
read failure as diagnostic context rather than manufacturing a weaker result.

### Existing domain rows remain operational

Existing columns remain the operational source for current positions:

- `bundles.van_leaf_position`;
- `bundles.delegation_tx_hash`;
- `votes.vc_tree_position`;
- `votes.tx_hash`; and
- persisted vote recovery JSON.

Hash-based confirmation and tree recovery update these rows through the same
atomic confirmation boundary.

The tree-recovery receipt on the attempt row preserves that generation's own VAN
and VC positions after the bundle's mutable VAN pointer advances. This allows a
transaction hash learned later to be checked against the original recovery.

### Tree-recovery receipt

The receipt JSON contains:

```text
{
  "format": "zcash_voting_tree_recovery_v1",
  "generation_digest": "<64 lowercase hex>",
  "round_tree_height": <u64>,
  "round_tree_next_index": <u64>,
  "round_tree_root": "<canonical padded base64 field element>",
  "van_leaf_position": <u64>,
  "vote_commitment_positions": [<u64>, ...]
}
```

Delegation has an empty vote-position array. Singleton has one position. Batch
positions use signed action order and match batch size.

All VAN and VC positions use `u64` in the lifecycle and wire API and must be at
most `i64::MAX` before SQLite persistence. Integer conversion is checked; no
position is truncated to the current `u32` VAN API. Position zero is a valid
position, never a pending sentinel.

The receipt is stored on the lowest-ID attempt for that generation carrying the
matching spent-nullifier evidence. Repeating the same receipt is idempotent.
Another receipt for different positions is a hard invariant error and writes
nothing.

### No durable scan workflow

Tree scanning is bounded in memory and per request, cancellable, and ephemeral,
but intentionally not bounded in total valid work. The database does not store:

- scan epochs;
- page cursors;
- partial match state;
- endpoint scan histories; or
- a separate normalized recovery-state table.

If a scan is interrupted, unavailable, malformed, or hits an individual request
bound, the next reconciliation starts a fresh scan. Partial results are never
evidence.

## Reservation-before-POST

### Entry validation

Every lifecycle call captures wallet and submission identity and validates:

- wallet and round;
- submission identity;
- bundle and proposal or batch digest;
- endpoint set and limits.

Cancellation is sampled on entry, but the call does not return `Cancelled`
before reading stronger durable evidence. Read-only reconciliation is allowed
for a cancelled invocation; cancellation prevents new network work and durable
mutation, not truthful reporting of evidence established by an earlier call.

Reconciliation reads durable state before requiring private recovery material:

1. A matching event confirmation or tree receipt returns `AlreadyConfirmed`.
2. Exact spent evidence with missing positions checks whether the descriptor and
   private recovery material remain usable.
3. Missing or corrupt required private material adds
   `UnrecoverableBundleStatus` while the chain outcome remains
   `SpentPositionPending`.
4. If entry cancellation is already true, the existing durable candidate,
   ambiguity, spent, and capability evidence is classified without a network
   request or write.
5. Otherwise, candidate hashes are reconciled.
6. Intact spent recovery material proceeds to tree recovery if hashes do not
   confirm.

Only a path that is about to create or retry a POST additionally requires:

- complete durable recovery material;
- canonical request body and payload digest; and
- recovery descriptor and generation digest.

For singleton, batch, and reconstructable Keystone delegation, the reservation
transaction re-derives the canonical request and descriptor from durable rows
and requires both digests to match the values about to be dispatched.

For restarted software delegation, the transaction re-derives every semantic
field and the descriptor, validates the supplied signature against the
generation's signing request, and verifies that the supplied canonical body
differs only in that permitted signature. It computes a fresh payload digest
for that concrete attempt. It does not claim to re-derive a nondurable
signature.

A recovered payload is bound to the exact storage row from which it was loaded.
Embedded round, bundle, proposal, choice, nullifiers, commitments, batch digest,
size, and order must match.

### Reservation

Every POST attempt is inserted in an immediate SQLite transaction before any
request byte is released. If reservation persistence fails, nothing is
dispatched.

A process-local in-flight guard is acquired before the reservation commits and
held until response classification finishes. It prevents same-process cleanup
or replacement while the POST is live.

Any `attempting` row with no matching process-local guard is an interrupted
reservation. Reconciliation and `resume_plan` atomically change it to
`outcome_unknown` before classifying or scheduling work. It is then handled like
every other possibly dispatched hashless attempt and retried with the same
generation.

### Definitely unsent

Cancellation before dispatch or a transport failure proven to occur before any
request byte is released removes only the fresh reservation.

Custom transports may report “definitely unsent” only at that boundary. Once
bytes are released to a network stack that may deliver them, failure is
ambiguous.

### Possibly dispatched

The following preserve the reservation as `outcome_unknown`:

- timeout;
- response-body failure;
- unusable success;
- accepted result without a valid hash;
- failure after request bytes were released;
- process interruption;
- HTTP 408 or 429 after POST;
- gateway error; and
- cancellation after dispatch.

An `outcome_unknown` attempt preserves its recovery descriptor and blocks
generation replacement. It does not immediately trigger a tree scan. The
lifecycle retries the same generation.

### Accepted

CheckTx success requires a canonical 32-byte transaction hash encoded as exactly
64 hexadecimal characters. The hash is normalized to lowercase and journaled as
`accepted`.

CheckTx acceptance does not update domain transaction-hash or position columns.
Those fields are written only after committed success.

If acceptance cannot be journaled, the lifecycle returns
`AcceptedButUnjournaled` carrying the hash and storage error. It never discards
the only known handle to a transaction that may commit.

### Rejected

A valid nonzero chain result is `rejected` for that exact attempt. It does not
erase older ambiguous attempts, accepted candidates, spent evidence, or a
tree-recovery receipt.

The attempt stores structured rejection information needed by the lifecycle.
Endpoint logs are diagnostic only.

## Retry behavior

### Default limits

| Limit | Default |
| --- | ---: |
| Complete request attempt | 10 seconds |
| POST attempts per lifecycle call | 3 |
| Retry backoffs | 2 seconds, then 4 seconds |
| Interpreted response body | 256 KiB |

Durations are nonzero and bounded. Endpoint sets are nonempty, bounded,
canonical, and distinct after URL normalization.

### Retryable results

The lifecycle may retry after:

- definite pre-dispatch transport failure;
- timeout or ambiguous transport failure;
- unusable response;
- accepted response without a usable hash;
- HTTP 408 or 429; and
- HTTP 500, 502, 503, or 504.

Other 5xx responses remain ambiguous even when not retried in the current call.
Redirects and unexpected success statuses are not followed or interpreted as
acceptance.

These limits are intentionally stricter than Vizor's current Dart client, which
can perform three attempts independently for every failover endpoint. The SDK
uses three POST attempts total across the endpoint set so total work is bounded.
It intentionally adds HTTP 408 and unusable/hashless success responses to the
retry set: either may hide a committed transaction, and obtaining a later hash
or exact spent response is the recovery mechanism defined here.

### Retry gate

Before every retry, the lifecycle:

1. persists the previous attempt classification;
2. reconciles every known candidate hash;
3. checks whether exact spent-nullifier evidence is now durable;
4. re-derives and verifies the same recovery descriptor;
5. checks bundle and ballot locks; and
6. checks cancellation.

If a known hash commits, retry stops and confirmation is applied.

If exact spent evidence exists, mutation retry stops permanently for that
generation and recovery proceeds by hash or tree.

If neither exists, another attempt may send the same generation. Singleton,
batch, and live delegation retries are byte-identical. Restarted software
delegation may vary only by its signature.

### Exhaustion

If bounded attempts end without a hash or spent evidence, the lifecycle returns
`OutcomeUnknown`. A later call repeats the same sequence from durable state.
The three-attempt limit is per lifecycle call, not a lifetime cap. Every later
attempt receives its own journal row and remains bound to the same generation.

`OutcomeUnknown` is recoverable. It blocks only replacement or successor work
on its own bundle. Independent bundles remain available.

## Transaction-hash reconciliation

### Candidate set

Candidate hashes come from:

- every non-rejected attempt with a canonical hash; and
- unconfirmed domain hash columns.

All candidates are normalized, deduplicated, and queried. The lifecycle never
chooses only the latest hash.

A live candidate or confirmed chain transaction hash may identify only one
semantic submission identity across every retained round for the captured wallet
and configured chain/network. An atomic batch is the sole exception: every
member row for that one batch may share its hash when all carry the same batch
digest. The exception does not permit reuse by a singleton member, another
batch, delegation, or another round.

Ownership discovery covers both the attempt journal and compatibility domain
hash columns. Checking ownership and journaling an accepted candidate occur in
one immediate transaction. Compatibility recording APIs perform their ownership
check and write under the same rule and transaction boundary.

A hash that is proven foreign is never made a live candidate for the current
identity. Because the current POST was nevertheless dispatched, its attempt
remains hashless `outcome_unknown`.

A hash proven to have committed unsuccessfully has no continuing ownership
role. Retirement removes it from candidate storage rather than keeping a
historical ownership record. If the same value later reappears through a
compatibility write or endpoint response, it is a fresh candidate and is
reconciled on the spot like every other candidate. The committed result for one
hash is immutable, so a usable failure retires it again, reopens the dispatch
gate in that same non-cancelled reconciliation, and cannot confirm any identity.

This deliberately relies on the trusted endpoint contract. An endpoint that
repeatedly attributes an old failed hash to unrelated new POSTs can force
repeated lookup and retirement; without retained historical ownership the SDK
does not claim to detect that behavior across calls.

### Lookup

`GET /shielded-vote/v1/tx/{hash}` returns:

- valid 200 committed success;
- valid 422 committed failure;
- valid 404 not yet committed; or
- unusable/transport failure.

Only 200, 404, and 422 are interpreted as transaction-status protocol bodies.
Their bodies must be bounded `application/json`, contain exactly one JSON
object and no trailing value, and reject duplicate or unknown top-level fields.
The accepted top-level shapes are:

```text
HTTP 404:
{"error":"tx not found"}

HTTP 200 or 422:
{
  "height": "<strict unsigned decimal string or exact JSON integer>",
  "code": <0 for 200; nonzero u32 for 422>,
  "log":"<bounded string>",
  "events":[<closed TxEvent>, ...]
}
```

`height` also accepts an exact JSON integer in `u64` range. HTTP 200 requires
code zero. HTTP 422 requires a nonzero `u32` code. `events` contains only the
closed supported `TxEvent` representations. A malformed 404 is unusable, not
pending. A status/body contradiction is unusable and cannot mutate state.
Other HTTP statuses are classified from the status before their bodies are
interpreted; an HTML rate-limit or gateway body does not manufacture decode
ambiguity.

Each configured endpoint receives one lookup attempt per pass. Syntactic
decoding and, for committed success, semantic event binding both occur inside
endpoint failover. A valid committed answer outranks 404. Unusable or
semantically unrelated success responses fail over. A committed failure is
definite for that hash and does not require its events to describe the local
submission. If no endpoint provides usable evidence, lookup returns unknown,
not pending.

Usable committed answers for one hash must agree on success or failure.
Semantically bound success answers must also agree on the event-derived
positions. Contradictory committed answers produce `OutcomeUnknown` with the
candidate preserved and an invariant diagnostic; they neither confirm nor
retire it. A previously durable confirmation still outranks that network
contradiction.

Endpoint and candidate results are aggregated by evidence, not loop order. A
terminal endpoint error such as 401 is inability to inspect that candidate; it
cannot outrank committed success, a valid pending response, an unusable response
that leaves commitment unsettled, a live ambiguous attempt, or another
independently sourced candidate. No error about one candidate settles another.

### Event binding

A committed-success hash is applied only after its events and referenced tree
positions bind to the recovery descriptor:

- Delegation verifies the reported VAN position contains the expected
  `van_cmx`.
- Singleton verifies the reported successor VAN and VC positions contain the
  expected commitments.
- Batch verifies digest, size, ordered proposals, ordered nullifiers, final VAN
  position, and every ordered VC position.

The chain does not know the wallet-local bundle index. Exact nullifiers,
commitments, proposal data, and batch digest bind the event to the local bundle.

### Reconciliation result

- Exactly one committed successful candidate confirms the generation.
- More than one successful candidate is an invariant violation and writes no
  confirmation, ownership, hash, or position state.
- A committed-failure candidate is retired.
- A 404 candidate remains pending and blocks another POST for that identity.
- An unreadable candidate remains unknown and blocks another POST for that
  identity.

Every failed candidate found in a non-cancelled pass is retired, including when
another candidate succeeds or when multiple successful candidates make
confirmation impossible. Retirement of an independently proven failure is not
confirmation state and remains valid despite the successful-candidate
conflict.

Retirement is durable and atomic. Every accepted attempt for that exact identity
carrying the hash becomes `rejected`, clears `chain_tx_hash`, stores the
committed nonzero code as `rejection_code`, stores the stable reason
`committed_failure` as `rejection_reason`, and stores the bounded redacted
endpoint log only as diagnostic text.

For delegation or singleton, the exact matching unconfirmed domain hash is
cleared only when its row has no confirmed position. For an atomic batch, the
same transaction clears the hash from every unconfirmed member carrying the
exact batch digest and from no row outside that batch. A hashless
`outcome_unknown` attempt is not retired by evidence about some other hash.
Restart candidate discovery therefore cannot rediscover the hash from the
carriers this retirement settled, and retirement cannot erase unrelated
ambiguity. A later writer may present the same value again; the next
reconciliation settles it from its immutable chain status instead of relying on
historical local ownership.

Cancellation before retirement performs none of those writes. The current
result still reflects the committed failure in memory, but every storage
mutation gate continues to see the accepted row and refuses replacement until
a later non-cancelled reconciliation retires it.

A confirmation already stored in the database outranks every later network
answer. A lagging or pruned endpoint cannot downgrade it.

### Late hash after tree recovery

A hash learned after tree recovery is only a candidate. Its committed-success
events must agree with the receipt's immutable positions before the hash may be
attached. Agreement is idempotent. Disagreement is an invariant error and does
not alter the tree-confirmed state.

## Structured spent-nullifier handling

### Response contract

Tree recovery starts only from structured evidence:

```text
StructuredRejection::NullifierAlreadySpent {
    state: committed,
    nullifier: [u8; 32],
}
```

The response carries a canonical 32-byte nullifier and explicitly states that it
is spent in committed chain state.

It is transported as an HTTP 422 JSON body:

```json
{
  "tx_hash": "<64 hexadecimal characters or empty string>",
  "code": 9,
  "log": "<bounded diagnostic text>",
  "rejection": {
    "kind": "nullifier_already_spent",
    "state": "committed",
    "nullifier": "<64 lowercase hexadecimal characters>"
  }
}
```

The displayed code is illustrative; any nonzero chain code is allowed.
`rejection` uses exactly these field names. Duplicate or unknown fields inside
it are rejected.

`tx_hash` on a spent-nullifier response is not recovery evidence. Vote-sdk
returns the hash of the newly rejected retry, not the earlier committed
transaction that consumed the nullifier. The SDK does not journal, poll, or
otherwise treat that value as a candidate. Whether it is present, empty,
malformed, or absent does not change spent recovery.

Compatibility-log substring matching is not sufficient. A malformed,
unsupported, mempool-only, or contradictory response is unusable evidence and
does not start tree recovery.

### Exact binding

Inside the transaction that records the rejected retry, the lifecycle re-derives
the descriptor and requires the reported nullifier to equal:

- the delegation signed-note nullifier or one of its ordered governance
  nullifiers;
- the singleton VAN nullifier; or
- one of the batch VAN nullifiers in signed action order.

A foreign nullifier does not mutate state for this generation.

### Recovery ordering

After valid spent evidence:

1. journal the retry as rejected with the canonical spent nullifier;
2. stop all later POSTs for this generation;
3. discard any hash returned by the rejected retry;
4. return `AlreadyConfirmed` immediately if the generation is already durable,
   exposing historical positions only when a tree receipt preserves them;
5. reconcile every independently sourced candidate currently known, regardless
   of whether it became visible before or after the spent response;
6. if one candidate commits successfully, apply event confirmation; and
7. otherwise, if the required descriptor and private recovery material remain
   available, run tree recovery for the missing positions.

An independently sourced candidate is one read from the attempt journal or a
compatibility domain column, not the hash field of the newly rejected retry.
Observation time is not provenance: a candidate journaled after the spent
response may still come from an earlier concurrent POST. A candidate that
becomes visible after the current reconciliation pass remains journaled for the
next pass and is never discarded merely because of when it was observed.

A pending or unreadable independent candidate remains journaled, but it does
not prevent tree recovery after spent evidence. Tree recovery can establish the
positions while later hash polling remains optional provenance work.

If the descriptor, `van_comm_rand`, vote recovery, or another private value
required to derive and spend the expected successor is missing, the SDK cannot
recover it from the public tree. It returns `SpentPositionPending` with
`UnrecoverableBundleStatus` for that bundle. Independent bundles remain
available.

This is the only path that starts tree recovery.

### Durable pending state

If tree recovery does not find a complete output layout, the spent attempt
remains `rejected` with `spent_nullifier` set and no receipt.

The derived lifecycle result is `SpentPositionPending`. On restart,
reconciliation recognizes that row and retries known hashes or a fresh complete
tree scan. No additional database state is needed.

`SpentPositionPending` blocks mutation replay and successor work on the affected
bundle, but does not block independent bundles.

## Commitment-tree recovery

### Trigger

Tree recovery runs only when:

- exact structured spent evidence is durable;
- the generation's required output positions are not already durable;
- no previously known hash has confirmed the generation;
- the exact descriptor and private recovery material remain available; and
- no tree-recovery receipt is already stored.

Any hash carried by the spent response is ignored because it names the rejected
retry. Independently sourced candidates are still reconciled first, regardless
of when they became visible, but a pending or temporarily unreadable candidate
does not block the tree fallback.

A timeout or hashless response by itself does not scan the tree. It causes
same-generation retry, matching Vizor's existing behavior.

### Scan API

The SDK uses the existing round-tree endpoints:

- `GET /shielded-vote/v1/commitment-tree/{round_id}/latest`
- `GET /shielded-vote/v1/commitment-tree/{round_id}/leaves`
  with inclusive `from_height` and `to_height`.

The scan starts at the trusted round creation height when available. Otherwise
it starts at zero. It snapshots `latest` once, then scans through that height.

### Per-request and memory bounds

Each individual tree request uses:

| Limit | Default |
| --- | ---: |
| One HTTP request | 10 seconds |
| One interpreted response body (`latest` or page) | 1 MiB |

There is no whole-scan page, byte, or elapsed-time cap. A valid large tree must
remain recoverable, so the scanner continues page by page until the snapshotted
tree is complete, an endpoint fails, or cancellation is observed. Memory use is
bounded by streaming one page plus the small set of expected commitments and
candidate positions.

Every request is independently bounded and cancellable, including while waiting
for response bytes. Pagination must make progress, so a malicious or broken
endpoint cannot create an infinite zero-progress loop.

The scan is not persisted page by page. If every endpoint fails or cancellation
occurs, the current call returns `SpentPositionPending`. A later reconciliation
fetches fresh endpoint tips and starts over.

Before scanning pages, the SDK fetches `latest` from every reachable configured
endpoint. It orders valid snapshots by descending height and then descending
`next_index`, and scans the freshest first. Two snapshots at the same height
with different size or root are contradictory endpoint responses and are not
used for recovery in that call. If the freshest endpoint fails during scanning,
failover starts a new complete scan against the next-freshest snapshot.

### Page validation

For one scan attempt:

- `latest.height` and `latest.next_index` are nonnegative and checked for local
  integer conversion;
- blocks remain inside the requested height range;
- block heights strictly increase;
- the first returned `start_index` is zero;
- every later `start_index` equals the preceding end index;
- pagination cursors strictly advance or use zero as the terminal marker;
- every leaf is canonical padded standard Base64 decoding to one canonical
  32-byte Pallas field element;
- duplicate, missing, malformed, or overflowed indexes are rejected;
- the final scanned index equals `latest.next_index`; and
- the final root equals the advertised root.

An invalid endpoint response fails over where possible. It never becomes a
successful match or a definitive no-match.

### Local matching

The scanner compares leaves locally against the descriptor.

It scans the complete snapshot even after finding a match, so duplicate matches
are detected and request timing does not reveal the match position.

The complete layout must appear exactly once:

- Delegation: one expected VAN.
- Singleton: expected successor VAN immediately followed by expected VC.
- Batch: expected final successor VAN immediately followed by every expected VC
  in signed action order.

Independent occurrences of individual singleton or batch leaves do not count as
the complete layout.

### Results

One unique complete match produces exact absolute positions and proceeds to
atomic position confirmation.

No complete match, delayed indexing, partial match, duplicate match, malformed
response, cancellation, or endpoint exhaustion leaves the generation
`SpentPositionPending`. The result carries a retryable diagnostic but does not
create a permanent conflict state.

Because endpoints are trusted, a valid unique complete match is sufficient; no
cross-endpoint quorum is required.

Tree recovery never invents or populates a transaction hash.

## Confirmation and position persistence

### Hash-based confirmation

Hash-based confirmation atomically records:

- validated transaction hash;
- delegation VAN position, or singleton/batch VC positions;
- the successor VAN position;
- exact recovery JSON updates; and
- helper-plan advancement tied to the same recovery generation.

CheckTx acceptance alone records none of these domain fields.

### Tree-based delegation confirmation

One immediate transaction:

1. re-derives and verifies the exact descriptor;
2. verifies the spent attempt and scan result;
3. checks the tree leaf at the recovered position equals the expected VAN;
4. stores the VAN position;
5. stores the tree-recovery receipt on the spent attempt; and
6. marks the delegation phase confirmed by its position.

No hash is written.

### Tree-based singleton confirmation

One immediate transaction:

1. re-derives round, bundle, proposal, choice, nullifier, successor VAN, VC, and
   generation digest;
2. verifies the recovered VAN/VC layout;
3. stores the immutable receipt;
4. stores the VC position and updates the exact recovery JSON;
5. advances the bundle's current VAN pointer; and
6. advances a compatible helper plan to the confirmed generation.

No partial write is permitted.

### Tree-based batch confirmation

One immediate transaction:

1. loads every member of the batch digest;
2. verifies member count, order, proposal choices, nullifiers, final VAN, and
   every VC;
3. records each VC position in action order;
4. updates every member recovery JSON and compatible helper plan;
5. advances the bundle VAN once to the final position; and
6. stores one complete receipt.

Every member succeeds or none does.

### Idempotency

Reapplying identical hash events or an identical tree receipt is a no-op success.
A different hash or different position for an already confirmed generation is
an invariant error and writes nothing.

An older confirmation cannot rewind a bundle whose VAN pointer has advanced
through a later confirmed vote.

### Public outcomes

The high-level lifecycle returns a `ChainLifecycleResult` containing one
chain-evidence outcome and an optional `UnrecoverableBundleStatus`. The
chain-evidence outcome is:

```text
ChainLifecycleResult {
    outcome: ChainLifecycleOutcome,
    unrecoverable_bundle: Option<UnrecoverableBundleStatus>
}
```

- `Accepted { tx_hash }`;
- `AcceptedButUnjournaled { tx_hash, storage_error }`;
- `Pending { known_tx_hashes }`;
- `OutcomeUnknown { known_tx_hashes, message }`;
- `SpentPositionPending { known_tx_hashes, message }`;
- `RecoveredByTree { tree_recovery }`;
- `Confirmed { tx_hash, confirmation }`;
- `AlreadyConfirmed { tx_hash: Option<String>, source, tree_recovery: Option<TreeRecoveryReceipt> }`;
- `Rejected { code, reason }`; or
- `Cancelled`.

All wire/FFI discriminants use stable snake-case names. Within confirmed
outcomes, a hash is optional only for tree-confirmed outcomes.

`confirmation` is the existing kind-specific event-confirmation result and does
not require a tree snapshot receipt. `tree_recovery` is the receipt defined in
the storage section.

`AlreadyConfirmed.source` is `hash_events` or `commitment_tree`. For
`hash_events`, `tx_hash` is present and `tree_recovery` is absent; the result
does not invent historical positions from the bundle's mutable current VAN
pointer. For `commitment_tree`, `tree_recovery` is present and is the only
source of generation-specific historical positions; `tx_hash` is optional
because a compatible hash may be attached later.

`UnrecoverableBundleStatus` is present only for proven loss or corruption of
private recovery material. A missing transaction hash or tree position alone
is not unrecoverable. The accompanying chain-evidence outcome and complete
candidate set remain authoritative.

## Concurrency and cancellation

### Locks

A process-wide asynchronous identity lock serializes one submission identity.
Every VAN-consuming operation also takes a bundle lock before deriving its
generation and holds it through reservation or confirmation.

This prevents two proposal identities from constructing successors from the same
current VAN. Different bundles remain independent.

Lock order is:

1. bundle lock when required;
2. submission-identity lock;
3. database handle;
4. immediate SQLite transaction.

Lock registries hold weak references and remain bounded by live work.

### Wallet capture

The operation captures wallet identity once. Every durable read, attempt write,
lookup, tree scan, and confirmation uses that captured identity. Switching the
host's active account cannot retarget the operation.

### Cancellation

Every high-level call and `ChainTransport` request receives the same
cancellation token. Transport errors report one of two dispatch phases:
`DefinitelyUnsent` or `PossiblyDispatched`. Only the transport may report the
first, and only before releasing request bytes.

Cancellation is checked:

- on entry, before deciding whether new work may start;
- after lock and database acquisition;
- before reservation and dispatch;
- after every request;
- before retry and backoff;
- before transaction lookup classification;
- before each scan page and scan retry;
- before candidate retirement; and
- after the final classification snapshot; and
- inside the confirmation transaction, after both the database handle and
  SQLite write lock are acquired but before the first write.

Cancellation before dispatch removes the unsent reservation. Cancellation after
possible dispatch preserves `OutcomeUnknown`. Cancellation after spent evidence
preserves `SpentPositionPending`. Cancellation after a tree match but before its
write causes the next call to rescan.

No cancellation result hides a known unsettled hash, possible dispatch, spent
evidence, or confirmed positions. If cancellation is already true on entry, the
call may perform the minimum read-only durable reconciliation needed to return
stronger existing evidence, but starts no request, retry, scan, retirement, or
confirmation write.

### Host operation epoch

Hosts such as Vizor may use a monotonically increasing operation epoch.
Account/session invalidation advances it synchronously. Every SDK callback
compares the captured epoch before irreversible work.

## Bundle causality and restart planning

### Bundle-local dependency

Each bundle has an independent VAN chain:

```text
delegation VAN
  → vote or batch
  → successor VAN
  → later vote or batch
```

An unresolved generation blocks only work that would use its unknown successor.
It does not impose a round-global stop.

### Planning rules

`resume_plan` derives work from attempts and domain positions:

- accepted or domain hashes produce poll work;
- hashless `outcome_unknown` with reconstructable canonical bytes produces
  same-generation retry work;
- restarted software delegation without its nondurable SpendAuth signature
  produces signing work for the same generation;
- exact spent evidence with no receipt and intact private recovery produces
  tree-recovery work;
- exact spent evidence with missing private recovery produces no same-bundle
  action and adds an `UnrecoverableBundleStatus` entry to the plan;
- a tree receipt or event-confirmed positions produce confirmed state;
- committed but never attempted recovery material produces submit work; and
- no existing generation produces fresh cast work.

A step that says “poll” always carries a hash. A step that says “recover tree”
always carries a complete descriptor and exact spent evidence. Neither is
presented as fresh signing work.

`RoundPlan` exposes `unrecoverable_bundles` separately from `next_steps`.
Each entry names bundle, triggering kind, optional proposal or batch digest, and
a stable reason code. These entries do not make independent bundle actions
blocking.

### Canonical public projection

One journal-aware derived-state function is the source for every public view of
submission progress. In one read-transaction snapshot it combines domain
positions and compatibility hashes with all matching attempt rows, then derives
the phase, complete candidate set, chain-evidence outcome, required recovery
material, and local recovery capability together.

The following surfaces use that projection rather than independently reading
legacy domain columns:

- singular delegation and vote phase getters;
- plural phase and status queries;
- `RoundPlan`, delegation status, and every recovery-work variant;
- `recovery::round_snapshot`;
- recoverable commitment-bundle APIs; and
- all corresponding wire and FFI views.

No surface derives a phase from one source and the hash or recovery payload for
that phase from another. Journal-only CheckTx acceptance therefore appears as
submitted everywhere, carries the complete candidate set everywhere polling is
possible, and keeps the pending commitment bundle available. A surface never
emits `Submitted`, `Poll*`, or equivalent work without enough evidence to
continue safely.

Every polling or recovery surface either exposes the complete normalized,
deduplicated candidate set or directs the caller to lifecycle reconciliation.
It never chooses a preferred, domain, latest, or first hash. An opaque
compatibility identifier may remain visible as the value historically recorded
by the host, but it is not a transaction-status candidate and is never
substituted for the canonical candidate set.

### Planner API

The planner adds these stable variants to `NextStep`:

```text
SignDelegationRetry {
    bundle_index,
    generation_digest,
    signing_request
}

RetryDelegation {
    bundle_index,
    generation_digest
}

RetryVote {
    bundle_index,
    proposal_id,
    generation_digest
}

RetryVoteBatch {
    bundle_index,
    batch_digest,
    generation_digest
}

RecoverDelegationFromTree {
    bundle_index,
    generation_digest
}

RecoverVoteFromTree {
    bundle_index,
    proposal_id,
    generation_digest
}

RecoverVoteBatchFromTree {
    bundle_index,
    batch_digest,
    generation_digest
}
```

Existing submit and poll variants remain. Poll variants carry the complete
candidate hash set. Retry and tree variants carry only opaque identity and
generation digests across FFI; `SignDelegationRetry` additionally carries the
public signing request. The lifecycle reloads and validates the private
descriptor internally.

`RetryDelegation` is emitted only while the exact Keystone or software
signature needed for its canonical body remains available. After restart,
software delegation instead emits `SignDelegationRetry`. Its `signing_request`
contains the existing public PCZT signing payload, not private recovery data.
The host returns the signature through
`submit_signed_delegation_retry(bundle_index, generation_digest, signature)`.
Inside the reservation transaction, the lifecycle reloads the private
descriptor, verifies that the supplied signature authorizes that exact
generation, permits only the signature-dependent payload digest to change, and
then reserves before dispatch. A signature for another generation dispatches
nothing.

`unrecoverable_bundles` contains:

```text
UnrecoverableBundleStatus {
    bundle_index,
    kind,
    proposal_id: Option<u32>,
    batch_digest: Option<[u8; 32]>,
    reason:
        missing_descriptor
        | missing_van_randomizer
        | missing_vote_recovery
        | corrupt_private_recovery
}
```

These reason codes are exhaustive and use stable snake-case wire values.

### Work ordering

Within a bundle:

1. confirm known hashes;
2. recover a spent hashless generation from the tree;
3. retry an ambiguous same generation;
4. submit committed work;
5. create fresh successor work;
6. submit or confirm helper shares.

The planner fairly merges ready work from independent bundles. A pending or
failed operation on bundle 0 does not suppress ready work on bundle 1.

### Choice and batch locking

Once a possibly dispatched singleton generation exists, its proposal choice
cannot change or become skipped. Re-selecting the same choice is idempotent.

Once a possibly dispatched batch exists, every member choice, proposal, order,
and batch digest remains fixed. Omitting one member on retry is a conflict.

These locks remain until the generation is confirmed, definitively proven
unsubmitted without older ambiguity, or the round is explicitly deleted.

### Completion and weight

Event confirmation and tree confirmation both count as semantic completion.
Tree confirmation can enable helper-share work without a transaction hash.

`SpentPositionPending` does not count as confirmed. The plan reports confirmed
and unresolved bundle weight separately.

## Cleanup, pruning, and deletion

### Ordinary cleanup

Ordinary reset and cleanup preserve:

- every non-rejected or ambiguous attempt;
- every generation descriptor that may have been dispatched;
- candidate hashes;
- spent-nullifier evidence;
- tree-recovery receipts;
- delegation setup and `van_comm_rand`;
- vote and batch recovery JSON;
- locked ballot choices and batch membership; and
- helper plans bound to the generation.

Hashless ambiguity remains protected because exact same-generation retry may
later produce spent evidence and tree recovery.

### Delegation preservation

Once any delegation attempt is journaled, ordinary cleanup never removes its
setup, proof, PCZT inputs, nullifiers, or randomizer. Software retry may replace
only the nondurable signature.

### Partial pruning

Partial pruning refuses a range containing:

- an ambiguous or accepted attempt;
- exact spent evidence;
- a tree-recovery receipt;
- an unconfirmed canonical domain hash;
- attempted delegation setup; or
- vote recovery needed by an unresolved generation.

Bundle indexes are never renumbered. Imported capability bundle sets remain
indivisible.

Rejected attempts with no older ambiguity, no spent evidence, and no candidate
do not freeze unrelated vote recovery. Opaque non-hash identifiers do not freeze
pruning.

### Explicit deletion

Explicit round or account deletion is the destructive escape hatch. The host
cancels and drains same-process work before deletion. Deletion removes local
evidence but does not undo a transaction that may already be on-chain.

The SDK checks its process-local submission, bundle, and scan registries and
returns `Busy` while matching work remains. The host retries deletion only after
the cancellation drain completes.

## Schema version 18

Version 17 is the released migration source. Version 18 is unreleased and its
unreleased definition is replaced in place.

Implementation:

- folds the final `chain_submission_attempts` definition into the existing
  `17 -> 18` migration;
- includes the same definition in `001_init.sql`;
- sets `CURRENT_VERSION = 18`; and
- does not add a separate `18 -> 19` migration.

The version-17 source has no chain-submission attempt rows, so no attempt
backfill or evidence reinterpretation is required. Existing round, bundle, vote,
helper, hash, and position state is preserved.

During `17 -> 18`, existing domain hash values that are exactly 64 hexadecimal
characters are normalized to lowercase. Other values remain byte-for-byte
unchanged as opaque compatibility identifiers and are not transaction-status
candidates.

Development databases created with an earlier version-18 schema are disposable
and must be recreated. No migration between unreleased version-18 shapes is
required.

## Existing recording APIs

Public functions that directly record a hash remain compatibility surfaces.
They cannot insert spent evidence or tree receipts, and their hashes remain
candidates until reconciliation settles them as confirmed or committed
failure. A failed value recorded again is reconciled and retired again before
it can permanently block later submission.

Caller-supplied positions cannot create tree confirmation. The high-level
lifecycle is the only supported boundary for SDK-owned network submission and
hashless recovery.

## Host responsibilities

The host owns:

1. authenticated endpoint configuration;
2. fail-closed route behavior;
3. opening the voting database through the exclusive ownership boundary and
   treating `Busy` as another active owner;
4. account, round, and operation cancellation;
5. invoking restart reconciliation before requesting fresh signing;
6. retaining an `AcceptedButUnjournaled` hash until it can be recorded;
7. presenting pending recovery without claiming success; and
8. explicit destructive confirmation for round/account deletion.

The SDK owns:

- typed canonical submission;
- reservation-before-POST;
- generation binding;
- bounded retry and failover;
- structured spent-nullifier parsing;
- known-hash reconciliation;
- bounded-memory, per-request-bounded, cancellable tree scanning after spent
  evidence;
- event and position validation;
- atomic confirmation;
- bundle-local planning; and
- cleanup guards.

## Diagnostics and sensitive data

Endpoint logs are diagnostic text, not protocol evidence. Raw bodies,
nullifiers, commitments, proofs, signatures, encrypted shares, recovery
descriptors, and tree receipts are not emitted through general `Debug`,
`Display`, logging, or telemetry.

Diagnostic text is bounded, valid UTF-8, control-character escaped, and redacted
before persistence or display.

Redaction occurs before UTF-8 byte truncation. Canonical hashes, nullifiers,
commitments, proofs, signatures, and encrypted-share encodings are replaced by
typed markers; ASCII control characters other than newline and tab are escaped;
then at most 4 KiB is stored or exposed. Raw endpoint bodies are never logged.

The host protects the voting database and backups at rest.

## Reviewer checklist

### Dispatch and identity

- Is every POST reserved before bytes are released?
- Does reservation include the exact generation descriptor?
- Can persistence failure still dispatch?
- Are definitely-unsent and possibly-dispatched failures separated correctly?
- Can a caller inject arbitrary JSON, a hash, spent evidence, or positions?
- Can a batch member be treated as a singleton?
- Does restarted software delegation request and validate a replacement
  signature for the same generation before reservation?

### Retry and hash reconciliation

- Are retries the same generation?
- Can software delegation change anything except its signature?
- Are all known hashes reconciled before another POST?
- Does accepted-without-hash remain ambiguous?
- Can a later rejection erase older ambiguity?
- Are failed candidates retired without erasing stronger evidence?
- Does retirement atomically clear every matching attempt and exact domain
  carrier, including every member of one batch?
- Is a failed hash that appears again reconciled and retired again without
  permanently blocking dispatch?
- Can a pending hash be mistaken for committed failure?
- Is hash ownership checked across every round and submission kind for the
  captured wallet and chain/network and written atomically with the candidate?
- Do malformed 404 and semantically unrelated committed responses fail over?
- Does final classification use one post-retirement durable snapshot without
  later blocking reads?

### Spent evidence

- Is the nullifier structured, canonical, and tied to committed state?
- Is it matched to the exact descriptor inside the journal transaction?
- Can log substring matching start tree recovery?
- Does spent evidence stop all later POSTs for that generation?
- Is every hash returned by the rejected retry ignored?
- Is every independently sourced candidate retained and reconciled regardless
  of when it became visible?
- Does tree recovery run whenever positions remain missing and private recovery
  material is available?

### Tree recovery

- Does scanning start at round creation height or safe height zero?
- Is every request and page bounded and cancellable without imposing a
  whole-scan cap?
- Does pagination have a strict progress requirement?
- Are pagination, indexes, roots, and canonical leaves validated?
- Is the full tree scanned after a match?
- Must the complete expected layout appear exactly once?
- Do partial, duplicate, unavailable, and delayed results remain retryable?
- Can tree recovery synthesize a transaction hash?

### Confirmation

- Does final persistence re-derive the descriptor?
- Are singleton VAN and VC positions atomic?
- Are every batch member and final VAN atomic?
- Is the receipt stored with the domain positions?
- Can idempotent replay change state?
- Can an old confirmation rewind the VAN?
- Must a late hash agree with the immutable receipt?
- Can hash-based `AlreadyConfirmed` invent positions no immutable receipt
  preserves?

### Planning and cleanup

- Does ambiguity block only its bundle successor?
- Can independent bundles continue?
- Does every poll step have a hash?
- Do all phase APIs, plans, recovery snapshots, recoverable-bundle APIs, and FFI
  views derive from the same journal-aware projection?
- Does every polling surface expose all candidates or require lifecycle
  reconciliation rather than selecting one?
- Can journal-only acceptance produce `Submitted` while omitting its recovery
  material?
- Does every tree step have spent evidence and a descriptor?
- Are choices and batch membership locked after possible dispatch?
- Does cleanup preserve all retry and tree-recovery material?
- Can partial pruning delete unresolved evidence?
- Does explicit deletion avoid claiming to undo chain state?

### Concurrency and cancellation

- Does every VAN-consuming path take the bundle lock?
- Does opening the database enforce one process owner before state access?
- Is the lock order consistent?
- Is wallet identity captured once?
- Is cancellation checked around every network and database wait?
- Can cancellation hide a possible dispatch or spent evidence?
- Does every privacy-sensitive request use the host route?

## Conformance map

The implementation is conformant only when every test ID below exists and
passes. The paths and names are normative traceability anchors: a test may move
or be renamed only when this map is updated in the same change. Parameterized
tests must report each listed case separately enough to identify the failing
boundary.

### Attempt journal, generation, and retry

In `zcash_voting/src/chain_submission/tests/dispatch_reservation.rs`:

- `reservation_and_descriptor_commit_before_post` proves both durable rows
  precede release of request bytes.
- `reservation_failure_dispatches_nothing` proves every insert, descriptor
  validation, and commit failure is pre-dispatch.
- `definitely_unsent_removes_only_the_fresh_attempt` preserves older evidence.
- `ambiguous_transport_matrix_remains_outcome_unknown` covers timeout,
  response-body failure, unusable success, 408, 429, gateway failure,
  cancellation after dispatch, and interruption.
- `retries_preserve_generation_and_canonical_bytes` covers singleton, batch,
  Keystone delegation, and a live software signature.
- `software_delegation_restart_requires_same_generation_signature` proves
  `SignDelegationRetry`, rejection of a foreign signature, and reservation of a
  valid replacement before POST.
- `descriptor_golden_vectors_are_stable` covers delegation, singleton, and
  batch, including normalization of confirmation-only fields.
- `accepted_hash_survives_journal_failure` returns
  `AcceptedButUnjournaled`.
- `post_dispatch_persistence_failure_preserves_strongest_evidence` covers
  classification, rejection journaling, and definitely-unsent cleanup errors.

### Evidence and cancellation race matrix

In `zcash_voting/src/chain_submission/tests/cancellation_concurrency.rs`:

- `candidate_inserted_at_each_wait_is_in_final_snapshot` inserts a candidate
  during POST classification, candidate lookup, failure retirement, and the
  wait to begin the pre-classification read transaction.
- `confirmation_written_at_each_wait_outranks_weaker_results` writes durable
  confirmation during POST, lookup, retirement, and final classification and
  tests pending, unknown, accepted, and rejected local answers.
- `storage_failure_at_each_post_dispatch_boundary_preserves_evidence` injects a
  failure after every fallible persistence boundary in the attempt loop.
- `final_snapshot_read_failure_preserves_in_memory_evidence` covers an accepted
  hash, hashless ambiguity, spent evidence, and a committed failure learned by
  the current pass.
- `cancellation_at_each_wait_preserves_stronger_evidence` covers POST, lookup,
  retirement, final snapshot, retry backoff, and tree scan.
- `cancellation_before_retirement_defers_write_and_keeps_storage_guard` proves
  the current result reflects the failure while replacement remains blocked
  until a later pass persists retirement.
- `confirmation_cancellation_check_runs_behind_both_write_locks` cancels after
  the database mutex and after `BEGIN IMMEDIATE`; neither case writes.
- `final_snapshot_excludes_retired_and_includes_racing_candidates` proves both
  directions of candidate-set freshness.
- `evidence_committed_after_snapshot_is_seen_by_every_mutation_gate` races a
  compatibility candidate against ballot change, generation replacement,
  cleanup, and pruning after a terminal classification.
- `unrecoverable_status_does_not_hide_chain_evidence` covers pending,
  unreadable, and hashless-ambiguous evidence with missing private recovery.

In `zcash_voting/src/chain_submission/tests/reconciliation.rs`:

- `mixed_candidate_result_matrix_retires_every_failure` covers
  success+failure, failure+pending, failure+unreadable, two failures, and a
  concurrent new candidate.
- `aggregate_outcome_matrix_is_independent_of_iteration_order` permutes
  accepted, pending, unknown, and unjournaled-accepted evidence and verifies the
  complete priority table and sorted candidate set.
- `rejected_outcome_uses_lowest_attempt_id_provenance` permutes multiple
  rejection codes and reasons and requires the same oldest journaled rejection.
- `later_rejection_and_cancellation_never_erase_older_ambiguity` covers every
  retry gate and fallible exit.
- `terminal_lookup_error_is_below_every_positive_evidence_class` covers
  confirmation, spent evidence, pending, unusable response, candidate presence,
  and live hashless ambiguity.

### Transaction lookup and ownership

In `zcash_voting/src/chain/mod.rs`:

- `transaction_status_accepts_only_the_closed_200_404_422_contracts` covers
  duplicate fields, unknown fields, trailing JSON, missing fields, malformed
  JSON, body limit, media type, decimal-string height, and integer height.
- `malformed_404_is_unusable_and_fails_over` includes empty JSON, an array,
  malformed JSON, and a wrong error value with `application/json`.
- `status_body_contradiction_is_unusable` covers 200 with nonzero code and 422
  with zero code.
- `syntactic_and_semantic_lookup_failures_both_fail_over` includes wrong round,
  kind, proposal set, batch order, nullifier, and commitment events on
  committed-success responses.
- `contradictory_committed_endpoint_answers_write_nothing` covers
  success/failure disagreement and different event-derived positions for one
  hash, returning `OutcomeUnknown` with that candidate preserved.
- `lookup_aggregates_evidence_independently_of_endpoint_order` permutes success,
  pending, unusable, 401, and transport failure.
- `uninterpreted_status_body_does_not_manufacture_ambiguity` covers HTML 429
  and 5xx bodies.

In `zcash_voting/src/chain_submission/tests/reconciliation.rs`:

- `accepted_hash_is_journaled_without_domain_mutation` preserves the CheckTx
  boundary.
- `every_domain_and_journal_candidate_is_reconciled` includes multiple attempts
  and compatibility hashes.
- `pending_or_unreadable_candidate_blocks_another_post` covers each candidate
  source.
- `failed_candidates_are_retired_on_every_non_cancelled_exit` covers success,
  multiple successes, pending, unreadable, terminal error, and no-success exits.
- `committed_failure_retirement_writes_canonical_evidence` verifies the
  nonzero code, `committed_failure` reason, redacted diagnostic, cleared attempt
  hash, and exact unconfirmed domain-hash clearing.
- `failed_batch_retirement_clears_every_exact_member` verifies one atomic write
  across all members and no row outside the batch.
- `exactly_one_success_confirms_and_two_successes_write_no_confirmation` proves
  both branches and still retires an independently failed third candidate.
- `durable_confirmation_outranks_every_network_answer` covers confirmation
  arriving before and during lookup.
- `late_hash_must_agree_with_tree_receipt` covers exact agreement and every
  position mismatch.

In `zcash_voting/src/chain_submission/tests/candidate_ownership.rs`:

- `hash_ownership_scope_matrix` covers cross-round, cross-kind, cross-bundle,
  singleton-to-singleton, singleton-to-batch, batch-to-batch, and both
  journal/domain carrier directions for live and confirmed hashes, while
  permitting an otherwise identical hash on a different configured
  chain/network.
- `one_atomic_batch_may_share_exactly_one_hash` proves the sole ownership
  exception and rejects nonmembers.
- `ownership_check_and_accepted_insert_are_one_transaction` races two
  lifecycle writers.
- `compatibility_ownership_check_and_write_are_one_transaction` races two
  direct-recording writers.
- `foreign_post_hash_preserves_hashless_ambiguity` proves the dispatched
  attempt is not mislabeled definitely rejected.
- `reappearing_committed_failure_is_retired_again_in_the_same_pass` covers both
  lifecycle and compatibility candidate sources and then permits dispatch.
- `reappearing_committed_failure_never_confirms_another_identity` covers round,
  kind, bundle, singleton, and batch identities.

### Structured spent evidence

In `zcash_voting/src/chain_submission/tests/reconciliation.rs`:

- `spent_nullifier_binding_matrix` covers delegation signed-note and governance
  nullifiers, singleton VAN, every ordered batch VAN, and foreign or malformed
  values.
- `unstructured_or_mempool_spent_claim_never_starts_tree_recovery` covers logs
  and contradictory structured responses.
- `spent_evidence_commits_before_scan_and_stops_every_later_post` observes both
  durable ordering and restart behavior.
- `spent_response_hash_is_ignored_in_every_encoding` covers canonical,
  uppercase, malformed, empty, and absent values.
- `independent_candidate_visible_after_spent_is_retained` races an earlier
  concurrent POST's journal write with spent-response classification.
- `independent_candidates_are_reconciled_before_tree_recovery` covers domain
  and accepted candidates alongside live hashless outcome-unknown evidence.
- `pending_or_unreadable_candidate_does_not_block_tree_recovery` preserves the
  candidate while recovering positions.
- `durable_confirmation_or_receipt_bypasses_tree_scan` covers both confirmation
  sources.
- `missing_private_material_keeps_spent_evidence_visible` returns
  `SpentPositionPending` plus each stable `UnrecoverableBundleStatus` reason.

### Commitment-tree scan

In `zcash_voting/src/vote_commitment_tree_client/tests.rs`:

- `complete_layout_matrix_recovers_exact_positions` covers delegation,
  singleton adjacency, and ordered batch layout.
- `scan_origin_uses_creation_height_or_safe_zero` covers both origins.
- `delayed_indexing_and_endpoint_exhaustion_remain_retryable` returns
  `SpentPositionPending`.
- `invalid_page_matrix_fails_over` covers range, height, start index, cursor,
  absolute index, canonical field, final size, and final root failures.
- `latest_and_page_response_body_limits_are_independently_enforced` applies the
  1 MiB interpreted-body limit to both endpoints.
- `pagination_must_make_strict_progress` rejects cursor and index stalls.
- `freshest_consistent_snapshot_is_scanned_first` covers ordering and
  same-height root contradictions.
- `valid_scan_larger_than_128_pages_completes` proves there is no hidden total
  page cap.
- `very_large_valid_snapshot_is_constant_memory_and_cancellable` checks
  streaming memory behavior, per-request timeout, and prompt cancellation for
  an enormous finite `next_index`.
- `complete_scan_continues_after_match` detects duplicate complete layouts and
  does not expose match position through request count.
- `partial_duplicate_or_independent_leaves_do_not_confirm` covers every output
  kind.
- `scan_never_synthesizes_transaction_hash` covers success and pending results.

### Atomic persistence and historical evidence

In `zcash_voting/src/confirmation.rs`:

- `delegation_tree_receipt_and_van_are_atomic`.
- `singleton_receipt_van_vc_recovery_and_helper_plan_are_atomic`.
- `batch_receipt_all_members_final_van_and_helper_plans_are_atomic`.
- `identical_hash_or_receipt_replay_is_idempotent`.
- `conflicting_hash_or_receipt_writes_nothing`.
- `old_confirmation_cannot_rewind_later_van`.
- `hash_already_confirmed_does_not_invent_historical_positions` advances the
  bundle VAN after confirmation and requires a hash-only result.
- `tree_already_confirmed_returns_positions_from_immutable_receipt` performs the
  same advancement and verifies the historical receipt remains exact.

### Canonical public projection and planning

In `zcash_voting/src/chain_submission/tests/public_contract.rs`:

- `all_public_projections_agree_on_journal_only_acceptance` checks singular and
  plural phases, statuses, `RoundPlan`, recovery work,
  `recovery::round_snapshot`, recoverable commitment bundles, and wire/FFI
  views.
- `all_public_projections_expose_the_complete_candidate_set` combines multiple
  domain and journal candidates and rejects preferred-hash selection.
- `submitted_projection_never_omits_poll_or_recovery_evidence` covers
  journal-only delegation, singleton, and batch acceptance.
- `outcome_unknown_plans_exact_generation_retry`.
- `restarted_software_delegation_plans_signing_before_retry`.
- `spent_recoverable_generation_plans_tree_recovery`.
- `spent_unrecoverable_generation_reports_status_without_hiding_evidence`.
- `tree_confirmed_vote_enables_hashless_helper_work`.
- `blocked_bundle_does_not_suppress_independent_bundle`.
- `recovered_successor_resumes_later_same_bundle_work`.
- `choice_and_batch_membership_remain_locked_after_possible_dispatch`.
- `confirmed_and_unresolved_weight_remain_distinct`.

### Cleanup, exclusivity, and schema

In `zcash_voting/src/chain_submission/tests/recovery_coverage.rs`:

- `cleanup_preserves_ambiguous_descriptors_spent_evidence_and_receipts`.
- `delegation_randomizer_survives_every_attempt_outcome`.
- `partial_pruning_refusal_matrix` covers ambiguous, accepted, spent,
  tree-confirmed, canonical compatibility-hash, delegation, singleton, and
  batch evidence.
- `terminal_vote_evidence_releases_only_its_exact_generation` covers rejected
  attempts, retired hashes, exact batch scoping, and unrelated recovery.
- `opaque_compatibility_identifier_does_not_freeze_cleanup`.
- `explicit_deletion_requires_drained_process_work`.

In `zcash_voting/src/storage/migrations.rs`:

- `version_17_to_18_creates_final_attempt_schema_without_inventing_evidence`.
- `fresh_and_migrated_version_18_schemas_match`.
- `current_version_is_18_and_no_version_19_migration_remains`.
- `sql_checks_enforce_representable_kind_state_and_nullability_shapes`.
- `sql_json_format_checks_reject_missing_null_and_non_text_tags` covers both
  `recovery_descriptor_json` and non-null `tree_recovery_json`.
- `typed_storage_rejects_cross_field_descriptor_receipt_and_range_mismatch`
  proves the rules SQL alone cannot express.
- `one_tree_receipt_carrier_exists_per_generation`.

In `zcash_voting/src/storage/tests.rs`:

- `second_process_owner_is_rejected_before_database_access` acquires the
  operating-system lock from a child process and expects `Busy`.
- `owner_clones_share_one_exclusive_lock` proves supported same-process use.

### Vizor integration

Vizor integration coverage uses these stable scenario IDs:

- `chain_success_hash_poll_event_confirmation`;
- `chain_spent_ignores_retry_hash_reconciles_independent_candidates_then_scans`;
- `chain_spent_missing_private_material_advances_other_bundles`;
- `chain_timeout_or_unusable_response_retries_same_generation`;
- `chain_delayed_tree_indexing_remains_recoverable`;
- `chain_account_or_session_cancellation_suppresses_stale_writes`;
- `chain_journal_only_acceptance_restores_every_public_projection`; and
- `chain_unresolved_bundle_does_not_stop_independent_work`.
