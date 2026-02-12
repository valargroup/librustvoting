---
name: MsgSubmitTally tally entries
overview: Add TallyEntry support to MsgSubmitTally so the election authority can submit decrypted vote totals per (proposal_id, vote_decision) pair, validate them against on-chain accumulators, store finalized results, and expose them via query — completing the tally finalization flow.
todos:
  - id: proto
    content: Add TallyEntry to tx.proto, TallyResult to types.proto, TallyResults query to query.proto, regenerate pb.go
    status: completed
  - id: validate
    content: Update MsgSubmitTally.ValidateBasic to require non-empty entries with no duplicate (proposal_id, decision) pairs
    status: completed
  - id: keys
    content: Add TallyResultPrefix (0x07) and TallyResultKey helper to types/keys.go
    status: completed
  - id: keeper
    content: Add SetTallyResult, GetTallyResult, GetAllTallyResults to keeper.go
    status: completed
  - id: handler
    content: Update SubmitTally handler to validate entries against accumulators, store TallyResults, return count
    status: completed
  - id: query
    content: Implement TallyResults query in query_server.go and add GET /zally/v1/tally-results/{roundIdHex} REST endpoint
    status: completed
  - id: tests-go
    content: Update keeper tests, validate_test, ABCI tests, fixtures for tally entries
    status: completed
  - id: tests-ts
    content: Update TypeScript E2E helpers and voting-flow test to submit entries and query results
    status: completed
isProject: false
---

# MsgSubmitTally: Add Tally Entries and Result Storage

## Context

`MsgSubmitTally` currently exists as a stub that only transitions a round from TALLYING to FINALIZED. Per the spec (Appendix B), the election authority must submit **decrypted vote totals** for each `(proposal_id, vote_decision)` pair, along with a proof of correct decryption. The chain validates and stores these results.

Since the current tally accumulator uses plaintext `uint64` (not El Gamal ciphertext), "decryption verification" for Day 2 is a direct equality check: `entry.total_value == stored_accumulator`. When the ciphertext model is added later, this swaps to DLEQ proof verification — but the message structure, storage, and queries remain unchanged.

## Changes

### 1. Proto: Add TallyEntry and update messages

**[sdk/proto/zvote/v1/tx.proto](sdk/proto/zvote/v1/tx.proto)**

Add `TallyEntry` to `MsgSubmitTally` and a result in the response:

```protobuf
message MsgSubmitTally {
  bytes  vote_round_id          = 1;
  string creator                = 2;
  repeated TallyEntry entries   = 3;  // NEW: one per (proposal, decision) pair
}

message TallyEntry {
  uint32 proposal_id       = 1;
  uint32 vote_decision     = 2;
  uint64 total_value       = 3;  // Decrypted aggregate (zatoshi)
  bytes  decryption_proof  = 4;  // Chaum-Pedersen DLEQ proof (optional for now)
}

message MsgSubmitTallyResponse {
  uint32 finalized_entries = 1;  // Number of entries stored
}
```

**[sdk/proto/zvote/v1/types.proto](sdk/proto/zvote/v1/types.proto)**

Add `TallyResult` for on-chain storage:

```protobuf
message TallyResult {
  bytes  vote_round_id    = 1;
  uint32 proposal_id      = 2;
  uint32 vote_decision    = 3;
  uint64 total_value      = 4;
}
```

Regenerate with `buf generate`.

### 2. ValidateBasic: Validate entries

**[sdk/x/vote/types/msgs.go](sdk/x/vote/types/msgs.go)** — Update `MsgSubmitTally.ValidateBasic()`:

- `entries` must be non-empty
- No duplicate `(proposal_id, vote_decision)` pairs
- Each `total_value` must be > 0

### 3. Keeper: Store and retrieve tally results

**[sdk/x/vote/types/keys.go](sdk/x/vote/types/keys.go)** — Add key prefix:

```go
TallyResultPrefix = []byte{0x07}  // 0x07 || round_id || proposal_id || decision -> TallyResult protobuf
```

**[sdk/x/vote/keeper/keeper.go](sdk/x/vote/keeper/keeper.go)** — Add methods:

- `SetTallyResult(kvStore, *types.TallyResult) error` — store one result
- `GetTallyResult(kvStore, roundID, proposalID, decision) (*types.TallyResult, error)` — retrieve one
- `GetAllTallyResults(kvStore, roundID) ([]*types.TallyResult, error)` — retrieve all for a round

### 4. Handler: Validate entries against accumulators and store

**[sdk/x/vote/keeper/msg_server.go](sdk/x/vote/keeper/msg_server.go)** — Update `SubmitTally`:

```
For each entry in msg.Entries:
  1. Validate proposal_id < len(round.Proposals)
  2. Read stored accumulator: GetTally(kvStore, roundID, entry.ProposalId, entry.VoteDecision)
  3. Verify entry.TotalValue == stored accumulator value (plaintext model)
     (Future: verify DLEQ proof against ciphertext accumulator + ea_pk)
  4. Store TallyResult
Transition round to FINALIZED
Return count of stored entries
```

If any entry fails validation, the entire message is rejected (atomic).

### 5. Query: Expose finalized results

**[sdk/proto/zvote/v1/query.proto](sdk/proto/zvote/v1/query.proto)** — Add:

```protobuf
rpc TallyResults(QueryTallyResultsRequest) returns (QueryTallyResultsResponse);

message QueryTallyResultsRequest {
  bytes vote_round_id = 1;
}

message QueryTallyResultsResponse {
  repeated TallyResult results = 1;
}
```

**[sdk/x/vote/keeper/query_server.go](sdk/x/vote/keeper/query_server.go)** — Implement the handler using `GetAllTallyResults`.

### 6. REST API: Add query endpoint

**[sdk/api/query_handler.go](sdk/api/query_handler.go)** — Add:

```
GET /zally/v1/tally-results/{roundIdHex} -> QueryTallyResults
```

### 7. Tests

**[sdk/x/vote/keeper/msg_server_test.go](sdk/x/vote/keeper/msg_server_test.go)**:

- Happy path: entries match accumulators, round finalized, results stored
- Rejected: entry total_value mismatches accumulator
- Rejected: entry references non-existent proposal
- Rejected: duplicate (proposal_id, decision) pair in entries
- Rejected: empty entries list
- Results queryable after finalization

**[sdk/x/vote/types/validate_test.go](sdk/x/vote/types/validate_test.go)**:

- ValidateBasic: empty entries, duplicate pairs, zero total_value

**[sdk/testutil/fixtures.go](sdk/testutil/fixtures.go)**:

- Update `ValidSubmitTally` to include entries

**[sdk/tests/api/src/helpers.ts](sdk/tests/api/src/helpers.ts)**:

- Update `makeSubmitTallyPayload` to include entries

**[sdk/tests/api/src/voting-flow.test.ts](sdk/tests/api/src/voting-flow.test.ts)**:

- Update tally lifecycle test to submit entries and verify results via query

**[sdk/app/abci_test.go](sdk/app/abci_test.go)**:

- Update ABCI integration test with entries

## Non-goals (deferred)

- El Gamal ciphertext accumulator (requires `MsgRevealShare` changes first)
- Real DLEQ proof verification (proof format still TBD in spec)
- `decryption_proof` validation (field present but unchecked, like mock ZKPs)

