//! IMT (Indexed Merkle Tree) utilities for the delegation proof system.
//!
//! Provides out-of-circuit helpers for building and verifying Poseidon-based
//! Indexed Merkle Tree non-membership proofs. Used by the delegation circuit
//! and builder.

use ff::PrimeField;
use halo2_gadgets::poseidon::primitives::{self as poseidon, ConstantLength};
use pasta_curves::pallas;

/// Depth of the nullifier Indexed Merkle Tree (Poseidon-based).
pub const IMT_DEPTH: usize = 32;

/// Domain tag for governance authorization nullifier (per spec §1.3.2, condition 14).
///
/// `"governance authorization"` encoded as a little-endian Pallas field element.
pub(crate) fn gov_auth_domain_tag() -> pallas::Base {
    let mut bytes = [0u8; 32];
    bytes[..24].copy_from_slice(b"governance authorization");
    pallas::Base::from_repr(bytes).unwrap()
}

/// Compute Poseidon hash of two field elements (out of circuit).
pub(crate) fn poseidon_hash_2(a: pallas::Base, b: pallas::Base) -> pallas::Base {
    poseidon::Hash::<_, poseidon::P128Pow5T3, ConstantLength<2>, 3, 2>::init().hash([a, b])
}

/// Compute governance nullifier out-of-circuit (per spec §1.3.2, condition 14).
///
/// `gov_null = Poseidon(nk, Poseidon(domain_tag, Poseidon(vote_round_id, real_nf)))`
///
/// where `domain_tag` = `"governance authorization"` as a field element.
pub(crate) fn gov_null_hash(
    nk: pallas::Base,
    vote_round_id: pallas::Base,
    real_nf: pallas::Base,
) -> pallas::Base {
    let step1 = poseidon_hash_2(vote_round_id, real_nf);
    let step2 = poseidon_hash_2(gov_auth_domain_tag(), step1);
    poseidon_hash_2(nk, step2)
}

/// IMT non-membership proof data.
#[derive(Clone, Debug)]
pub struct ImtProofData {
    /// The Merkle root of the IMT.
    pub root: pallas::Base,
    /// The low nullifier of the bracketing leaf.
    pub low_nf: pallas::Base,
    /// The next nullifier of the bracketing leaf (0 for max leaf).
    pub next_nf: pallas::Base,
    /// Position of the bracketing leaf in the tree.
    pub leaf_pos: u32,
    /// Sibling hashes along the Merkle path.
    pub path: [pallas::Base; IMT_DEPTH],
}

/// Trait for providing IMT non-membership proofs.
///
/// Implementations must return proofs against a consistent root — all proofs
/// from the same provider must share the same `root()` value.
pub trait ImtProvider {
    /// The current IMT root.
    fn root(&self) -> pallas::Base;
    /// Generate a non-membership proof for the given nullifier.
    fn non_membership_proof(&self, nf: pallas::Base) -> ImtProofData;
}

// ================================================================
// Test-only
// ================================================================

#[cfg(test)]
use ff::Field;

/// Precomputed empty subtree hashes for the IMT (Poseidon-based).
///
/// `empty[0] = Poseidon(0, 0)`, `empty[i] = Poseidon(empty[i-1], empty[i-1])`.
#[cfg(test)]
pub(crate) fn empty_imt_hashes() -> Vec<pallas::Base> {
    let mut hashes = vec![poseidon_hash_2(pallas::Base::zero(), pallas::Base::zero())];
    for _ in 1..=IMT_DEPTH {
        let prev = *hashes.last().unwrap();
        hashes.push(poseidon_hash_2(prev, prev));
    }
    hashes
}

