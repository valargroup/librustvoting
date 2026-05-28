//! Minimal vote/share recovery walkthrough.
//!
//! Full proof generation requires a confirmed delegation bundle and synced VAN
//! witness. This example focuses on the stable post-commit API shape that wallet
//! SDKs use for recovery and helper-share resubmission.

use zcash_voting::prelude::*;
use zcash_voting::{EncryptedShare, WireEncryptedShare};

fn main() -> Result<(), VotingError> {
    let recovery = VoteRecoveryBundle {
        vote_round_id: "00".repeat(32),
        bundle_index: 0,
        proposal_id: 1,
        vote_decision: 0,
        anchor_height: 123,
        vc_tree_position: 42,
        single_share: true,
        num_options: 2,
        van_nullifier: [1; 32],
        vote_authority_note_new: [2; 32],
        vote_commitment: [3; 32],
        proof: vec![4; 96],
        shares_hash: [5; 32],
        r_vpk: [6; 32],
        alpha_v: [7; 32],
        vote_auth_sig: [8; 64],
        encrypted_shares: vec![EncryptedShare {
            c1: vec![9; 32],
            c2: vec![10; 32],
            share_index: 0,
            plaintext_value: 1,
            randomness: vec![11; 32],
        }],
        share_blinds: vec![field_bytes(12)],
        share_comms: vec![field_bytes(13)],
    };

    let json = serialize_recovery(&recovery)?;
    let recovered = parse_recovery(&json)?;
    let payload = recover_payload(&recovered, 0)?;
    let nullifier = compute_nullifier(&recovered.vote_commitment, 0, &field_bytes(12))?;

    assert_eq!(
        payload.enc_share,
        WireEncryptedShare::from(&recovered.encrypted_shares[0])
    );
    assert_eq!(nullifier.len(), 32);
    Ok(())
}

fn field_bytes(value: u8) -> [u8; 32] {
    let mut bytes = [0u8; 32];
    bytes[0] = value;
    bytes
}
