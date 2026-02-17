---
name: EA Key Ceremony Keeper
overview: Implement the Election Authority key setup ceremony as a Cosmos SDK keeper layer with new protobuf messages, state management, and integration tests covering Phases A through F of the spec.
todos:
  - id: step1-proto
    content: "Step 1: Protobuf definitions -- add CeremonyState, CeremonyStatus, ValidatorPallasKey, DealerPayload, AckEntry to types.proto; add MsgRegisterPallasKey, MsgDealExecutiveAuthorityKey, MsgAckExecutiveAuthorityKey to tx.proto; remove ea_pk from MsgCreateVotingSession; regenerate Go code"
    status: done
  - id: step2-scaffolding
    content: "Step 2: Go scaffolding -- add CeremonyStateKey to keys.go, sentinel errors to errors.go, event types to events.go, register new msgs (MsgRegisterPallasKey, MsgDealExecutiveAuthorityKey, MsgAckExecutiveAuthorityKey) in codec.go"
    status: done
  - id: step3-keeper-crud
    content: "Step 3: Keeper CRUD -- implement GetCeremonyState/SetCeremonyState and helper functions in keeper_ceremony.go; write unit tests for round-trip storage"
    status: done
  - id: step4-register
    content: "Step 4: MsgRegisterPallasKey handler -- implement handler + tests (valid registration, reject duplicates, reject invalid points, reject wrong ceremony status)"
    status: done
  - id: step5-deal
    content: "Step 5: MsgDealExecutiveAuthorityKey handler -- implement handler + tests (valid deal, reject mismatched payloads, reject invalid ea_pk, reject wrong status, REGISTERING->DEALT transition)"
    status: pending
  - id: step6-ack
    content: "Step 6: MsgAckExecutiveAuthorityKey handler -- implement handler + tests (valid ack, reject duplicates, reject non-validator, all-ack triggers DEALT->CONFIRMED transition)"
    status: pending
  - id: step7-timeout
    content: "Step 7: EndBlocker timeout -- add ceremony timeout logic to EndBlock; test partial-ack timeout->CONFIRMED and zero-ack timeout->ABORTED"
    status: pending
  - id: step8-gate
    content: "Step 8: CreateVotingSession ceremony gate -- update handler to read ceremony ea_pk, reject if not CONFIRMED; update existing tests to seed ceremony state"
    status: pending
  - id: step9-wiring
    content: "Step 9: Module wiring -- add signers, RegisterInterfaces, register Msg RPCs in module.go"
    status: pending
  - id: step10-e2e
    content: "Step 10: Full ceremony integration test -- end-to-end flow with real ECIES crypto: register 3 validators, deal with real encryption, ack, confirm, create voting session"
    status: pending
isProject: false
---

# EA Key Ceremony Keeper Layer

## Key Design Decision

The EA key ceremony is a **one-time chain-level setup**, not per voting round. Once the ceremony completes and `ea_pk` is confirmed in global state, any number of voting sessions can be created using that `ea_pk`. The ceremony must complete before any `MsgCreateVotingSession` can be submitted.

The ceremony lifecycle is tracked by a singleton `CeremonyState` in the KV store, separate from `VoteRound`. The ceremony has its own status enum (`CeremonyStatus`); no new `SessionStatus` is needed.

```
Ceremony:   REGISTERING ──> DEALT ──> CONFIRMED
                                  └──> ABORTED (timeout, 0 acks)
VoteRound:                              ACTIVE ──> TALLYING ──> FINALIZED
                                          ^
                                          │ (gated: requires CONFIRMED ceremony)
```

---

## Step 1: Protobuf Definitions

**Goal:** Define all new types and messages. Regenerate Go code. Everything compiles but nothing is wired up yet.

**Modify [sdk/proto/zvote/v1/types.proto](sdk/proto/zvote/v1/types.proto)** -- add:

