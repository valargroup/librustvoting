# POC: Incremental Tree Server + Sharded Tree Client Sync

Design for a proof-of-concept where:
1. **Server** maintains the vote commitment tree with continuous incremental updates (mirroring chain state).
2. **Client** syncs to those updates using a sharded tree and can generate Merkle witnesses.

This document recommends a clear separation of **server**, **client**, and **communication** abstractions, and a test that proves the interaction. The final goal is to wire client APIs into Zashi (librustzcash) and server APIs on the Cosmos SDK chain (sdk).

---

## Design context (loaded)

- **vote-commitment-tree README & BRIDGE_TREE**: Vote tree is Poseidon-based, depth 32, same ShardTree/incrementalmerkletree stack as Orchard. Sync flow: wallet/server receive leaves (VAN/VC) per block, append locally, checkpoint by height, then use `path(position, anchor_height)` for ZKP #2 / ZKP #3. Server today exposes `CommitmentTreeAtHeight` and `LatestCommitmentTree` (root + next_index + height) but **not** leaves or a compact-block/frontier format.
- **Gov Steps V1 (Obsidian)** and **Figma Wallet SDK → Cosmos** were not loaded (MCP timeouts); the README is the source of truth for tree role and sync options.
- **librustzcash**: ShardTree usage in `zcash_client_sqlite` (commitment_tree.rs, wallet init migrations), `zcash_client_backend` (shardtree serialization), `zcash_client_memory` (MemoryShardStore, frontiers). Witness generation uses frontiers from lightwalletd and checkpoint reconstruction.
- **sdk**: Vote keeper stores leaves at `0x02 || index`, roots at `0x03 || height`, state at `0x06`. `ComputeTreeRoot` is currently Blake2b placeholder; append is per-leaf. HTTP: `GET /zally/v1/commitment-tree/{height}` and `.../latest` return `CommitmentTreeState` (next_index, root, height) only.

---

## Recommended layer split

Keep three clearly separated layers so that server and client can evolve independently and the wire format is the only contract.

```
┌─────────────────────────────────────────────────────────────────┐
│  SERVER (incremental tree authority)                             │
│  - Owns full tree or incremental representation                  │
│  - Applies updates (leaves per block / checkpoint per height)     │
│  - Exposes: tree state, leaves/frontier for sync                 │
└───────────────────────────┬─────────────────────────────────────┘
                             │
                             │  Sync protocol (leaves / compact block / frontier)
                             │
┌───────────────────────────▼─────────────────────────────────────┐
│  CLIENT (sharded tree)                                           │
│  - Receives updates via sync API                                 │
│  - Maintains ShardTree (sparse: only what’s needed for witnesses) │
│  - Exposes: checkpoint root, path(position, anchor_height)       │
└─────────────────────────────────────────────────────────────────┘
```

- **Server abstraction**: “Incremental tree server” — appends leaves, checkpoints by block height, can answer: current state (root, next_index, height), and **either** raw leaves in range **or** a compact update (e.g. leaves for a height range) / frontier for a given height.
- **Client abstraction**: “Shard tree client” — consumes server responses, applies them to a local `VoteCommitmentTree` (or generic ShardTree over `MerkleHashVote`), maintains checkpoints, answers `root_at_height` and `path(position, anchor_height)`.
- **Communication**: Explicit **sync protocol** — request/response types and (optionally) transport. No client logic in server code and no server logic in client code beyond “parse this message and call tree APIs”.

---

## 1. Server abstraction (incremental tree server)

**Responsibility**: Maintain the canonical tree state; apply appends and checkpoints as the chain does (or as a mirror of chain state). Expose only what clients need to sync.

**Suggested interface (conceptual)**:

- **State**: `(root, next_index, height)` — already in sdk as `CommitmentTreeState`.
- **Leaves in range**: `get_leaves(from_index, to_index) -> [leaf_bytes]` — so the client can replay appends.
- **Optional (fast-sync)**: `get_frontier(height) -> Frontier` or subtree roots at a given height, to bootstrap without replaying all leaves.

**POC scope (server side)**:

- Implement an **in-process** “incremental tree server” that:
  - Holds a single `VoteCommitmentTree` (or equivalent).
  - Exposes:
    - `state() -> (root, next_index, height)` (or reuse `CommitmentTreeState`).
    - `append(leaf)` / `append_two(a, b)` and `checkpoint(height)` so its state mirrors “blocks”.
    - `leaves(from_index, to_index) -> Vec<[u8;32]>` — so the client can pull leaves and apply them.
- No HTTP/gRPC in the POC; the “server” is just a Rust struct that the test drives. This keeps the **server abstraction** clear: “thing that owns the tree and answers state + leaves.”

**Later (Cosmos SDK)**:

- Chain already stores leaves in KV and has EndBlocker root snapshots. To act as “incremental tree server” you can:
  - Add a query (or HTTP) that returns leaves in a range: e.g. `GetCommitmentLeaves(from_index, to_index)`.
  - Optionally add a “frontier at height” or “subtree roots at height” for fast-sync (like lightwalletd’s subtree roots).
