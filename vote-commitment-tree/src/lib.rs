//! Append-only Poseidon Merkle tree for the Vote Commitment Tree (Gov Steps V1).
//!
//! This tree holds both **Vote Authority Notes (VANs)** and **Vote Commitments (VCs)** as leaves.
//! Domain separation (DOMAIN_VAN / DOMAIN_VC) is applied when *constructing* leaf values
//! (in circuits / chain); this crate stores and hashes already-committed field elements.
//!
//! Insertion order (per cosmos-sdk-messages-spec):
//! - `MsgDelegateVote` → append 1 leaf (VAN)
//! - `MsgCastVote` → append 2 leaves (new VAN, then VC)
//!
//! ## Architecture
//!
//! The crate is split into server and client layers with a sync API boundary:
//!
//! - **Shared types** ([`MerkleHashVote`], [`Anchor`], [`MerklePath`]) — used by both sides.
//! - **[`TreeServer`]** — authoritative full tree: append, checkpoint, serve data via [`TreeSyncApi`].
//! - **[`TreeClient`]** — sparse tree: sync from server, mark positions, generate witnesses.
//! - **[`TreeSyncApi`]** — trait defining the communication boundary (in-process for POC,
//!   maps to Cosmos SDK endpoints in production).
//!
//! Built on `incrementalmerkletree` / `shardtree` (same crates that back Orchard's
//! note commitment tree), with two substitutions:
//! - **Hash:** Poseidon (no layer tagging) instead of Sinsemilla
//! - **Empty leaf:** `poseidon_hash(0, 0)` instead of `Fp::from(2)`

// -- Modules ---------------------------------------------------------------

mod anchor;
pub mod client;
mod hash;
mod path;
pub mod server;
pub mod sync_api;

// -- Re-exports (public API) -----------------------------------------------

pub use anchor::Anchor;
pub use client::{SyncError, TreeClient};
pub use hash::{MerkleHashVote, TREE_DEPTH};
pub use path::{MerklePath, MERKLE_PATH_BYTES};
pub use server::TreeServer;
pub use sync_api::TreeSyncApi;

/// Backwards-compatible alias for [`TreeServer`].
///
/// The original `VoteCommitmentTree` API is fully preserved on `TreeServer`.
pub type VoteCommitmentTree = TreeServer;

// -- Shared utilities ------------------------------------------------------

use pasta_curves::Fp;

/// Poseidon hash of two field elements (delegates to imt-tree for circuit consistency).
#[inline]
pub fn poseidon_hash(left: Fp, right: Fp) -> Fp {
    imt_tree::poseidon_hash(left, right)
}