```protobuf
enum CeremonyStatus {
  CEREMONY_STATUS_UNSPECIFIED   = 0;
  CEREMONY_STATUS_REGISTERING   = 1; // Accepting validator pk_i registrations
  CEREMONY_STATUS_DEALT         = 2; // DealerTx landed, awaiting acks
  CEREMONY_STATUS_CONFIRMED     = 3; // Sufficient acks received, ea_pk ready
  CEREMONY_STATUS_ABORTED       = 4; // Timeout with zero acks
}

message CeremonyState {
  CeremonyStatus              status      = 1;
  bytes                       ea_pk       = 2;  // Set when DealerTx lands
  repeated ValidatorPallasKey validators  = 3;  // All registered pk_i
  repeated DealerPayload      payloads    = 4;  // ECIES envelopes from DealerTx
  repeated AckEntry           acks        = 5;  // Per-validator ack status
  string                      dealer      = 6;  // Validator address of the dealer
  uint64                      deal_height = 7;  // Block height when DealerTx landed
  uint64                      ack_timeout = 8;  // Timeout in seconds after deal_height
}

message ValidatorPallasKey {
  string validator_address = 1;
  bytes  pallas_pk         = 2; // Compressed Pallas point (32 bytes)
}

message DealerPayload {
  string validator_address = 1;
  bytes  ephemeral_pk      = 2; // E_i (32 bytes)
  bytes  ciphertext        = 3; // ct_i: ChaCha20-Poly1305 (32 + 16 = 48 bytes)
}

message AckEntry {
  string validator_address = 1;
  bytes  ack_signature     = 2; // Signature over H("ack" || ea_pk || validator_address)
  uint64 ack_height        = 3;
}
```

**Modify [sdk/proto/zvote/v1/tx.proto](sdk/proto/zvote/v1/tx.proto):**
- Add 3 new RPCs to the `Msg` service: `RegisterPallasKey`, `DealExecutiveAuthorityKey`, `AckExecutiveAuthorityKey`
- Add request/response message types for each
- Remove `ea_pk` field (field 8) from `MsgCreateVotingSession` (mark as `reserved 8;`)

**Regenerate:** `types.pb.go`, `tx.pb.go`, `tx_grpc.pb.go`

**Verify:** `go build ./...` compiles (existing code that references `msg.EaPk` in `CreateVotingSession` will break -- fix the reference to read from ceremony state instead, but leave the handler logic for Step 8).

---

## Step 2: Go Scaffolding

**Goal:** Add the supporting constants, errors, and event types so later steps can reference them.

**Modify [sdk/x/vote/types/keys.go](sdk/x/vote/types/keys.go):**
- Add `CeremonyStateKey = []byte{0x09}` (singleton key, like `TreeStateKey`)

**Modify [sdk/x/vote/types/errors.go](sdk/x/vote/types/errors.go):**
- `ErrCeremonyNotReady` -- ceremony not in CONFIRMED status
- `ErrCeremonyWrongStatus` -- operation invalid for current ceremony status
- `ErrDuplicateRegistration` -- validator already registered pk_i
- `ErrInvalidPallasPoint` -- point is not on curve, is identity, or wrong size
- `ErrPayloadMismatch` -- dealer payload count does not match validator count
- `ErrDuplicateAck` -- validator already acked
- `ErrNotRegisteredValidator` -- validator not in ceremony's validator list

**Modify [sdk/x/vote/types/events.go](sdk/x/vote/types/events.go):**
- `EventTypeRegisterPallasKey`, `EventTypeDealExecutiveAuthorityKey`, `EventTypeAckExecutiveAuthorityKey`, `EventTypeCeremonyStatusChange`
- `AttributeKeyValidatorAddress`, `AttributeKeyCeremonyStatus`, `AttributeKeyEAPK`

**Modify [sdk/x/vote/types/codec.go](sdk/x/vote/types/codec.go):**
- Add `&MsgRegisterPallasKey{}`, `&MsgDealExecutiveAuthorityKey{}`, `&MsgAckExecutiveAuthorityKey{}` to `RegisterImplementations`

---

## Step 3: Keeper CRUD + Unit Tests

**Goal:** Ceremony state can be stored and retrieved. Pure data layer, no business logic.

**Create `sdk/x/vote/keeper/keeper_ceremony.go`:**

```go
func (k Keeper) GetCeremonyState(kvStore) (*types.CeremonyState, error)
func (k Keeper) SetCeremonyState(kvStore, *types.CeremonyState) error
func FindValidatorInCeremony(state, valAddr) (index int, found bool)
func FindAckForValidator(state, valAddr) (index int, found bool)
func AllValidatorsAcked(state) bool
```

**Create `sdk/x/vote/keeper/keeper_ceremony_test.go`:**
- Test `GetCeremonyState` returns nil when no ceremony exists
- Test `SetCeremonyState` / `GetCeremonyState` round-trip
- Test `FindValidatorInCeremony` found/not-found
- Test `AllValidatorsAcked` with full acks, partial acks, no acks

