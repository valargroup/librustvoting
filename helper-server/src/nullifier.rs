//! Share nullifier derivation.
//!
//! Per Gov Steps V1 §5.3:
//! ```text
//! share_nullifier = Poseidon(
//!     Poseidon(domain_tag, vote_commitment),
//!     Poseidon(share_index_fp, enc_share_hash)
//! )
//! ```
//!
//! Uses the same Poseidon (P128Pow5T3 over Pallas Fp) as the vote-commitment-tree
//! crate. The two-level hash-chain approach packs 4 logical inputs into arity-2
//! Poseidon calls.

use base64::prelude::*;
use ff::PrimeField;
use pasta_curves::Fp;

use vote_commitment_tree::poseidon_hash;

use crate::types::SharePayload;

/// Domain separator for share nullifiers, encoded as an Fp element.
/// "share spend" → first 31 bytes of the UTF-8 string, zero-padded.
fn domain_tag() -> Fp {
    let mut bytes = [0u8; 32];
    let tag = b"share spend";
    bytes[..tag.len()].copy_from_slice(tag);
    // Ensure the encoding is canonical (< field modulus). Since the tag is
    // short, the top byte is zero, so this always succeeds.
    Fp::from_repr(bytes).unwrap()
}

/// Derive the share nullifier for a queued share.
///
/// Inputs:
/// - `vote_commitment`: the VC leaf value (Fp) from the vote commitment tree
/// - `share_index`: which of the 4 shares (0..3)
/// - `enc_share_c1`, `enc_share_c2`: the El Gamal ciphertext components
///
/// Returns `None` if any field decoding fails.
pub fn derive_share_nullifier(payload: &SharePayload, vote_commitment: Fp) -> Option<Fp> {
    let share_index_fp = Fp::from(payload.enc_share.share_index as u64);

    // Hash the encrypted share: Poseidon(c1_fp, c2_fp).
    let c1_bytes = BASE64_STANDARD.decode(&payload.enc_share.c1).ok()?;
    let c2_bytes = BASE64_STANDARD.decode(&payload.enc_share.c2).ok()?;
    if c1_bytes.len() != 32 || c2_bytes.len() != 32 {
        return None;
    }
    let mut c1_arr = [0u8; 32];
    let mut c2_arr = [0u8; 32];
    c1_arr.copy_from_slice(&c1_bytes);
    c2_arr.copy_from_slice(&c2_bytes);
    let c1_fp = Option::from(Fp::from_repr(c1_arr))?;
    let c2_fp = Option::from(Fp::from_repr(c2_arr))?;
    let enc_share_hash = poseidon_hash(c1_fp, c2_fp);

    // Two-level hash chain: pack 4 inputs into 3 arity-2 calls.
    let left = poseidon_hash(domain_tag(), vote_commitment);
    let right = poseidon_hash(share_index_fp, enc_share_hash);
    Some(poseidon_hash(left, right))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::EncryptedShareWire;

    #[test]
    fn nullifier_deterministic() {
        let vc = Fp::from(42);
        let c1 = BASE64_STANDARD.encode(Fp::from(1).to_repr());
        let c2 = BASE64_STANDARD.encode(Fp::from(2).to_repr());

        let payload = SharePayload {
            shares_hash: BASE64_STANDARD.encode([0u8; 32]),
            proposal_id: 0,
            vote_decision: 1,
            enc_share: EncryptedShareWire {
                c1: c1.clone(),
                c2: c2.clone(),
                share_index: 0,
            },
            tree_position: 1,
            vote_round_id: hex::encode([0u8; 32]),
        };

        let nf1 = derive_share_nullifier(&payload, vc).unwrap();
        let nf2 = derive_share_nullifier(&payload, vc).unwrap();
        assert_eq!(nf1, nf2);

        // Different share_index → different nullifier.
        let mut payload2 = payload.clone();
        payload2.enc_share.share_index = 1;
        let nf3 = derive_share_nullifier(&payload2, vc).unwrap();
        assert_ne!(nf1, nf3);
    }
}
