//! Shared fixtures for the SQLite chain-submission store tests.

use super::super::*;
use crate::{
    confirmation::{TxEvent, TxEventAttribute},
    storage::queries,
    types::EncryptedShare,
    vote::{VoteBatchRecovery, VoteRecoveryBundle},
    VotingRoundParams,
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

pub(super) fn recovery() -> VoteRecoveryBundle {
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
        batch: None,
    }
}

pub(super) fn open_prepared(path: &str) -> Arc<VotingDb> {
    let db = Arc::new(VotingDb::open(path).unwrap());
    prepare(db)
}

pub(super) fn open_prepared_in_memory() -> Arc<VotingDb> {
    prepare(Arc::new(VotingDb::open_in_memory().unwrap()))
}

fn prepare(db: Arc<VotingDb>) -> Arc<VotingDb> {
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
    identity_for(0, 1)
}

pub(super) fn identity_for(bundle_index: u32, proposal_id: u32) -> ChainSubmissionIdentity {
    ChainSubmissionIdentity::new(
        "wallet",
        crate::Network::Testnet,
        [0x11; 32],
        bundle_index,
        ChainSubmissionTarget::Vote { proposal_id },
    )
    .unwrap()
}

pub(super) fn recovery_for(bundle_index: u32, proposal_id: u32) -> VoteRecoveryBundle {
    let mut value = recovery();
    value.bundle_index = bundle_index;
    value.proposal_id = proposal_id;
    value.vote_decision = proposal_id % value.num_options;
    value.van_nullifier[0] = bundle_index as u8;
    value.vote_authority_note_new[0] = proposal_id as u8;
    value.vote_commitment[0] = proposal_id as u8;
    value
}

pub(super) fn store_two_vote_batch(db: &VotingDb) -> [u8; 32] {
    let mut first = recovery_for(0, 1);
    let mut second = recovery_for(0, 2);
    second.van_nullifier = [0x20; 32];
    second.vote_authority_note_new = [0x22; 32];
    second.vote_commitment = [0x62; 32];
    second.r_vpk = [0x25; 32];
    let actions = [&first, &second]
        .into_iter()
        .map(
            |recovery| crate::vote_commitment::CastVoteBatchSighashAction {
                r_vpk: &recovery.r_vpk,
                van_nullifier: &recovery.van_nullifier,
                vote_authority_note_new: &recovery.vote_authority_note_new,
                vote_commitment: &recovery.vote_commitment,
                proposal_id: recovery.proposal_id,
            },
        )
        .collect::<Vec<_>>();
    let digest = crate::vote_commitment::cast_vote_batch_sighash(
        ROUND,
        first.anchor_height as u64,
        &actions,
    )
    .unwrap();
    first.batch = Some(VoteBatchRecovery {
        digest,
        index: 0,
        size: 2,
    });
    second.batch = Some(VoteBatchRecovery {
        digest,
        index: 1,
        size: 2,
    });
    for recovery in [&first, &second] {
        queries::store_vote(
            &db.conn(),
            ROUND,
            "wallet",
            0,
            recovery.proposal_id,
            recovery.vote_decision,
            &recovery.vote_commitment,
        )
        .unwrap();
        crate::vote::insert_recovery_fixture(db, recovery).unwrap();
    }
    digest
}

pub(super) fn batch_identity(digest: [u8; 32]) -> ChainSubmissionIdentity {
    ChainSubmissionIdentity::new(
        "wallet",
        crate::Network::Testnet,
        [0x11; 32],
        0,
        ChainSubmissionTarget::VoteBatch {
            ordered_batch_digest: digest,
        },
    )
    .unwrap()
}

pub(super) fn committed() -> crate::chain_submission::protocol::CommittedTransaction {
    crate::chain_submission::protocol::CommittedTransaction {
        height: 8,
        code: 0,
        events: vec![TxEvent {
            event_type: "cast_vote".to_string(),
            attributes: vec![
                TxEventAttribute {
                    key: "vote_round_id".to_string(),
                    value: ROUND.to_string(),
                },
                TxEventAttribute {
                    key: "leaf_index".to_string(),
                    value: "7,8".to_string(),
                },
            ],
        }],
    }
}