---

## Step 4: MsgRegisterPallasKey + Tests

**Goal:** Validators can register their Pallas public keys. Ceremony transitions from empty to REGISTERING on first registration.

**Create `sdk/x/vote/keeper/msg_server_ceremony.go`** with `RegisterPallasKey` handler:
- If no ceremony exists, create one in `REGISTERING` status
- Validate ceremony is in `REGISTERING` status
- Validate `pallas_pk` is 32 bytes, decompresses to a valid Pallas point, not identity
- Reject duplicate `validator_address`
- Append to `ceremony.validators`, save
- Emit event

**Tests in `keeper_ceremony_test.go`:**
- Happy path: register 3 validators sequentially
- Reject duplicate registration
- Reject invalid point (wrong size, identity, off-curve)
- Reject registration when ceremony is in DEALT status

---

## Step 5: MsgDealExecutiveAuthorityKey + Tests

**Goal:** Bootstrap validator distributes encrypted `ea_sk` to all registered validators. Ceremony transitions REGISTERING -> DEALT.

**Add `DealExecutiveAuthorityKey` handler to `msg_server_ceremony.go`:**
- Validate ceremony is in `REGISTERING` status
- Validate at least 1 validator is registered
- Validate `ea_pk` is a valid Pallas point
- Validate `len(payloads) == len(ceremony.validators)`
- Validate each payload's `validator_address` matches exactly one registered validator (1:1 mapping)
- Validate each `ephemeral_pk` is a valid Pallas point
- Store payloads, `ea_pk`, `dealer`, `deal_height` in ceremony state
- Transition to `DEALT`
- Emit event

**Tests:**
- Happy path: 3 validators registered, deal with 3 payloads -> DEALT
- Reject: payload count mismatch
- Reject: payload references unknown validator
- Reject: duplicate validator in payloads
- Reject: invalid `ea_pk`
- Reject: ceremony not in REGISTERING status

---

## Step 6: MsgAckExecutiveAuthorityKey + Tests

**Goal:** Validators acknowledge receipt of `ea_sk`. When all ack, ceremony transitions DEALT -> CONFIRMED.

**Add `AckExecutiveAuthorityKey` handler to `msg_server_ceremony.go`:**
- Validate ceremony is in `DEALT` status
- Validate creator is in `ceremony.validators`
- Reject if creator already has an entry in `ceremony.acks`
- Record `AckEntry` with `ack_height = block_height`
- If `AllValidatorsAcked(state)`, transition to `CONFIRMED`
- Emit event (include whether this ack triggered confirmation)

**Tests:**
- Happy path: 3 validators, all ack sequentially -> last ack triggers CONFIRMED
- Reject duplicate ack
- Reject ack from non-registered validator
- Reject ack when ceremony is not DEALT
- Partial acks: 2 of 3 ack -> still DEALT

---

## Step 7: EndBlocker Timeout + Tests

**Goal:** If ack timeout expires, ceremony auto-transitions based on ack count.

**Modify [sdk/x/vote/module.go](sdk/x/vote/module.go) `EndBlock`:**
- Read `CeremonyState`; if nil or status != `DEALT`, skip
- Compute timeout: `deal_block_time + ack_timeout`
- If `block_time >= timeout`:
  - If `len(acks) >= 1`: transition to `CONFIRMED`
  - If `len(acks) == 0`: transition to `ABORTED`
  - Emit `EventTypeCeremonyStatusChange`

**Note:** `deal_height` stores the block height, but we need the block time at that height for timeout comparison. Two options: (a) store `deal_time` instead of `deal_height`, or (b) store both. We'll store `deal_time` (uint64 unix seconds) since that's what we compare against. Rename the proto field accordingly.

**Tests (in `keeper_ceremony_test.go`):**
- 3 validators, deal at time T, 1 ack, advance block_time past T+timeout -> CONFIRMED
- 3 validators, deal at time T, 0 acks, advance block_time past T+timeout -> ABORTED
- No timeout yet: deal at time T, advance to T+timeout-1 -> still DEALT

---

## Step 8: CreateVotingSession Ceremony Gate + Tests

**Goal:** `MsgCreateVotingSession` no longer accepts `ea_pk` directly; it reads it from the confirmed ceremony.

