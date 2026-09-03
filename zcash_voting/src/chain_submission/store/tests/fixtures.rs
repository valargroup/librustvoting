use std::sync::Arc;

use crate::{
    storage::{queries, VotingDb},
    types::EncryptedShare,
    vote::{VoteBatchRecovery, VoteRecoveryBundle},
    ChainSubmissionIdentity, ChainSubmissionTarget, VotingRoundParams,
};

pub(super) const ROUND: &str = "1111111111111111111111111111111111111111111111111111111111111111";

pub(super) fn temporary_path(label: &str) -> String {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir()
        .join(format!(
            "chain-submission-{label}-{}-{nonce}.sqlite",
            std::process::id()
        ))
        .to_string_lossy()
        .into_owned()
}

pub(super) fn open_prepared(path: &str) -> Arc<VotingDb> {
    let db = Arc::new(VotingDb::open(path).unwrap());
    db.set_wallet_id("wallet");
    if !db.has_round(ROUND).unwrap() {
        db.create_round(
            crate::Network::Testnet,
            &VotingRoundParams {
                vote_round_id: ROUND.to_string(),
                snapshot_height: 100,
                ea_pk: vec![0xea; 32],
                nc_root: vec![0xaa; 32],
                nullifier_imt_root: vec![0xbb; 32],
            },
            None,
        )
        .unwrap();
        queries::insert_bundle(&db.conn(), ROUND, "wallet", 0, &[1]).unwrap();
        crate::vote::insert_recovery_fixture(&db, &recovery()).unwrap();
    }
    db
}

pub(super) fn identity() -> ChainSubmissionIdentity {
    ChainSubmissionIdentity::new(
        "wallet",
        crate::Network::Testnet,
        [0x11; 32],
        0,
        ChainSubmissionTarget::Vote { proposal_id: 1 },
    )
    .unwrap()
}

fn recovery() -> VoteRecoveryBundle {
    VoteRecoveryBundle {
        vote_round_id: ROUND.to_string(),
        bundle_index: 0,
        proposal_id: 1,
        vote_decision: 2,
        anchor_height: 123,
        vc_tree_position: 0,
        single_share: false,
        num_options: 3,
        van_nullifier: [0x10; 32],
        vote_authority_note_new: [0x01; 32],
        vote_commitment: [0x21; 32],
        proof: vec![0x13; 96],
        shares_hash: [0x14; 32],
        r_vpk: [0x15; 32],
        alpha_v: [0x16; 32],
        vote_auth_sig: [0x17; 64],
        encrypted_shares: vec![EncryptedShare {
            c1: vec![0x21; 32],
            c2: vec![0x22; 32],
            share_index: 0,
            plaintext_value: 5,
            randomness: vec![0x23; 32],
        }],
        share_blinds: vec![[0x41; 32]],
        share_comms: vec![[0x51; 32]],
        batch: None::<VoteBatchRecovery>,
    }
}