- Root computation should be switched from Blake2b to the same Poseidon tree as `vote-commitment-tree` (FFI or reimplementation; README recommends FFI).

---

## 2. Client abstraction (shard tree client)

**Responsibility**: Hold a local ShardTree (sparse); ingest updates from the server; answer witness and root queries.

**Suggested interface**:

- **Ingest**: `apply_leaves(leaves: &[[u8;32]])` and `checkpoint(height)` so the client tree stays in sync with server.
- **Queries**: `root_at_height(height) -> Option<[u8;32]>`, `path(position, anchor_height) -> Option<MerklePath>` (or equivalent).
- **Optional**: `put_frontier(height, frontier)` for fast-sync (like Zashi/Orchard).

**POC scope (client side)**:

- A **sync client** type that:
  - Owns a `VoteCommitmentTree` (or a ShardTree with `MerkleHashVote` and a persistent or in-memory store).
  - Has `apply_leaves(leaves)` and `checkpoint(height)`.
  - Exposes `root_at_height`, `path(position, anchor_height)`.
  - Does **not** know about HTTP or protobuf; it only consumes “list of leaves” and “checkpoint at height”.

So: **client API = tree API + apply_leaves + checkpoint**. No “fetch from network” inside this abstraction; that belongs to the **communication** layer.

**Later (Zashi / librustzcash)**:

- This client becomes the vote-chain tree counterpart of the Orchard commitment tree in the wallet: same pattern (apply updates, checkpoint, witness_at_checkpoint_id). It can live in a new crate or a submodule that librustzcash (or Zashi’s Rust layer) depends on, with FFI for Swift.

---

## 3. Communication abstraction (sync protocol)

**Responsibility**: Define the **wire format** and **semantics** of “how the client gets updates from the server.” Server and client only depend on this contract, not on each other’s internals.

**Suggested minimal protocol for POC**:

- **Fetch state**: Returns `(root, next_index, height)` — same as `CommitmentTreeState`.
- **Fetch leaves**: Given `(from_index, to_index)`, returns a list of 32-byte leaves in order. Client can then call `apply_leaves` and then `checkpoint(height)` when it knows the height that corresponds to `to_index`.

**Concrete types (example)**:

```text
# Request/response (could be Rust structs, JSON, or protobuf for sdk)

GetState    -> { root, next_index, height }
GetLeaves   -> request: { from_index, to_index }; response: { leaves: [[u8;32]] }
```

**POC scope (communication)**:

- Define these as **Rust types** (and optionally a tiny “channel” that passes in-memory messages). No real network. The test will:
  - Drive the “server” (append + checkpoint),
  - Use the “protocol” to get state and leaves,
  - Feed them into the “client,”
  - Assert `client.root_at_height(h)` matches `server.state().root` and that `client.path(pos, h)` verifies.
- This proves that **server → protocol → client** preserves tree semantics.

**Later (Cosmos SDK)**:

- Implement the same contract over gRPC/HTTP: e.g. `CommitmentTreeAtHeight` / `LatestCommitmentTree` already give state; add `GetCommitmentLeaves(from_index, to_index)` (and optionally frontier/subtree endpoints). The client in Zashi then uses HTTP/gRPC to get state and leaves and calls into the same “shard tree client” API.

---

## 4. Test to prove the interaction

**Goal**: One test that runs the full path: server updates → protocol → client sync → witness and root consistency.

**Steps**:

1. **Server**: Create empty incremental tree server; append leaves (e.g. simulate MsgDelegateVote + MsgCastVote: one leaf, then two leaves); call `checkpoint(height)` after each “block.”
2. **Protocol**: From server, get state and get leaves (e.g. 0..next_index) in one or more chunks.
3. **Client**: Create empty shard tree client; for each chunk, call `apply_leaves(chunk)`; then call `checkpoint(height)` for the height that corresponds to the last applied index (using state from server).
4. **Assert**:
   - For each checkpoint height `h`: `client.root_at_height(h) == server.root_at_height(h)` (or equivalent).
   - For a chosen leaf index and anchor height: `path = client.path(index, anchor_height)`; `path.verify(leaf, root)` and `root == server.root_at_height(anchor_height)`.
5. **Optional**: Re-run with “fetch leaves in chunks” (e.g. by block) to simulate incremental sync.

This test should live in the repo that contains both server and client abstractions (e.g. `vote-commitment-tree` or a small `vote-tree-sync` crate). It does **not** need a real chain or network.

---

## 5. Suggested crate/layout for POC

To keep boundaries clear:

- **vote-commitment-tree** (existing): Core tree types (`MerkleHashVote`, `VoteCommitmentTree`, `MerklePath`, `Anchor`). No “server” or “client” or network — just the tree.
- **vote-tree-sync** (new, optional):  
  - **server**: `IncrementalTreeServer` — wraps or uses `VoteCommitmentTree`, exposes `state()`, `leaves(from, to)`, and mutators `append`/`checkpoint`.  
  - **client**: `ShardTreeSyncClient` — owns `VoteCommitmentTree`, has `apply_leaves`, `checkpoint`, `root_at_height`, `path`.  
  - **protocol**: `GetState`, `GetLeaves` (and response types) as plain structs; optional in-memory “channel” for the test.  
  - **integration test**: As in §4, server → protocol → client → assert roots and path verification.