**Modify [sdk/x/vote/keeper/msg_server.go](sdk/x/vote/keeper/msg_server.go) `CreateVotingSession`:**
- Read `CeremonyState` from store
- If nil or status != `CONFIRMED`, return `ErrCeremonyNotReady`
- Set `round.EaPk = ceremony.EaPk` (instead of `msg.EaPk`)
- Remove reference to `msg.EaPk`

**`VoteRound.ea_pk` (field 10) stays** -- it records which EA key was active when the round was created (important for future key rotation).

**Update [sdk/x/vote/keeper/msg_server_test.go](sdk/x/vote/keeper/msg_server_test.go):**
- Add a helper `seedConfirmedCeremony(s)` that writes a `CeremonyState{Status: CONFIRMED, EaPk: ...}` to the store
- Call it in `SetupTest` or at the start of each `CreateVotingSession` test
- Update `validSetupMsg()` to remove the `EaPk` field
- Add test: reject `CreateVotingSession` when no ceremony exists
- Add test: reject when ceremony is DEALT (not yet CONFIRMED)
- Add test: verify `round.EaPk` matches ceremony's `ea_pk`

---

## Step 9: Module Wiring

**Goal:** New messages are routable by the Cosmos SDK runtime.

**Modify [sdk/x/vote/module.go](sdk/x/vote/module.go):**
- Add `ProvideRegisterPallasKeySigner`, `ProvideDealExecutiveAuthorityKeySigner`, `ProvideAckExecutiveAuthorityKeySigner` (no-op signers, same pattern as existing)
- Register them in `init()` via `appmodule.Provide`
- In `RegisterInterfaces`, the new msgs are already added in Step 2 via `codec.go`
- In `RegisterServices`, the new RPCs are already in the generated `tx_grpc.pb.go` -- just ensure the `msgServer` implements them (it does, from Steps 4-6)

---

## Step 10: Full Ceremony Integration Test with Real ECIES

**Goal:** End-to-end test proving the entire ceremony works with real cryptography.

**Add to `keeper_ceremony_test.go`:**

```
TestFullCeremonyWithECIES:
  1. Generate 3 Pallas keypairs (sk_i, pk_i) using elgamal.KeyGen
  2. Register all 3 pk_i via MsgRegisterPallasKey
  3. Generate ea_sk, ea_pk using elgamal.KeyGen
  4. For each validator, encrypt ea_sk to pk_i using ecies.Encrypt
  5. Submit MsgDealExecutiveAuthorityKey with ea_pk and all 3 ECIES envelopes
  6. For each validator:
     a. Grab their (E_i, ct_i) from ceremony state
     b. Decrypt using ecies.Decrypt(sk_i, envelope)
     c. Verify decrypted bytes == ea_sk bytes
     d. Verify ea_sk * G == ea_pk
     e. Submit MsgAckExecutiveAuthorityKey
  7. Verify ceremony is CONFIRMED
  8. Create a voting session, verify round.EaPk == ea_pk
```

This test uses real `crypto/ecies` and `crypto/elgamal` -- no mocks.

---

## File Summary

**Modify:**
- [sdk/proto/zvote/v1/types.proto](sdk/proto/zvote/v1/types.proto) (Step 1)
- [sdk/proto/zvote/v1/tx.proto](sdk/proto/zvote/v1/tx.proto) (Step 1)
- [sdk/x/vote/types/keys.go](sdk/x/vote/types/keys.go) (Step 2)
- [sdk/x/vote/types/errors.go](sdk/x/vote/types/errors.go) (Step 2)
- [sdk/x/vote/types/events.go](sdk/x/vote/types/events.go) (Step 2)
- [sdk/x/vote/types/codec.go](sdk/x/vote/types/codec.go) (Step 2)
- [sdk/x/vote/keeper/msg_server.go](sdk/x/vote/keeper/msg_server.go) (Step 8)
- [sdk/x/vote/module.go](sdk/x/vote/module.go) (Steps 7, 9)
- [sdk/x/vote/keeper/msg_server_test.go](sdk/x/vote/keeper/msg_server_test.go) (Step 8)

**Create:**
- `sdk/x/vote/keeper/keeper_ceremony.go` (Step 3)
- `sdk/x/vote/keeper/msg_server_ceremony.go` (Steps 4-6)
- `sdk/x/vote/keeper/keeper_ceremony_test.go` (Steps 3-10)

**Regenerate:**
- `sdk/x/vote/types/types.pb.go` (Step 1)
- `sdk/x/vote/types/tx.pb.go` (Step 1)
- `sdk/x/vote/types/tx_grpc.pb.go` (Step 1)
