use std::sync::Arc;

use crate::{
    chain_submission::{
        protocol::CommittedTransaction, state::SubmissionObservation, CandidateTransactionHash,
        ChainSubmissionIdentity, ChainSubmissionState, ChainSubmissionTarget,
    },
    confirmation::{TxEvent, TxEventAttribute},
    storage::{queries, VotingDb},
    types::EncryptedShare,
    vote::{VoteBatchRecovery, VoteRecoveryBundle},
    Network, VotingRoundParams,
};

use super::super::{
    ChainSubmissionStore, ConfirmationCommit, SqliteChainSubmissionStore, StoreAdmission,
    StoreAdvancementRequest,
};

const ROUND: &str = "1111111111111111111111111111111111111111111111111111111111111111";

#[test]
fn atomic_batch_tracking_and_confirmation_survive_reopen() {
    let path = temporary_path("batch-confirmed");
    let candidate = CandidateTransactionHash::from_bytes([0x45; 32]);
    let (digest, generation) = {
        let (db, digest) = open_prepared_batch(&path);
        let store = SqliteChainSubmissionStore::new(db);
        let request =
            StoreAdvancementRequest::vote_batch(batch_identity(digest), vec![1, 2]).unwrap();
        let StoreAdmission::Ready { derived, .. } = store.admit(&request, true, 1, 10).unwrap()
        else {
            panic!("fresh batch admission")
        };
        let generation = derived.generation().clone();
        store
            .classify_post(
                &generation,
                SubmissionObservation::UsableCandidateHash(candidate),
                11,
            )
            .unwrap();
        (digest, generation)
    };

    {
        let (db, reopened_digest) = open_prepared_batch(&path);
        assert_eq!(reopened_digest, digest);
        let store = SqliteChainSubmissionStore::new(Arc::clone(&db));
        let request =
            StoreAdvancementRequest::vote_batch(batch_identity(digest), vec![1, 2]).unwrap();
        let StoreAdmission::Ready { record, .. } = store.admit(&request, true, 1, 12).unwrap()
        else {
            panic!("tracking batch must reconcile after reopen")
        };
        assert_eq!(record.durable_state(), ChainSubmissionState::Tracking);

        let recoveries = batch_recoveries().1;
        let committed = committed_batch(digest, &recoveries);
        let result = store
            .confirm_committed(&request, &generation, candidate, &committed, &|| true, 13)
            .unwrap();
        assert!(matches!(result, ConfirmationCommit::Confirmed(_)));
        assert_eq!(
            db.get_vote_tx_hash(ROUND, 0, 1).unwrap().as_deref(),
            Some(candidate.to_string().as_str())
        );
        for (proposal_id, position) in [(1, 8), (2, 9)] {
            assert_eq!(
                queries::load_vote_row_state(&db.conn(), ROUND, "wallet", 0, proposal_id)
                    .unwrap()
                    .unwrap()
                    .vc_tree_position,
                Some(position)
            );
        }
    }

    let _ = std::fs::remove_file(path);
}

fn temporary_path(label: &str) -> String {
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
        batch: None,
    }
}

fn batch_recoveries() -> ([u8; 32], Vec<VoteRecoveryBundle>) {
    let first = recovery();
    let mut second = first.clone();
    second.proposal_id = 2;
    second.vote_decision = 1;
    second.van_nullifier = first.vote_authority_note_new;
    second.vote_authority_note_new = [0x02; 32];
    second.vote_commitment = [0x22; 32];
    let mut recoveries = vec![first, second];
    let actions = recoveries
        .iter()
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
    let digest = crate::vote_commitment::cast_vote_batch_sighash(ROUND, 123, &actions).unwrap();
    for (index, recovery) in recoveries.iter_mut().enumerate() {
        recovery.batch = Some(VoteBatchRecovery {
            digest,
            index: index as u32,
            size: 2,
        });
    }
    (digest, recoveries)
}

fn open_prepared_batch(path: &str) -> (Arc<VotingDb>, [u8; 32]) {
    let db = Arc::new(VotingDb::open(path).unwrap());
    db.set_wallet_id("wallet");
    let (digest, recoveries) = batch_recoveries();
    if !db.has_round(ROUND).unwrap() {
        db.create_round(
            Network::Testnet,
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
        queries::insert_bundle(&db.conn(), ROUND, "wallet", 0, &[1, 2]).unwrap();
        for recovery in &recoveries {
            crate::vote::insert_recovery_fixture(&db, recovery).unwrap();
        }
    }
    (db, digest)
}

fn batch_identity(digest: [u8; 32]) -> ChainSubmissionIdentity {
    ChainSubmissionIdentity::new(
        "wallet",
        Network::Testnet,
        [0x11; 32],
        0,
        ChainSubmissionTarget::VoteBatch {
            ordered_batch_digest: digest,
        },
    )
    .unwrap()
}

fn committed_batch(digest: [u8; 32], recoveries: &[VoteRecoveryBundle]) -> CommittedTransaction {
    CommittedTransaction {
        height: 8,
        code: 0,
        events: vec![TxEvent {
            event_type: "cast_vote_batch".to_string(),
            attributes: vec![
                TxEventAttribute {
                    key: "vote_round_id".to_string(),
                    value: ROUND.to_string(),
                },
                TxEventAttribute {
                    key: "batch_digest".to_string(),
                    value: hex::encode(digest),
                },
                TxEventAttribute {
                    key: "batch_size".to_string(),
                    value: "2".to_string(),
                },
                TxEventAttribute {
                    key: "final_van_leaf_index".to_string(),
                    value: "7".to_string(),
                },
                TxEventAttribute {
                    key: "vote_commitment_leaf_indices".to_string(),
                    value: "8,9".to_string(),
                },
                TxEventAttribute {
                    key: "proposal_ids".to_string(),
                    value: "1,2".to_string(),
                },
                TxEventAttribute {
                    key: "van_nullifiers".to_string(),
                    value: recoveries
                        .iter()
                        .map(|recovery| hex::encode(recovery.van_nullifier))
                        .collect::<Vec<_>>()
                        .join(","),
                },
            ],
        }],
    }
}