Alternatively, the POC can live inside `vote-commitment-tree` under a `sync` or `poc` module and a feature flag, so everything stays in one crate until you split for librustzcash/sdk.

---

## 6. Final wiring (summary)

- **Client APIs in Zashi (librustzcash)**  
  - Use the **shard tree client** abstraction: apply leaves from the vote chain, checkpoint by height, then `path(position, anchor_height)` for ZKP #2 (and similarly for helper server for ZKP #3).  
  - Transport: in Zashi, call existing (or new) chain APIs (HTTP/gRPC) to get `CommitmentTreeState` and `GetCommitmentLeaves(from, to)`; map responses into `apply_leaves` + `checkpoint`.  
  - Same pattern as Orchard + lightwalletd (compact blocks / frontier), but with “leaves in range” and no trial decryption.

- **Server APIs on Cosmos SDK chain**  
  - Keep `CommitmentTreeAtHeight` and `LatestCommitmentTree`.  
  - Add **GetCommitmentLeaves(from_index, to_index)** (and optionally frontier/subtree at height) so the client can sync without parsing full blocks.  
  - Replace `ComputeTreeRoot` with the same Poseidon tree as `vote-commitment-tree` (FFI recommended in README).

---

## 7. Recommendations

1. **Implement the POC in Rust only first** (in-process server and client, no HTTP). Proves that the sync protocol and tree semantics are correct; then add Go (sdk) and Swift (Zashi) on top of the same contract.
2. **Define the sync protocol as shared types** (e.g. `GetLeaves { from_index, to_index }` and `Leaves { leaves }`) and use them in both the Rust test and, later, in the sdk query handlers and the client’s network layer.
3. **Add `GetCommitmentLeaves` (and optionally frontier) on the chain** so the wallet does not have to parse full Cosmos blocks to get leaves; this matches the “custom compact block” option in the README.
4. **Keep root computation in one place**: use `vote-commitment-tree` (via FFI) in the chain’s EndBlocker so roots and Merkle paths are identical on chain and in the client.
5. **Reuse the same checkpoint/witness pattern as Orchard** in librustzcash: the vote-tree client is “another ShardTree” with `apply_leaves` + `checkpoint` + `witness_at_checkpoint_id`; only the hash and leaf type differ.
6. **Test the full path once**: server append/checkpoint → get state + leaves via protocol → client apply + checkpoint → assert roots and path verification. That single test de-risks the interaction before wiring into sdk and Zashi.

If you want, next step can be: (a) add a minimal `sync` or `poc` module inside `vote-commitment-tree` with the server/client/protocol types and the one integration test, or (b) sketch the exact `GetCommitmentLeaves` proto and keeper method for the sdk.

---

## 8. Where things live (repo map)

| Layer | POC (Rust) | Final wiring |
|-------|------------|--------------|
| **Server** | `IncrementalTreeServer` in vote-commitment-tree or vote-tree-sync | **sdk**: `x/vote/keeper` (append + EndBlocker root), new query `GetCommitmentLeaves`; HTTP in `api/query_handler.go` |
| **Client** | `ShardTreeSyncClient` in same crate | **librustzcash**: New vote-tree sync client (like Orchard in `zcash_client_sqlite/.../commitment_tree.rs`); Zashi calls via FFI |
| **Protocol** | Rust structs GetState, GetLeaves | **sdk**: Proto `zvote.v1.Query`; add GetCommitmentLeavesRequest/Response in `proto/zvote/v1/query.proto` and keeper |
| **Tree core** | vote-commitment-tree | Shared: chain uses same tree via FFI for ComputeTreeRoot; client uses same crate for ShardTree |

---

## 8. Where things live (repo map)

| Layer | POC (Rust) | Final wiring |
|-------|------------|--------------|
| **Server** | `IncrementalTreeServer` in `vote-commitment-tree` or `vote-tree-sync` | **sdk**: `x/vote/keeper` (append + EndBlocker root), new query `GetCommitmentLeaves`; HTTP in `api/query_handler.go` |
| **Client** | `ShardTreeSyncClient` in same crate | **librustzcash**: New vote-tree sync client (like Orchard commitment tree in `zcash_client_sqlite/src/wallet/commitment_tree.rs`, `wallet.rs`); Zashi calls via FFI |
| **Protocol** | Rust structs `GetState`, `GetLeaves` | **sdk**: Proto `zvote.v1.Query` + `CommitmentTreeState`; add `GetCommitmentLeavesRequest/Response` in `proto/zvote/v1/query.proto` and keeper |
| **Tree core** | `vote-commitment-tree` | Shared: chain uses same tree via FFI for `ComputeTreeRoot`; client uses same crate for ShardTree |