/// IMT provider with evenly-spaced leaves for testing.
///
/// Creates 17 leaves at intervals of 2^250, covering the entire Pallas field
/// (p ~= 16.something x 2^250). Any hash-derived nullifier will have
/// `diff1 < 2^250`, satisfying the circuit's 250-bit range check.
#[cfg(test)]
#[derive(Debug)]
pub struct SpacedLeafImtProvider {
    /// The root of the IMT.
    root: pallas::Base,
    /// Leaf data: `(low_nf, next_nf)` for each of the 17 leaves at positions 0..16.
    leaves: Vec<(pallas::Base, pallas::Base)>,
    /// Bottom 5 levels of the 32-leaf subtree.
    /// `subtree_levels[0]` has 32 leaf hashes, `subtree_levels[5]` has 1 subtree root.
    subtree_levels: Vec<Vec<pallas::Base>>,
}

#[cfg(test)]
impl SpacedLeafImtProvider {
    /// Create a new spaced-leaf IMT provider.
    ///
    /// Builds 17 leaves at positions 0..16 with `low_nf = k * 2^250`:
    /// - Leaf k (k=0..15): `(k*step, (k+1)*step)`
    /// - Leaf 16: `(16*step, 0)` — max leaf, covers nf in `(16*step, p)`
    pub fn new() -> Self {
        let step = pallas::Base::from(2u64).pow([250, 0, 0, 0]);
        let empty = empty_imt_hashes();

        // Build 17 leaves.
        let mut leaves = Vec::with_capacity(17);
        for k in 0u64..17 {
            let low_nf = step * pallas::Base::from(k);
            let next_nf = if k < 16 {
                step * pallas::Base::from(k + 1)
            } else {
                pallas::Base::zero() // max leaf
            };
            leaves.push((low_nf, next_nf));
        }

        // Build 32-position subtree (positions 0..31). Positions 17..31 are empty.
        let mut level0 = vec![empty[0]; 32];
        for (i, (low, next)) in leaves.iter().enumerate() {
            level0[i] = poseidon_hash_2(*low, *next);
        }

        let mut subtree_levels = vec![level0];
        for _l in 1..=5 {
            let prev = subtree_levels.last().unwrap();
            let mut current = Vec::with_capacity(prev.len() / 2);
            for j in 0..(prev.len() / 2) {
                current.push(poseidon_hash_2(prev[2 * j], prev[2 * j + 1]));
            }
            subtree_levels.push(current);
        }

        // Compute full root: hash subtree root up through levels 5..31 with empty siblings.
        let mut root = subtree_levels[5][0];
        for l in 5..IMT_DEPTH {
            root = poseidon_hash_2(root, empty[l]);
        }

        SpacedLeafImtProvider {
            root,
            leaves,
            subtree_levels,
        }
    }
}

#[cfg(test)]
impl ImtProvider for SpacedLeafImtProvider {
    fn root(&self) -> pallas::Base {
        self.root
    }

    fn non_membership_proof(&self, nf: pallas::Base) -> ImtProofData {
        // Determine which bracket nf falls in: k = nf >> 250.
        // In the LE byte repr, bit 250 is bit 2 of byte 31.
        let repr = nf.to_repr();
        let k = (repr.as_ref()[31] >> 2) as usize;
        let k = k.min(16); // clamp to valid range

        let (low_nf, next_nf) = self.leaves[k];
        let leaf_pos = k as u32;

        let empty = empty_imt_hashes();

        // Build Merkle path.
        let mut path = [pallas::Base::zero(); IMT_DEPTH];

        // Levels 0..4: siblings from the 32-leaf subtree.
        let mut idx = k;
        for l in 0..5 {
            let sibling_idx = idx ^ 1;
            path[l] = self.subtree_levels[l][sibling_idx];
            idx >>= 1;
        }

        // Levels 5..31: empty subtree hashes (all leaves beyond position 31 are empty).
        for l in 5..IMT_DEPTH {
            path[l] = empty[l];
        }

        ImtProofData {
            root: self.root,
            low_nf,
            next_nf,
            leaf_pos,
            path,
        }
    }
}
