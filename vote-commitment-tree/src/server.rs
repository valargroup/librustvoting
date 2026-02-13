//! Server-side vote commitment tree.
//!
//! [`TreeServer`] owns the authoritative full tree. It wraps a
//! `ShardTree<MemoryShardStore, 32, 4>` and provides:
//! - `append` / `append_two` for leaf insertion (from `MsgDelegateVote` / `MsgCastVote`)
//! - `checkpoint` for EndBlocker root snapshots
//! - `root_at_height` / `path` for anchor lookup and witness generation
//! - [`TreeSyncApi`] implementation to serve data to clients
//!
//! In production, the Go keeper owns persistence and the Rust FFI builds the
//! tree from KV; the POC just skips the Go layer.

use std::collections::BTreeMap;
use std::convert::Infallible;

use incrementalmerkletree::{Hashable, Level, Retention};
use pasta_curves::Fp;
use shardtree::{store::memory::MemoryShardStore, ShardTree};

use crate::hash::{MerkleHashVote, MAX_CHECKPOINTS, SHARD_HEIGHT, TREE_DEPTH};
use crate::path::MerklePath;
use crate::sync_api::{BlockCommitments, TreeState, TreeSyncApi};

// ---------------------------------------------------------------------------
// TreeServer
// ---------------------------------------------------------------------------

/// Server-side vote commitment tree: full tree with continuous appends.
///
/// Provides the same API as the original `VoteCommitmentTree`, plus block-level
/// bookkeeping to implement [`TreeSyncApi`] for client sync.
pub struct TreeServer {
    inner: ShardTree<
        MemoryShardStore<MerkleHashVote, u32>,
        { TREE_DEPTH as u8 },
        { SHARD_HEIGHT },
    >,
    /// Next leaf position (number of leaves appended).
    next_position: u64,
    /// Latest checkpoint id (block height) that has been recorded.
    latest_checkpoint: Option<u32>,

    // -- Sync bookkeeping --------------------------------------------------

    /// Completed blocks: height → commitments (populated on `checkpoint`).
    blocks: BTreeMap<u32, BlockCommitments>,
    /// Leaves accumulated for the current (not yet checkpointed) block.
    pending_leaves: Vec<MerkleHashVote>,
    /// Start index for the pending block.
    pending_start: u64,
}

impl TreeServer {
    /// Create an empty tree.
    pub fn empty() -> Self {
        Self {
            inner: ShardTree::new(MemoryShardStore::empty(), MAX_CHECKPOINTS),
            next_position: 0,
            latest_checkpoint: None,
            blocks: BTreeMap::new(),
            pending_leaves: Vec::new(),
            pending_start: 0,
        }
    }

    /// Append a single leaf (e.g. one VAN from `MsgDelegateVote`).
    ///
    /// The leaf is marked so witnesses can be generated for it later.
    /// Returns the leaf index.
    pub fn append(&mut self, leaf: Fp) -> u64 {
        let index = self.next_position;
        let hash = MerkleHashVote::from_fp(leaf);
        self.inner
            .append(hash, Retention::Marked)
            .expect("append must succeed (tree not full)");
        self.pending_leaves.push(hash);
        self.next_position += 1;
        index
    }

    /// Append two leaves (e.g. new VAN + VC from `MsgCastVote`).
    ///
    /// Returns the index of the first leaf.
    pub fn append_two(&mut self, first: Fp, second: Fp) -> u64 {
        let index = self.append(first);
        self.append(second);
        index
    }

    /// Snapshot the current tree state at the given block height.
    ///
    /// Called by EndBlocker after processing all transactions in a block.
    /// The root at this checkpoint becomes a valid anchor for ZKP #2 / ZKP #3.
    pub fn checkpoint(&mut self, height: u32) {
        self.inner
            .checkpoint(height)
            .expect("checkpoint must succeed");
        self.latest_checkpoint = Some(height);

        // Record block-level data for the sync API.
        let commitments = BlockCommitments {
            height,
            start_index: self.pending_start,
            leaves: std::mem::take(&mut self.pending_leaves),
        };
        self.blocks.insert(height, commitments);
        self.pending_start = self.next_position;
    }

