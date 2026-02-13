# Witness Generation for Voting Proposal Verification

This document explains how Zashi generates Merkle inclusion proofs (witnesses) for Orchard notes at historical block heights, which is a core primitive for the Zcash voting proposal verification system.

## Overview

For voting, users must prove they held shielded ZEC at a specific "snapshot height" without moving funds on-chain. This requires generating a Merkle witness proving their note commitment existed in the Orchard commitment tree at that height.

**Key insight:** Witness generation always fetches the tree frontier from lightwalletd to ensure correct root computation. Local wallet data alone is unreliable because wallets only download shards containing their own notes.

## Architecture

### The Orchard Commitment Tree

Orchard uses a depth-32 Merkle tree to store note commitments. When a shielded transaction is mined, its note commitments are appended to this tree. The tree is append-only and grows over time:

```
Block 2,400,000: Tree has 1,000,000 leaves  →  Root = 0xabc...
Block 2,400,100: Tree has 1,000,500 leaves  →  Root = 0xdef...
Block 2,400,200: Tree has 1,001,000 leaves  →  Root = 0x123...
```

A note at position P exists in all tree states after it was added, but the **witness (sibling hashes) differs** depending on how many subsequent leaves exist.

### How Notes and Their Positions Are Obtained

A common question: how does the wallet know which notes it owns and where they sit in the commitment tree?

**During normal wallet sync**, the wallet scans each block's shielded outputs using the user's viewing key. When a note is successfully decrypted (meaning it belongs to this wallet), the wallet records it in the local database along with its **commitment tree position**—the leaf index where this note's commitment was appended to the global Orchard tree.

This position is a permanent, immutable property of the note. It's determined by when the note's transaction was mined and never changes, regardless of what happens to the tree afterward.

**Listing notes for voting** uses `listOrchardNotes()`, which queries the local wallet database:

```sql
SELECT rn.id, rn.commitment_tree_position, rn.value, COALESCE(t.mined_height, 0) AS mined_height
FROM orchard_received_notes rn
JOIN transactions t ON rn.transaction_id = t.id_tx
WHERE rn.commitment_tree_position IS NOT NULL
ORDER BY rn.id
```

This returns every Orchard note the wallet owns with its position, value, and the height at which it was mined. The caller then filters to notes where `mined_height ≤ snapshot_height` to find notes eligible for voting at that snapshot.

**Key point:** Note discovery is entirely local—it uses data the wallet already has from normal sync. No network requests are needed to enumerate notes or their positions. The only network request in the entire witness generation flow is fetching the tree frontier (see below).

### What's Stored Locally

Zashi's wallet database (via `librustzcash`) stores:

| Table                      | Contents                                                | Pruned?             |
| -------------------------- | ------------------------------------------------------- | ------------------- |
| `orchard_tree_shards`      | Tree nodes for shards containing wallet's notes         | No                  |
| `orchard_tree_cap`         | Upper tree levels                                       | No                  |
| `orchard_tree_checkpoints` | (height, tree_position) pairs                           | Yes (last 100 only) |
| `blocks`                   | Block metadata including `orchard_commitment_tree_size` | No                  |

Shard data is permanent but incomplete—wallets only download shards containing their own notes. Checkpoints are pruned to the last 100 blocks for reorg handling.

### The Checkpoint Pruning Problem

ShardTree (the tree implementation) generates witnesses via:

```rust
tree.witness_at_checkpoint_id(note_position, &snapshot_height)
```

This requires a checkpoint at `snapshot_height` to know the tree size at that height. But checkpoints older than 100 blocks are pruned.

**Problem:** Voting snapshots may be weeks in the past (thousands of blocks), so the checkpoint won't exist.

### The Solution: Checkpoint Reconstruction via Frontier

The current implementation sidesteps local checkpoint data entirely. When inserting the frontier from lightwalletd (see below), we also create a checkpoint entry at the frontier position:

```rust
// Frontier insertion also creates the checkpoint:
if let Some(nonempty_frontier) = orchard_frontier.clone().take() {
    let frontier_position = nonempty_frontier.position();
    tree.insert_frontier_nodes(nonempty_frontier, Retention::Checkpoint { ... })?;

    // insert_frontier_nodes marks nodes but doesn't create the checkpoint entry
    let checkpoint = Checkpoint::at_position(frontier_position);
    tree.store_mut().add_checkpoint(checkpoint_height, checkpoint)?;
}

let witness = tree.witness_at_checkpoint_id(position, &checkpoint_height)?;
```

The frontier position _is_ the tree size at that height, so the checkpoint is reconstructed directly from the lightwalletd response rather than from local `blocks` table data.

### The Incomplete Shard Problem

Even after checkpoint reconstruction, witness generation may fail with `TreeIncomplete`:

```
TreeIncomplete([Address { level: Level(4), index: 3105875 }])
```

**Root cause:** Wallets only download tree shards containing their own notes. To compute a witness, we need sibling hashes that may be in shards the wallet never downloaded.

### The Solution: Always Use Frontier from Lightwalletd

The `GetTreeState(height)` RPC returns the tree frontier at any height. The frontier contains ommer (sibling) hashes along the path from the frontier position to the root—all the data needed to compute a correct witness for any note.

**Why not use local data?** We initially tried a "local first, fallback to frontier" approach, but discovered that local witness generation can "succeed" with incorrect data. The shardtree has partial shard data and can compute a structurally valid witness with wrong sibling hashes, producing an incorrect root. There's no error—just wrong results. This is especially problematic for notes at the tree frontier.

**Current approach:**

