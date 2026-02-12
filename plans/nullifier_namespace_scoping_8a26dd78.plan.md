---
name: Nullifier Namespace Scoping
overview: Scope the nullifier KV store by nullifier type (gov/VAN/share) and round ID to prevent cross-type and cross-round collisions, matching the spec's three independent nullifier sets.
todos:
  - id: keys
    content: Add NullifierType constants and update NullifierKey() in types/keys.go
    status: completed
  - id: keeper
    content: Update HasNullifier, SetNullifier, CheckNullifiersUnique signatures in keeper.go
    status: completed
  - id: msgs-interface
    content: Add GetNullifierType() to VoteMessage interface and implement on all 4 message types in msgs.go
    status: completed
  - id: ante
    content: Pass nullifier type + round ID through ante/validate.go
    status: completed
  - id: msg-server
    content: Update DelegateVote, CastVote, RevealShare handlers in msg_server.go
    status: completed
  - id: genesis-proto
    content: Add type and round_id fields to NullifierEntry in types.proto and regenerate
    status: completed
  - id: genesis-go
    content: Update InitGenesis/ExportGenesis in genesis.go for typed nullifiers
    status: completed
  - id: tests
    content: Update all existing tests and add cross-type/cross-round isolation regression tests
    status: completed
isProject: false
---

# Nullifier Namespace Scoping

## Problem

All nullifier types (gov, VAN, share) across all voting rounds share a single flat keyspace:

```
0x01 || nullifier_bytes
```

The spec requires three independent sets scoped by round:

```
gov_null/{round_id}/{nullifier}
van_null/{round_id}/{nullifier}
share_null/{round_id}/{nullifier}
```

This means a gov nullifier from round A can block a VAN nullifier in round B, or a share nullifier can falsely reject a delegation.

## New Key Format

```
0x01 || type_byte || round_id (32 bytes) || nullifier_bytes
```

Where `type_byte` is:

- `0x00` = Gov nullifier (from `MsgDelegateVote`)
- `0x01` = VAN nullifier (from `MsgCastVote`)
- `0x02` = Share nullifier (from `MsgRevealShare`)

## Changes

### 1. Define `NullifierType` and update key builders in [sdk/x/vote/types/keys.go](sdk/x/vote/types/keys.go)

- Add `NullifierType` byte constants: `NullifierTypeGov = 0x00`, `NullifierTypeVAN = 0x01`, `NullifierTypeShare = 0x02`
- Change `NullifierKey(nullifier)` to `NullifierKey(nfType NullifierType, roundID, nullifier []byte)`
- Key format becomes: `NullifierPrefix || nfType || roundID || nullifier`
- Add `NullifierPrefixForRound(nfType, roundID)` helper for future prefix iteration (genesis export)

### 2. Update keeper nullifier methods in [sdk/x/vote/keeper/keeper.go](sdk/x/vote/keeper/keeper.go)

- `HasNullifier(kvStore, nullifier)` becomes `HasNullifier(kvStore, nfType, roundID, nullifier)`
- `SetNullifier(kvStore, nullifier)` becomes `SetNullifier(kvStore, nfType, roundID, nullifier)`
- `CheckNullifiersUnique(ctx, nullifiers)` becomes `CheckNullifiersUnique(ctx, nfType, roundID, nullifiers)`
- Update KV Store Layout doc comment at top of file

### 3. Update `VoteMessage` interface and implementations in [sdk/x/vote/types/msgs.go](sdk/x/vote/types/msgs.go)

- Add `GetNullifierType() NullifierType` to the `VoteMessage` interface
- `MsgDelegateVote.GetNullifierType()` returns `NullifierTypeGov`
- `MsgCastVote.GetNullifierType()` returns `NullifierTypeVAN`
- `MsgRevealShare.GetNullifierType()` returns `NullifierTypeShare`
- `MsgCreateVotingSession.GetNullifierType()` returns `0` (unused; guarded by `len(nullifiers) > 0` check)

### 4. Update ante handler in [sdk/x/vote/ante/validate.go](sdk/x/vote/ante/validate.go)

- Pass `msg.GetNullifierType()` and `msg.GetVoteRoundId()` to `k.CheckNullifiersUnique()`

### 5. Update message server handlers in [sdk/x/vote/keeper/msg_server.go](sdk/x/vote/keeper/msg_server.go)

- `DelegateVote`: pass `types.NullifierTypeGov` + `msg.VoteRoundId` to `SetNullifier`
- `CastVote`: pass `types.NullifierTypeVAN` + `msg.VoteRoundId` to `SetNullifier`
- `RevealShare`: pass `types.NullifierTypeShare` + `msg.VoteRoundId` to `SetNullifier`

### 6. Update genesis proto and handling

- [sdk/proto/zvote/v1/types.proto](sdk/proto/zvote/v1/types.proto): Add `uint32 nullifier_type` and `bytes round_id` fields to `NullifierEntry`
- Regenerate protobuf Go code
- [sdk/x/vote/keeper/genesis.go](sdk/x/vote/keeper/genesis.go): Update `InitGenesis` and `ExportGenesis` to use typed nullifier entries

### 7. Update all tests

- [sdk/x/vote/keeper/keeper_test.go](sdk/x/vote/keeper/keeper_test.go): All `HasNullifier`/`SetNullifier`/`CheckNullifiersUnique` calls need type + round params. Add cross-round and cross-type isolation tests.
- [sdk/x/vote/keeper/msg_server_test.go](sdk/x/vote/keeper/msg_server_test.go): Assertion calls to `HasNullifier` need type + round.
- [sdk/x/vote/ante/validate_test.go](sdk/x/vote/ante/validate_test.go): `recordNullifier` helper needs type + round. Add test that same nullifier bytes in different types/rounds don't collide.

### 8. Add new regression tests

Add specific tests in `keeper_test.go` for the scoping guarantees:

- Same nullifier bytes stored as gov in round A do NOT block VAN check in round A
- Same nullifier bytes stored as gov in round A do NOT block gov check in round B
- Same nullifier bytes stored as gov in round A DO block gov check in round A (existing behavior, still works)