    /// Current Merkle root (at the latest checkpoint).
    pub fn root(&self) -> Fp {
        if let Some(id) = self.latest_checkpoint {
            self.inner
                .root_at_checkpoint_id(&id)
                .ok()
                .flatten()
                .map(|h| h.0)
                .unwrap_or_else(|| MerkleHashVote::empty_root(Level::from(TREE_DEPTH as u8)).0)
        } else {
            MerkleHashVote::empty_root(Level::from(TREE_DEPTH as u8)).0
        }
    }

    /// Root at a specific checkpoint height (anchor lookup).
    ///
    /// Returns `None` if the checkpoint does not exist.
    pub fn root_at_height(&self, height: u32) -> Option<Fp> {
        self.inner
            .root_at_checkpoint_id(&height)
            .ok()
            .flatten()
            .map(|h| h.0)
    }

    /// Number of leaves appended.
    pub fn size(&self) -> u64 {
        self.next_position
    }

    /// Build a Merkle path for the leaf at `position`, valid at the given
    /// checkpoint `anchor_height`.
    ///
    /// Returns `None` if the position or checkpoint is invalid.
    pub fn path(&self, position: u64, anchor_height: u32) -> Option<MerklePath> {
        let pos = incrementalmerkletree::Position::from(position);
        self.inner
            .witness_at_checkpoint_id(pos, &anchor_height)
            .ok()
            .flatten()
            .map(MerklePath::from)
    }
}

// ---------------------------------------------------------------------------
// TreeSyncApi implementation
// ---------------------------------------------------------------------------

impl TreeSyncApi for TreeServer {
    type Error = Infallible;

    fn get_block_commitments(
        &self,
        from_height: u32,
        to_height: u32,
    ) -> Result<Vec<BlockCommitments>, Self::Error> {
        let blocks = self
            .blocks
            .range(from_height..=to_height)
            .map(|(_, bc)| bc.clone())
            .collect();
        Ok(blocks)
    }

    fn get_root_at_height(&self, height: u32) -> Result<Option<Fp>, Self::Error> {
        Ok(self.root_at_height(height))
    }

    fn get_tree_state(&self) -> Result<TreeState, Self::Error> {
        Ok(TreeState {
            next_index: self.next_position,
            root: self.root(),
            height: self.latest_checkpoint.unwrap_or(0),
        })
    }
}