```
1. Fetch GetTreeState(snapshot_height) from lightwalletd
2. Extract frontier and compute authoritative root directly from it
3. Insert frontier into ShardTree via insert_frontier_nodes()
4. Add checkpoint (insert_frontier_nodes marks nodes but doesn't create checkpoint entry)
5. Generate witness using the local tree structure
6. Return witness with the frontier's authoritative root (not shardtree's computed root)
```

**Critical implementation detail:** The witness root must come from `orchard_commitment_tree.root()` (computed directly from the frontier), NOT from `tree.root_at_checkpoint_id()` (which uses mixed local data). The auth path comes from the shardtree, but the root comes from the frontier.

**Privacy note:** Fetching `GetTreeState(snapshot_height)` reveals nothing sensitive—the snapshot height is public and all voters make the same request.

## Implementation

### Repositories Modified

All changes are on the `greg/voting-proposal-verification` branch:

| Repository               | Changes                                               |
| ------------------------ | ----------------------------------------------------- |
| `zcash-light-client-ffi` | FFI functions for witness generation and note listing |
| `zcash-swift-wallet-sdk` | Swift bindings exposing FFI to iOS                    |
| `zashi-ios`              | Demo UI in Advanced Settings                          |

### FFI Functions

**`zcashlc_get_orchard_witness_with_frontier`**

Generates a Merkle witness for a note using frontier data from lightwalletd:

```rust
pub unsafe extern "C" fn zcashlc_get_orchard_witness_with_frontier(
    db_data: *const u8,
    db_data_len: usize,
    network_id: u32,
    note_position: u64,
    checkpoint_height: u32,
    tree_state: *const u8,      // Protobuf-encoded TreeState
    tree_state_len: usize,
) -> *mut ffi::BoxedSlice
```

Returns serialized witness (1100 bytes):

- Bytes 0-31: note commitment (32 bytes) - the leaf value for verification
- Bytes 32-39: position (u64 LE)
- Bytes 40-71: root hash at checkpoint height (32 bytes)
- Bytes 72-75: path length (u32 LE, always 32)
- Bytes 76-1099: auth path (32 sibling hashes × 32 bytes)

The witness is self-contained: verification can recompute the root by hashing the note commitment up through the auth path.

**`zcashlc_verify_orchard_witness`**

Verifies a witness by recomputing the Merkle root from the note commitment and auth path. This simulates exactly what the ZKP circuit will do.

```rust
pub unsafe extern "C" fn zcashlc_verify_orchard_witness(
    witness_data: *const u8,
    witness_len: usize,
) -> i32  // 1 = valid, 0 = invalid, -1 = error
```

**`zcashlc_list_orchard_notes`**

Lists wallet's Orchard notes with their tree positions:

```rust
pub unsafe extern "C" fn zcashlc_list_orchard_notes(
    db_data: *const u8,
    db_data_len: usize,
    network_id: u32,
) -> *mut ffi::BoxedSlice
```

Returns array of (note_id, position, value_zats, mined_height) for each note.

### Swift SDK

The SDK exposes these through the `Synchronizer` protocol:

```swift
func getOrchardWitnessAtHeight(
    notePosition: UInt64,
    checkpointHeight: BlockHeight
) async throws -> Data

func verifyOrchardWitness(witnessData: Data) async throws -> Bool

func listOrchardNotes() async throws -> [OrchardNoteInfo]
```

The `getOrchardWitnessAtHeight` function always fetches the frontier from lightwalletd:

1. Fetches `GetTreeState(checkpointHeight)` from lightwalletd
2. Calls `zcashlc_get_orchard_witness_with_frontier` with the serialized TreeState
3. Returns the witness with the correct root and note commitment

The `verifyOrchardWitness` function recomputes the Merkle root from the note commitment and auth path, verifying the witness is valid. This simulates what the ZKP circuit will do.

### Demo UI

Located in `zashi-ios/modules/Sources/Features/WitnessDemo/`:

1. Lists wallet's Orchard notes with values and positions
2. User selects a note and enters snapshot height
3. Generates witness and verifies it by recomputing the Merkle root
4. Displays note commitment, tree root, and verification status

## Voting Flow

For the voting system, this enables:

1. **Snapshot announcement:** A vote specifies snapshot height H
2. **Note discovery:** Wallet lists all Orchard notes from local DB via `listOrchardNotes()`, filters to notes where `mined_height ≤ H`
3. **Witness generation:** For each eligible note, wallet fetches `GetTreeState(H)` from lightwalletd, inserts the frontier, and generates a witness proving the note exists in the tree at height H
4. **Verification:** Root hash from witness matches published tree root at H
5. **Vote submission:** Witness included in vote proof (separate from this implementation)

**Privacy:** The only network request (`GetTreeState(H)`) reveals nothing—snapshot height H is public and all voters make the same request.

## Key Constraints

1. **Wallet must have synced the snapshot height** - The `blocks` table must contain the snapshot height's tree size
2. **Note must exist at snapshot height** - The note's mined height must be ≤ snapshot height
3. **Network access required** - Always fetches frontier from lightwalletd (privacy-preserving: all voters make same request)
4. **Orchard only** - Sapling witnesses not implemented (voting uses Orchard)

## Commits

### zcash-light-client-ffi

- `e03c5b4` - Remove unused local witness generation function
- `0063ef3` - Fix witness root computation to use frontier directly
- `a38d9f9` - Add FFI function to extract Orchard tree root from TreeState
- `72be633` - Fix witness generation with frontier by explicitly adding checkpoint

### zcash-swift-wallet-sdk

- `4ddbecd9` - Remove unused local witness generation backend method
- `59286128` - Always use frontier for witness generation
- `6fb2d0b0` - Add getOrchardTreeRoot for witness verification

### zashi-ios

- `1159791` - Add WitnessDemo feature for voting proposal verification
