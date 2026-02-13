# Zcash Note Commitment Tree (NCT)

Summary of how the **note commitment tree** works on Zcash mainnet. This is the append-only Merkle tree that stores note commitments for the shielded pools. We use its root at snapshot height as the anchor `nc_root` for ZKP #1 in the voting protocol; we do not build or maintain this tree in this repo.

## Pools and depths

| Pool    | Tree depth | Leaf type                          | Merkle CRH        |
|---------|------------|------------------------------------|-------------------|
| Sprout  | 29         | Bit-twiddling hash output          | (spec-specific)   |
| Sapling | 32         | Jubjub $u$-coordinate (`cmu`)      | Bowe–Hopwood Pedersen |
| Orchard | 32         | Pallas $x$-coordinate (`cmx`)      | Sinsemilla        |

Orchard is the one used for voting (Orchard notes only).

## Structure (Orchard)

- **Single global tree**, fixed depth **32** → **2³²** leaf positions.
- **Append-only:** each new shielded output appends one note commitment at the next leaf index. Order: block order, then transaction/output order within the block.
- **Leaves:** The **extracted note commitment** `cmx` = $x$-coordinate of the Pallas note commitment point (from `NoteCommit^Orchard` over $g_d$, $pk_d$, value, $\rho$, $\psi$ with trapdoor $rcm$).
- **Internal nodes:** $\mathsf{MerkleCRH}^\mathsf{Orchard}(\mathsf{layer}, \mathsf{left}, \mathsf{right})$ — Sinsemilla with personalization `"z.cash:Orchard-MerkleCRH"`, inputs: layer index (10 bits), then 255 bits of left, 255 bits of right (each a Pallas base field element).
- **Uncommitted leaf:** Value **2** ($\mathbb{F}_p$). Not the $x$-coordinate of any Pallas point, so it cannot collide with a real note commitment. Unused leaves are treated as 2; empty subtrees have deterministic roots (precomputed “empty roots” per level).
- **Anchor:** The tree root at a **block boundary** (after all commitments from that block are appended). Valid spend (or voting) proofs use the Merkle path from a leaf to this root.

## Capacity

- **2³² ≈ 4.3 billion** leaf positions. When the last position is used, no further note commitments can be added without a protocol change (e.g. a new tree or new structure in a future upgrade).

## Wallet / light client (how the tree is used)

- Wallets do **not** store the full tree. They use a **ShardTree** (incremental / sparse) and only materialize subtrees needed for witnesses.
- **Sync:** Download compact blocks (with `cmx` per Orchard output), trial-decrypt with viewing keys, insert commitments into the local ShardTree, optionally use precomputed subtree roots from the server for fast-sync.
- **Witness:** For a note at leaf index `position` and anchor at block height $H$, the wallet calls something like `witness_at_checkpoint_id(position, H)` to get the **Merkle path** (32 sibling hashes). The path plus `cmx` hash to the published anchor; the circuit (or verifier) checks that root.

### How your path updates when others append

You track your leaf; when someone else’s note is appended, it can lie in a sibling branch, so the sibling hashes on your path change. There is **no separate “sibling updated” notification**. You learn about it by **receiving the next compact block(s)**. Each block carries **every** new `cmx` for that block in append order. You **insert all of them** into your local tree (not only the notes you decrypted). That updates the tree state; when you then ask for a witness at the new block height, you get the updated path with the new sibling values. If you only inserted your own commitments, your tree would be wrong—your path depends on the full ordered sequence of leaves. So: one ordered stream of commitments per block; you apply it; your path stays correct.

## References

- **Spec:** Zcash Protocol Spec — e.g. Orchard Merkle CRH, uncommitted value (Orchard), action encoding.
- **In this repo:** `orchard/src/tree.rs` (Merkle hash, path, anchor), `orchard/src/note/commitment.rs` (note commitment, `cmx`), `orchard/book/src/design/commitment-tree.md`.
- **Voting use:** `nc_root` at snapshot height is the anchor for note-inclusion proofs in ZKP #1; see this crate’s README and BRIDGE_TREE.md.