// ---------------------------------------------------------------------------
// Tests (migrated from the original tree.rs)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anchor::Anchor;
    use crate::hash::EMPTY_ROOTS;

    fn fp(x: u64) -> Fp {
        Fp::from(x)
    }

    #[test]
    fn empty_tree_has_deterministic_root() {
        let t1 = TreeServer::empty();
        let t2 = TreeServer::empty();
        assert_eq!(t1.root(), t2.root());
        assert_eq!(t1.size(), 0);
    }

    #[test]
    fn empty_roots_are_consistent() {
        let leaf = MerkleHashVote::empty_leaf();
        assert_eq!(EMPTY_ROOTS[0], leaf);

        let mut expected = leaf;
        for level in 0..TREE_DEPTH {
            assert_eq!(
                EMPTY_ROOTS[level], expected,
                "empty root mismatch at level {}",
                level
            );
            expected = MerkleHashVote::combine(Level::from(level as u8), &expected, &expected);
        }
    }

    #[test]
    fn append_one_and_path() {
        let mut tree = TreeServer::empty();
        let idx = tree.append(fp(100));
        assert_eq!(idx, 0);
        assert_eq!(tree.size(), 1);

        tree.checkpoint(1);
        let path = tree.path(0, 1).unwrap();
        assert!(path.verify(fp(100), tree.root()));
    }

    #[test]
    fn append_two_leaves_paths_verify() {
        let mut tree = TreeServer::empty();
        tree.append(fp(1));
        tree.append(fp(2));
        assert_eq!(tree.size(), 2);

        tree.checkpoint(1);
        let root = tree.root();
        for i in 0..2u64 {
            let leaf = fp(i + 1);
            let path = tree.path(i, 1).unwrap();
            assert!(path.verify(leaf, root), "path for leaf {} must verify", i);
        }
    }

    #[test]
    fn append_two_batch() {
        let mut tree = TreeServer::empty();
        let idx = tree.append_two(fp(10), fp(20));
        assert_eq!(idx, 0);
        assert_eq!(tree.size(), 2);

        tree.checkpoint(1);
        let root = tree.root();
        let p0 = tree.path(0, 1).unwrap();
        let p1 = tree.path(1, 1).unwrap();
        assert!(p0.verify(fp(10), root));
        assert!(p1.verify(fp(20), root));
    }

    #[test]
    fn path_reject_wrong_leaf() {
        let mut tree = TreeServer::empty();
        tree.append(fp(42));
        tree.checkpoint(1);
        let path = tree.path(0, 1).unwrap();
        assert!(!path.verify(fp(0), tree.root()));
        assert!(!path.verify(fp(43), tree.root()));
    }

    #[test]
    fn path_reject_wrong_root() {
        let mut tree = TreeServer::empty();
        tree.append(fp(1));
        tree.checkpoint(1);
        let path = tree.path(0, 1).unwrap();
        assert!(!path.verify(fp(1), Fp::zero()));
    }

    #[test]
    fn checkpoint_preserves_root() {
        let mut tree = TreeServer::empty();
        tree.append(fp(1));
        tree.append(fp(2));
        tree.checkpoint(10);

        let root_at_10 = tree.root_at_height(10).unwrap();

        tree.append(fp(3));
        tree.checkpoint(11);

        assert_eq!(tree.root_at_height(10).unwrap(), root_at_10);
        assert_ne!(tree.root_at_height(11).unwrap(), root_at_10);
    }

    #[test]
    fn witness_at_earlier_checkpoint() {
        let mut tree = TreeServer::empty();
        tree.append(fp(100));
        tree.checkpoint(5);

        tree.append(fp(200));
        tree.checkpoint(6);

        let root_5 = tree.root_at_height(5).unwrap();
        let path = tree.path(0, 5).unwrap();
        assert!(path.verify(fp(100), root_5));

        let root_6 = tree.root_at_height(6).unwrap();
        let path = tree.path(1, 6).unwrap();
        assert!(path.verify(fp(200), root_6));
    }

    #[test]
    fn anchor_empty_tree() {
        let anchor = Anchor::empty_tree();
        let tree = TreeServer::empty();
        assert_eq!(anchor.inner(), tree.root());
    }

    // -- TreeSyncApi tests -------------------------------------------------

    #[test]
    fn sync_api_get_tree_state() {
        let mut server = TreeServer::empty();
        server.append(fp(1));
        server.checkpoint(1);

        let state = server.get_tree_state().unwrap();
        assert_eq!(state.next_index, 1);
        assert_eq!(state.height, 1);
        assert_eq!(state.root, server.root());
    }

    #[test]
    fn sync_api_get_block_commitments() {
        let mut server = TreeServer::empty();

        server.append(fp(10));
        server.checkpoint(1);

        server.append(fp(20));
        server.append(fp(30));
        server.checkpoint(2);

        let blocks = server.get_block_commitments(1, 2).unwrap();
        assert_eq!(blocks.len(), 2);

        assert_eq!(blocks[0].height, 1);
        assert_eq!(blocks[0].start_index, 0);
        assert_eq!(blocks[0].leaves.len(), 1);
        assert_eq!(blocks[0].leaves[0], MerkleHashVote::from_fp(fp(10)));

        assert_eq!(blocks[1].height, 2);
        assert_eq!(blocks[1].start_index, 1);
        assert_eq!(blocks[1].leaves.len(), 2);
    }

    #[test]
    fn sync_api_get_root_at_height() {
        let mut server = TreeServer::empty();
        server.append(fp(1));
        server.checkpoint(5);

        let root = server.get_root_at_height(5).unwrap();
        assert!(root.is_some());
        assert_eq!(root.unwrap(), server.root_at_height(5).unwrap());

        let no_root = server.get_root_at_height(99).unwrap();
        assert!(no_root.is_none());
    }
}
