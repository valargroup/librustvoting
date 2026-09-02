//! SQLite authority for the chain-submission lifecycle.

use std::sync::Arc;

use rusqlite::{named_params, OptionalExtension, Transaction, TransactionBehavior};

use crate::storage::VotingDb;

use super::{
    abandoned_diagnostic, ensure_generation, map_generation_error, preserve_loaded_state,
    transition_failure, ChainSubmissionStore, ConfirmationCommit, StoreAdmission,
    StoreAdvancementRequest, StoredChainSubmission, SubmissionDerivationRequest,
};
use crate::chain_submission::{
    confirmation::{apply_confirmed_generation, validate_hash_confirmation},
    coordination::SubmissionCoordination,
    generation::{derive_delegation, derive_vote, derive_vote_batch, DerivedChainSubmission},
    result::{LegacyProjectionConfirmation, ValidatedChainSubmissionConfirmation},
    state::{
        apply_submission_observation, DigestlessRecoveryGuard, SubmissionObservation,
        SubmissionRecordState,
    },
    CandidateTransactionHash, ChainSubmissionDiagnostic, ChainSubmissionDiagnosticKind,
    ChainSubmissionFailure, ChainSubmissionFailureKind, ChainSubmissionGeneration,
    ChainSubmissionGenerationDigest, ChainSubmissionIdentity, ChainSubmissionState,
    ChainSubmissionTarget,
};

pub(crate) struct SqliteChainSubmissionStore {
    db: Arc<VotingDb>,
}

impl SqliteChainSubmissionStore {
    pub(crate) fn new(db: Arc<VotingDb>) -> Self {
        Self { db }
    }

    fn transact<R>(
        &self,
        operation: impl FnOnce(&Transaction<'_>) -> Result<R, ChainSubmissionFailure>,
    ) -> Result<R, ChainSubmissionFailure> {
        let mut conn = self.db.conn();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        let result = operation(&tx)?;
        tx.commit().map_err(storage_error)?;
        Ok(result)
    }
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;
    use crate::{
        confirmation::{TxEvent, TxEventAttribute},
        storage::queries,
        types::EncryptedShare,
        vote::{VoteBatchRecovery, VoteRecoveryBundle},
        VotingRoundParams,
    };

    const ROUND: &str = "1111111111111111111111111111111111111111111111111111111111111111";

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

    fn open_prepared(path: &str) -> Arc<VotingDb> {
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

    fn identity() -> ChainSubmissionIdentity {
        identity_for(0, 1)
    }

    fn identity_for(bundle_index: u32, proposal_id: u32) -> ChainSubmissionIdentity {
        identity_for_chain("vote-chain-1", bundle_index, proposal_id)
    }

    fn identity_for_chain(
        vote_chain_id: &str,
        bundle_index: u32,
        proposal_id: u32,
    ) -> ChainSubmissionIdentity {
        ChainSubmissionIdentity::new(
            "wallet",
            crate::Network::Testnet,
            vote_chain_id,
            [0x11; 32],
            bundle_index,
            ChainSubmissionTarget::Vote { proposal_id },
        )
        .unwrap()
    }

    fn recovery_for(bundle_index: u32, proposal_id: u32) -> VoteRecoveryBundle {
        let mut value = recovery();
        value.bundle_index = bundle_index;
        value.proposal_id = proposal_id;
        value.vote_decision = proposal_id % value.num_options;
        value.van_nullifier[0] = bundle_index as u8;
        value.vote_authority_note_new[0] = proposal_id as u8;
        value.vote_commitment[0] = proposal_id as u8;
        value
    }

    fn store_two_vote_batch(db: &VotingDb) -> [u8; 32] {
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

    fn batch_identity(digest: [u8; 32]) -> ChainSubmissionIdentity {
        ChainSubmissionIdentity::new(
            "wallet",
            crate::Network::Testnet,
            "vote-chain-1",
            [0x11; 32],
            0,
            ChainSubmissionTarget::VoteBatch {
                ordered_batch_digest: digest,
            },
        )
        .unwrap()
    }

    fn committed() -> crate::chain_submission::protocol::CommittedTransaction {
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

    #[test]
    fn restart_normalizes_unclassified_reservation_without_redispatch() {
        let path = temporary_path("abandoned");
        {
            let db = open_prepared(&path);
            let store = SqliteChainSubmissionStore::new(db);
            assert!(matches!(
                store
                    .admit(&StoreAdvancementRequest::vote(identity()), true, 1, 10)
                    .unwrap(),
                StoreAdmission::Ready {
                    fresh_reservation: true,
                    ..
                }
            ));
        }
        {
            let db = open_prepared(&path);
            let store = SqliteChainSubmissionStore::new(db);
            let admission = store
                .admit(&StoreAdvancementRequest::vote(identity()), true, 1, 20)
                .unwrap();
            let StoreAdmission::Authoritative(record) = admission else {
                panic!("restart must not derive or reserve")
            };
            assert_eq!(record.durable_state(), ChainSubmissionState::Recovering);
            assert_eq!(record.committed_post_reservations(), 1);
        }
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn abandoned_batch_normalizes_before_roster_derivation() {
        let db = open_prepared(":memory:");
        let digest = store_two_vote_batch(&db);
        let store = SqliteChainSubmissionStore::new(Arc::clone(&db));
        let request = StoreAdvancementRequest::vote_batch(batch_identity(digest), vec![1, 2])
            .expect("valid batch request");
        assert!(matches!(
            store.admit(&request, true, 1, 10).unwrap(),
            StoreAdmission::Ready {
                fresh_reservation: true,
                ..
            }
        ));
        db.conn()
            .execute(
                "UPDATE votes SET commitment_bundle_json=NULL
                  WHERE round_id=?1 AND wallet_id='wallet'
                    AND bundle_index=0 AND proposal_id=2",
                [ROUND],
            )
            .unwrap();

        let StoreAdmission::Authoritative(record) = store.admit(&request, true, 1, 20).unwrap()
        else {
            panic!("abandoned batch reservation must normalize without derivation")
        };
        assert_eq!(record.durable_state(), ChainSubmissionState::Recovering);
        assert_eq!(record.committed_post_reservations(), 1);
    }

    #[test]
    fn tracking_and_atomic_confirmation_survive_reopen() {
        let path = temporary_path("confirmed");
        let candidate = CandidateTransactionHash::from_bytes([0x44; 32]);
        let generation = {
            let db = open_prepared(&path);
            let store = SqliteChainSubmissionStore::new(db);
            let StoreAdmission::Ready { derived, .. } = store
                .admit(&StoreAdvancementRequest::vote(identity()), true, 1, 10)
                .unwrap()
            else {
                panic!("fresh admission")
            };
            let generation = derived.generation().clone();
            let tracking = store
                .classify_post(
                    &generation,
                    SubmissionObservation::UsableCandidateHash(candidate),
                    11,
                )
                .unwrap();
            assert_eq!(tracking.durable_state(), ChainSubmissionState::Tracking);
            let diagnostic = ChainSubmissionDiagnostic::from_redacted_message(
                ChainSubmissionDiagnosticKind::ReconciliationPending,
                "candidate lookup temporarily unavailable",
            );
            let tracking = store
                .reconcile(
                    &generation,
                    SubmissionObservation::CandidatePending,
                    Some(diagnostic.clone()),
                    12,
                )
                .unwrap();
            assert_eq!(tracking.diagnostic(), Some(&diagnostic));
            generation
        };
        {
            let db = open_prepared(&path);
            let store = SqliteChainSubmissionStore::new(Arc::clone(&db));
            let StoreAdmission::Ready { record, .. } = store
                .admit(&StoreAdvancementRequest::vote(identity()), true, 1, 13)
                .unwrap()
            else {
                panic!("tracking must reconcile")
            };
            assert_eq!(record.tracking_started_at(), Some(11));
            assert_eq!(
                record.diagnostic().map(ChainSubmissionDiagnostic::message),
                Some("candidate lookup temporarily unavailable")
            );
            let committed = committed();
            let result = store
                .confirm_committed(
                    &StoreAdvancementRequest::vote(identity()),
                    &generation,
                    candidate,
                    &committed,
                    &|| true,
                    14,
                )
                .unwrap();
            assert!(matches!(result, ConfirmationCommit::Confirmed(_)));
            let expected_hash = candidate.to_string();
            assert_eq!(
                db.get_vote_tx_hash(ROUND, 0, 1).unwrap().as_deref(),
                Some(expected_hash.as_str())
            );
        }
        {
            let db = open_prepared(&path);
            let store = SqliteChainSubmissionStore::new(db);
            let StoreAdmission::Authoritative(record) = store
                .admit(&StoreAdvancementRequest::vote(identity()), true, 1, 15)
                .unwrap()
            else {
                panic!("terminal state must be authoritative")
            };
            assert_eq!(record.durable_state(), ChainSubmissionState::Confirmed);
        }
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn confirmed_predecessor_allows_the_next_bundle_generation() {
        let db = open_prepared(":memory:");
        let store = SqliteChainSubmissionStore::new(Arc::clone(&db));
        let request = StoreAdvancementRequest::vote(identity());
        let StoreAdmission::Ready { derived, .. } = store.admit(&request, true, 1, 10).unwrap()
        else {
            panic!("fresh admission")
        };
        let candidate = CandidateTransactionHash::from_bytes([0x44; 32]);
        store
            .classify_post(
                derived.generation(),
                SubmissionObservation::UsableCandidateHash(candidate),
                11,
            )
            .unwrap();
        store
            .confirm_committed(
                &request,
                derived.generation(),
                candidate,
                &committed(),
                &|| true,
                12,
            )
            .unwrap();

        crate::vote::insert_recovery_fixture(&db, &recovery_for(0, 2)).unwrap();
        assert!(matches!(
            store
                .admit(
                    &StoreAdvancementRequest::vote(identity_for(0, 2)),
                    true,
                    1,
                    13,
                )
                .unwrap(),
            StoreAdmission::Ready {
                fresh_reservation: true,
                ..
            }
        ));
    }

    #[test]
    fn obsolete_migration_delegation_guard_does_not_block_a_confirmed_successor() {
        let db = open_prepared(":memory:");
        let positions = [vec![1, 0, 0, 0, 1], 8_u64.to_be_bytes().to_vec()].concat();
        db.conn()
            .execute(
                "INSERT INTO chain_submissions
                   (identity_key, round_id, wallet_id, network, vote_chain_id,
                    bundle_index, kind, proposal_id, generation_digest, state,
                    committed_post_reservations, diagnostic_kind, diagnostic,
                    created_at, updated_at)
                 VALUES (?1, ?2, 'wallet', 'testnet', NULL, 0,
                         'delegation', NULL, NULL, 'recovering', 0,
                         'recovery_unavailable', 'legacy delegation evidence', 9, 9)",
                rusqlite::params![vec![0x74_u8; 32], ROUND],
            )
            .unwrap();
        db.conn()
            .execute(
                "INSERT INTO chain_submissions
                   (identity_key, round_id, wallet_id, network, vote_chain_id,
                    bundle_index, kind, proposal_id, generation_digest, state,
                    committed_post_reservations, confirmation_source,
                    final_van_position, vote_commitment_positions, created_at, updated_at)
                 VALUES (?1, ?2, 'wallet', 'testnet', NULL, 0, 'vote', 1, NULL,
                         'legacy_confirmed', 0, 'legacy_projection', 7, ?3, 9, 9)",
                rusqlite::params![vec![0x75_u8; 32], ROUND, positions],
            )
            .unwrap();
        crate::vote::insert_recovery_fixture(&db, &recovery_for(0, 2)).unwrap();
        let store = SqliteChainSubmissionStore::new(db);

        assert!(matches!(
            store
                .admit(
                    &StoreAdvancementRequest::vote(identity_for(0, 2)),
                    true,
                    1,
                    10,
                )
                .unwrap(),
            StoreAdmission::Ready {
                fresh_reservation: true,
                ..
            }
        ));
    }

    #[test]
    fn digestless_predecessor_guard_blocks_unknown_successor_work() {
        let db = open_prepared(":memory:");
        crate::vote::insert_recovery_fixture(&db, &recovery_for(0, 2)).unwrap();
        db.conn()
            .execute(
                "INSERT INTO chain_submissions
                 (identity_key, round_id, wallet_id, network, vote_chain_id, bundle_index,
                  kind, proposal_id, generation_digest, state, committed_post_reservations,
                  diagnostic_kind, diagnostic, created_at, updated_at)
                 VALUES (?1, ?2, 'wallet', 'testnet', NULL, 0, 'vote', 1, NULL,
                         'recovering', 0, 'recovery_unavailable',
                         'legacy generation cannot be reconstructed', 10, 10)",
                rusqlite::params![vec![0x31_u8; 32], ROUND],
            )
            .unwrap();
        let store = SqliteChainSubmissionStore::new(db);

        let failure = match store.admit(
            &StoreAdvancementRequest::vote(identity_for(0, 2)),
            true,
            1,
            11,
        ) {
            Err(failure) => failure,
            Ok(_) => panic!("unknown legacy successor must block later bundle work"),
        };
        assert_eq!(failure.kind(), ChainSubmissionFailureKind::InvalidInput);
    }

    #[test]
    fn stale_batch_roster_is_rejected_before_unrelated_member_guard() {
        let db = open_prepared(":memory:");
        let digest = store_two_vote_batch(&db);
        db.conn()
            .execute(
                "INSERT INTO chain_submissions
                 (identity_key, round_id, wallet_id, network, vote_chain_id, bundle_index,
                  kind, proposal_id, generation_digest, state, committed_post_reservations,
                  diagnostic_kind, diagnostic, created_at, updated_at)
                 VALUES (?1, ?2, 'wallet', 'testnet', NULL, 0, 'vote', 3, NULL,
                         'recovering', 0, 'recovery_unavailable',
                         'unrelated legacy generation', 10, 10)",
                rusqlite::params![vec![0x39_u8; 32], ROUND],
            )
            .unwrap();
        let store = SqliteChainSubmissionStore::new(db);
        let request =
            StoreAdvancementRequest::vote_batch(batch_identity(digest), vec![1, 3]).unwrap();

        let failure = store
            .admit(&request, true, 1, 11)
            .err()
            .expect("the stale caller roster must be rejected");

        assert_eq!(failure.kind(), ChainSubmissionFailureKind::InvalidInput);
        assert!(failure
            .message()
            .contains("does not match the complete persisted batch"));
        assert!(failure.strongest_state().is_none());
    }

    #[test]
    fn active_predecessor_blocks_successor_across_vote_chain_ids() {
        let db = open_prepared(":memory:");
        crate::vote::insert_recovery_fixture(&db, &recovery_for(0, 2)).unwrap();
        let store = SqliteChainSubmissionStore::new(db);
        let first = StoreAdvancementRequest::vote(identity_for_chain("vote-chain-1", 0, 1));
        assert!(matches!(
            store.admit(&first, true, 1, 10).unwrap(),
            StoreAdmission::Ready {
                fresh_reservation: true,
                ..
            }
        ));

        let failure = match store.admit(
            &StoreAdvancementRequest::vote(identity_for_chain("vote-chain-2", 0, 2)),
            true,
            1,
            11,
        ) {
            Err(failure) => failure,
            Ok(_) => panic!("the bundle predecessor is independent of vote-chain configuration"),
        };
        assert_eq!(failure.kind(), ChainSubmissionFailureKind::InvalidInput);
    }

    #[test]
    fn duplicate_candidate_hash_becomes_hashless_recovery() {
        let db = open_prepared(":memory:");
        queries::insert_bundle(&db.conn(), ROUND, "wallet", 1, &[2]).unwrap();
        crate::vote::insert_recovery_fixture(&db, &recovery_for(1, 2)).unwrap();
        let store = SqliteChainSubmissionStore::new(db);
        let candidate = CandidateTransactionHash::from_bytes([0x55; 32]);

        let StoreAdmission::Ready { derived: first, .. } = store
            .admit(&StoreAdvancementRequest::vote(identity()), true, 1, 10)
            .unwrap()
        else {
            panic!("first admission")
        };
        store
            .classify_post(
                first.generation(),
                SubmissionObservation::UsableCandidateHash(candidate),
                11,
            )
            .unwrap();

        let StoreAdmission::Ready {
            derived: second, ..
        } = store
            .admit(
                &StoreAdvancementRequest::vote(identity_for(1, 2)),
                true,
                1,
                12,
            )
            .unwrap()
        else {
            panic!("second admission")
        };
        let record = store
            .classify_post(
                second.generation(),
                SubmissionObservation::UsableCandidateHash(candidate),
                13,
            )
            .unwrap();
        assert!(matches!(
            record.state(),
            SubmissionRecordState::Recovering {
                candidate_transaction_hash: None,
                ambiguity_diagnostic,
            } if ambiguity_diagnostic.kind() == ChainSubmissionDiagnosticKind::InvalidProtocolResponse
        ));
    }

    #[test]
    fn lifecycle_timestamps_clamp_when_wall_clock_moves_backward() {
        let db = open_prepared(":memory:");
        let store = SqliteChainSubmissionStore::new(db);
        let StoreAdmission::Ready { derived, .. } = store
            .admit(&StoreAdvancementRequest::vote(identity()), true, 1, 100)
            .unwrap()
        else {
            panic!("fresh admission")
        };
        let candidate = CandidateTransactionHash::from_bytes([0x66; 32]);
        let tracking = store
            .classify_post(
                derived.generation(),
                SubmissionObservation::UsableCandidateHash(candidate),
                90,
            )
            .unwrap();
        assert_eq!(tracking.tracking_started_at(), Some(100));
        assert_eq!(tracking.updated_at(), 100);

        let recovering = store
            .reconcile(
                derived.generation(),
                SubmissionObservation::TrackingWindowExpired(
                    ChainSubmissionDiagnostic::from_redacted_message(
                        ChainSubmissionDiagnosticKind::TrackingWindowExpired,
                        "clock rollback expires tracking conservatively",
                    ),
                ),
                None,
                80,
            )
            .unwrap();
        assert_eq!(recovering.durable_state(), ChainSubmissionState::Recovering);
        assert_eq!(recovering.updated_at(), 100);
    }

    #[test]
    fn failed_abandoned_normalization_reports_possible_dispatch() {
        let db = open_prepared(":memory:");
        let store = SqliteChainSubmissionStore::new(Arc::clone(&db));
        store
            .admit(&StoreAdvancementRequest::vote(identity()), true, 1, 10)
            .unwrap();
        db.conn()
            .execute_batch(
                "CREATE TRIGGER fail_chain_submission_update
                 BEFORE UPDATE ON chain_submissions
                 BEGIN SELECT RAISE(ABORT, 'injected normalization failure'); END;",
            )
            .unwrap();

        let failure = match store.admit(&StoreAdvancementRequest::vote(identity()), true, 1, 20) {
            Err(failure) => failure,
            Ok(_) => panic!("normalization must fail"),
        };
        assert_eq!(
            failure.strongest_state().unwrap().evidence(),
            crate::chain_submission::ChainSubmissionStateEvidence::KnownPossiblyDispatched
        );
        let state: String = db
            .conn()
            .query_row("SELECT state FROM chain_submissions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(state, "submitting");
    }
}

fn storage_error(error: rusqlite::Error) -> ChainSubmissionFailure {
    ChainSubmissionFailure::without_state(ChainSubmissionFailureKind::Storage, error.to_string())
}

fn possible_dispatch_error(error: ChainSubmissionFailure) -> ChainSubmissionFailure {
    ChainSubmissionFailure::with_known_possible_dispatch(error.kind(), error.message())
}

fn network_name(network: crate::Network) -> &'static str {
    match network {
        crate::Network::Mainnet => "mainnet",
        crate::Network::Testnet => "testnet",
        crate::Network::Regtest => "regtest",
    }
}

fn native_identity_key(identity: &ChainSubmissionIdentity) -> Vec<u8> {
    let mut key = b"zcash_voting.chain_submission.identity.v1\0".to_vec();
    for value in [
        identity.wallet_id().as_bytes(),
        network_name(identity.network()).as_bytes(),
        identity.vote_chain_id().as_bytes(),
        identity.vote_round_id(),
    ] {
        key.extend_from_slice(&(value.len() as u64).to_be_bytes());
        key.extend_from_slice(value);
    }
    key.extend_from_slice(&identity.bundle_index().to_be_bytes());
    match identity.target() {
        ChainSubmissionTarget::Delegation => key.push(0),
        ChainSubmissionTarget::Vote { proposal_id } => {
            key.push(1);
            key.extend_from_slice(&proposal_id.to_be_bytes());
        }
        ChainSubmissionTarget::VoteBatch {
            ordered_batch_digest,
        } => {
            key.push(2);
            key.extend_from_slice(&ordered_batch_digest);
        }
    }
    key
}

fn derive(
    tx: &Transaction<'_>,
    request: &SubmissionDerivationRequest,
) -> Result<DerivedChainSubmission, ChainSubmissionFailure> {
    match request {
        SubmissionDerivationRequest::Delegation { identity, signer } => {
            let signer = match signer {
                crate::delegate::DelegationSigner::Signature { sig, sighash } => {
                    crate::delegate::DelegationSigner::Signature {
                        sig: *sig,
                        sighash: *sighash,
                    }
                }
            };
            derive_delegation(tx, identity, signer)
        }
        SubmissionDerivationRequest::Vote { identity } => derive_vote(tx, identity),
        SubmissionDerivationRequest::VoteBatch { identity } => derive_vote_batch(tx, identity),
    }
    .map_err(map_generation_error)
}

fn load_guard(
    tx: &Transaction<'_>,
    identity: &ChainSubmissionIdentity,
) -> Result<Option<StoredChainSubmission>, ChainSubmissionFailure> {
    let (kind, proposal): (&str, Option<u32>) = match identity.target() {
        ChainSubmissionTarget::Delegation => ("delegation", None),
        ChainSubmissionTarget::Vote { proposal_id } => ("vote", Some(proposal_id)),
        ChainSubmissionTarget::VoteBatch { .. } => return Ok(None),
    };
    load_one(
        tx,
        "SELECT identity_key, generation_digest, state, candidate_transaction_hash,
                committed_post_reservations, tracking_started_at, diagnostic_kind,
                diagnostic, confirmation_source, confirmed_transaction_hash,
                final_van_position, vote_commitment_positions, created_at, updated_at
           FROM chain_submissions
          WHERE wallet_id = :wallet AND network = :network AND round_id = :round
            AND bundle_index = :bundle AND kind = :kind
            AND proposal_id IS :proposal
            AND vote_chain_id IS NULL",
        named_params! {
            ":wallet": identity.wallet_id(),
            ":network": network_name(identity.network()),
            ":round": hex::encode(identity.vote_round_id()),
            ":bundle": identity.bundle_index(),
            ":proposal": proposal,
            ":kind": kind,
        },
        identity,
    )
}

fn load_native(
    tx: &Transaction<'_>,
    identity: &ChainSubmissionIdentity,
) -> Result<Option<StoredChainSubmission>, ChainSubmissionFailure> {
    load_one(
        tx,
        "SELECT identity_key, generation_digest, state, candidate_transaction_hash,
                committed_post_reservations, tracking_started_at, diagnostic_kind,
                diagnostic, confirmation_source, confirmed_transaction_hash,
                final_van_position, vote_commitment_positions, created_at, updated_at
           FROM chain_submissions WHERE identity_key = :key",
        named_params! { ":key": native_identity_key(identity) },
        identity,
    )
}

fn load_one<P: rusqlite::Params>(
    tx: &Transaction<'_>,
    sql: &str,
    params: P,
    identity: &ChainSubmissionIdentity,
) -> Result<Option<StoredChainSubmission>, ChainSubmissionFailure> {
    tx.query_row(sql, params, |row| {
        let digest: Option<Vec<u8>> = row.get(1)?;
        let candidate: Option<Vec<u8>> = row.get(3)?;
        let attempts: i64 = row.get(4)?;
        let tracking: Option<i64> = row.get(5)?;
        let diagnostic_kind: Option<String> = row.get(6)?;
        let diagnostic_message: Option<String> = row.get(7)?;
        let source: Option<String> = row.get(8)?;
        let confirmed_hash: Option<Vec<u8>> = row.get(9)?;
        let final_van: Option<i64> = row.get(10)?;
        let positions: Option<Vec<u8>> = row.get(11)?;
        let diagnostic = match (diagnostic_kind.as_deref(), diagnostic_message) {
            (Some(kind), Some(message)) => Some(ChainSubmissionDiagnostic::from_redacted_message(
                parse_diagnostic_kind(kind)?,
                message,
            )),
            (None, None) => None,
            _ => return Err(rusqlite::Error::InvalidQuery),
        };
        let candidate = candidate
            .map(bytes32)
            .transpose()?
            .map(CandidateTransactionHash::from_bytes);
        let confirmation = if let Some(source) = source.as_deref() {
            let final_van = u64::try_from(final_van.ok_or(rusqlite::Error::InvalidQuery)?)
                .map_err(|_| rusqlite::Error::InvalidQuery)?;
            let positions = decode_positions(&positions.ok_or(rusqlite::Error::InvalidQuery)?)?;
            let hash = confirmed_hash
                .map(bytes32)
                .transpose()?
                .map(CandidateTransactionHash::from_bytes);
            Some((source, final_van, positions, hash))
        } else {
            None
        };
        let state_name: String = row.get(2)?;
        let state = match state_name.as_str() {
            "submitting" => SubmissionRecordState::Submitting,
            "tracking" => SubmissionRecordState::Tracking {
                candidate_transaction_hash: candidate.ok_or(rusqlite::Error::InvalidQuery)?,
            },
            "recovering" if digest.is_none() => {
                SubmissionRecordState::DigestlessRecoveryGuard(DigestlessRecoveryGuard::new())
            }
            "recovering" => SubmissionRecordState::Recovering {
                candidate_transaction_hash: candidate,
                ambiguity_diagnostic: diagnostic.clone().ok_or(rusqlite::Error::InvalidQuery)?,
            },
            "confirmed" => {
                let (source, final_van, positions, hash) =
                    confirmation.ok_or(rusqlite::Error::InvalidQuery)?;
                let value = match source {
                    "hash" => ValidatedChainSubmissionConfirmation::from_hash(
                        hash.ok_or(rusqlite::Error::InvalidQuery)?,
                        final_van,
                        positions,
                    ),
                    "tree" => ValidatedChainSubmissionConfirmation::from_tree(final_van, positions),
                    "legacy_import" => ValidatedChainSubmissionConfirmation::from_legacy_import(
                        hash, final_van, positions,
                    ),
                    _ => return Err(rusqlite::Error::InvalidQuery),
                }
                .map_err(|_| rusqlite::Error::InvalidQuery)?;
                SubmissionRecordState::Confirmed(value)
            }
            "legacy_confirmed" => {
                let (_, final_van, positions, _) =
                    confirmation.ok_or(rusqlite::Error::InvalidQuery)?;
                if positions.len() != 1 {
                    return Err(rusqlite::Error::InvalidQuery);
                }
                SubmissionRecordState::LegacyConfirmed(
                    LegacyProjectionConfirmation::from_positions(final_van, positions[0])
                        .map_err(|_| rusqlite::Error::InvalidQuery)?,
                )
            }
            "rejected" => SubmissionRecordState::Rejected(
                diagnostic.clone().ok_or(rusqlite::Error::InvalidQuery)?,
            ),
            _ => return Err(rusqlite::Error::InvalidQuery),
        };
        Ok(StoredChainSubmission {
            identity: identity.clone(),
            generation_digest: digest
                .map(bytes32)
                .transpose()?
                .map(ChainSubmissionGenerationDigest::from_bytes),
            state,
            committed_post_reservations: u64::try_from(attempts)
                .map_err(|_| rusqlite::Error::InvalidQuery)?,
            tracking_started_at: tracking
                .map(u64::try_from)
                .transpose()
                .map_err(|_| rusqlite::Error::InvalidQuery)?,
            diagnostic,
            created_at: u64::try_from(row.get::<_, i64>(12)?)
                .map_err(|_| rusqlite::Error::InvalidQuery)?,
            updated_at: u64::try_from(row.get::<_, i64>(13)?)
                .map_err(|_| rusqlite::Error::InvalidQuery)?,
        })
    })
    .optional()
    .map_err(storage_error)
}

fn bytes32(value: Vec<u8>) -> rusqlite::Result<[u8; 32]> {
    value.try_into().map_err(|_| rusqlite::Error::InvalidQuery)
}

fn decode_positions(encoded: &[u8]) -> rusqlite::Result<Vec<u64>> {
    if encoded.len() < 5 || encoded[0] != 1 {
        return Err(rusqlite::Error::InvalidQuery);
    }
    let count = u32::from_be_bytes(encoded[1..5].try_into().unwrap()) as usize;
    if encoded.len() != 5 + count * 8 {
        return Err(rusqlite::Error::InvalidQuery);
    }
    Ok(encoded[5..]
        .chunks_exact(8)
        .map(|chunk| u64::from_be_bytes(chunk.try_into().unwrap()))
        .collect())
}

fn encode_positions(positions: &[u64]) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(5 + positions.len() * 8);
    encoded.push(1);
    encoded.extend_from_slice(&(positions.len() as u32).to_be_bytes());
    for position in positions {
        encoded.extend_from_slice(&position.to_be_bytes());
    }
    encoded
}

fn parse_diagnostic_kind(value: &str) -> rusqlite::Result<ChainSubmissionDiagnosticKind> {
    match value {
        "ambiguous_dispatch" => Ok(ChainSubmissionDiagnosticKind::AmbiguousDispatch),
        "tracking_window_expired" => Ok(ChainSubmissionDiagnosticKind::TrackingWindowExpired),
        "chain_rejected" => Ok(ChainSubmissionDiagnosticKind::ChainRejected),
        "reconciliation_pending" => Ok(ChainSubmissionDiagnosticKind::ReconciliationPending),
        "invalid_protocol_response" => Ok(ChainSubmissionDiagnosticKind::InvalidProtocolResponse),
        "recovery_unavailable" => Ok(ChainSubmissionDiagnosticKind::RecoveryUnavailable),
        "storage_failure" => Ok(ChainSubmissionDiagnosticKind::StorageFailure),
        _ => Err(rusqlite::Error::InvalidQuery),
    }
}

fn diagnostic_name(value: ChainSubmissionDiagnosticKind) -> &'static str {
    match value {
        ChainSubmissionDiagnosticKind::AmbiguousDispatch => "ambiguous_dispatch",
        ChainSubmissionDiagnosticKind::TrackingWindowExpired => "tracking_window_expired",
        ChainSubmissionDiagnosticKind::ChainRejected => "chain_rejected",
        ChainSubmissionDiagnosticKind::ReconciliationPending => "reconciliation_pending",
        ChainSubmissionDiagnosticKind::InvalidProtocolResponse => "invalid_protocol_response",
        ChainSubmissionDiagnosticKind::RecoveryUnavailable => "recovery_unavailable",
        ChainSubmissionDiagnosticKind::StorageFailure => "storage_failure",
    }
}

fn insert_fresh(
    tx: &Transaction<'_>,
    record: &StoredChainSubmission,
) -> Result<(), ChainSubmissionFailure> {
    let identity = record.identity();
    let (kind, proposal, batch): (&str, Option<u32>, Option<Vec<u8>>) = match identity.target() {
        ChainSubmissionTarget::Delegation => ("delegation", None, None),
        ChainSubmissionTarget::Vote { proposal_id } => ("vote", Some(proposal_id), None),
        ChainSubmissionTarget::VoteBatch {
            ordered_batch_digest,
        } => ("vote_batch", None, Some(ordered_batch_digest.to_vec())),
    };
    tx.execute(
        "INSERT INTO chain_submissions
         (identity_key, round_id, wallet_id, network, vote_chain_id, bundle_index, kind,
          proposal_id, ordered_batch_digest, generation_digest, state,
          committed_post_reservations, created_at, updated_at)
         VALUES (:key, :round, :wallet, :network, :chain, :bundle, :kind, :proposal,
                 :batch, :digest, 'submitting', :attempts, :created, :created)",
        named_params! {
            ":key": native_identity_key(identity), ":round": hex::encode(identity.vote_round_id()),
            ":wallet": identity.wallet_id(), ":network": network_name(identity.network()),
            ":chain": identity.vote_chain_id(), ":bundle": identity.bundle_index(), ":kind": kind,
            ":proposal": proposal, ":batch": batch,
            ":digest": record.generation_digest().unwrap().as_bytes(),
            ":attempts": record.committed_post_reservations(), ":created": record.created_at(),
        },
    )
    .map_err(storage_error)?;
    Ok(())
}

fn persist_mutable(
    tx: &Transaction<'_>,
    record: &StoredChainSubmission,
) -> Result<(), ChainSubmissionFailure> {
    let (state, candidate, diagnostic, confirmation) = match record.state() {
        SubmissionRecordState::Submitting => ("submitting", None, None, None),
        SubmissionRecordState::Tracking {
            candidate_transaction_hash,
        } => (
            "tracking",
            Some(*candidate_transaction_hash),
            record.diagnostic(),
            None,
        ),
        SubmissionRecordState::Recovering {
            candidate_transaction_hash,
            ambiguity_diagnostic,
        } => (
            "recovering",
            *candidate_transaction_hash,
            Some(ambiguity_diagnostic),
            None,
        ),
        SubmissionRecordState::DigestlessRecoveryGuard(_) => {
            return Err(ChainSubmissionFailure::with_durable_state(
                ChainSubmissionFailureKind::InvariantViolation,
                ChainSubmissionState::Recovering,
                "migration guards are immutable",
            ))
        }
        SubmissionRecordState::Confirmed(value) => (
            "confirmed",
            value.confirmation().transaction_hash(),
            None,
            Some(value.confirmation()),
        ),
        SubmissionRecordState::LegacyConfirmed(_) => {
            return Err(ChainSubmissionFailure::with_durable_state(
                ChainSubmissionFailureKind::InvariantViolation,
                ChainSubmissionState::LegacyConfirmed,
                "legacy confirmations are immutable",
            ))
        }
        SubmissionRecordState::Rejected(value) => ("rejected", None, Some(value), None),
    };
    let (source, confirmed_hash, final_van, positions) =
        confirmation.map_or((None, None, None, None), |value| {
            let source = match value.source() {
                crate::chain_submission::ChainSubmissionConfirmationSource::Hash => "hash",
                crate::chain_submission::ChainSubmissionConfirmationSource::Tree => "tree",
                crate::chain_submission::ChainSubmissionConfirmationSource::LegacyImport => {
                    "legacy_import"
                }
                crate::chain_submission::ChainSubmissionConfirmationSource::LegacyProjection => {
                    unreachable!()
                }
            };
            (
                Some(source),
                value.transaction_hash(),
                Some(value.final_van_position()),
                Some(encode_positions(value.vote_commitment_positions())),
            )
        });
    tx.execute(
        "UPDATE chain_submissions SET state=:state, candidate_transaction_hash=:candidate,
          committed_post_reservations=:attempts, tracking_started_at=:tracking,
          diagnostic_kind=:diagnostic_kind, diagnostic=:diagnostic,
          confirmation_source=:source, confirmed_transaction_hash=:confirmed_hash,
          final_van_position=:final_van, vote_commitment_positions=:positions, updated_at=:updated
          WHERE identity_key=:key",
        named_params! {
            ":state": state, ":candidate": candidate.map(|v| v.as_bytes().to_vec()),
            ":attempts": record.committed_post_reservations(), ":tracking": record.tracking_started_at(),
            ":diagnostic_kind": diagnostic.map(|v| diagnostic_name(v.kind())),
            ":diagnostic": diagnostic.map(ChainSubmissionDiagnostic::message),
            ":source": source, ":confirmed_hash": confirmed_hash.map(|v| v.as_bytes().to_vec()),
            ":final_van": final_van, ":positions": positions, ":updated": record.updated_at(),
            ":key": native_identity_key(record.identity()),
        },
    ).map_err(storage_error)?;
    Ok(())
}

fn normalize_abandoned(
    tx: &Transaction<'_>,
    mut record: StoredChainSubmission,
    now: u64,
) -> Result<StoredChainSubmission, ChainSubmissionFailure> {
    record.state = apply_submission_observation(
        Some(record.state),
        SubmissionObservation::AbandonedSubmitting(abandoned_diagnostic()),
    )
    .map_err(|error| {
        possible_dispatch_error(transition_failure(ChainSubmissionState::Submitting, error))
    })?
    .ok_or_else(|| {
        possible_dispatch_error(transition_failure(
            ChainSubmissionState::Submitting,
            "abandoned reservation normalization removed the durable row",
        ))
    })?;
    record.diagnostic = match &record.state {
        SubmissionRecordState::Recovering {
            ambiguity_diagnostic,
            ..
        } => Some(ambiguity_diagnostic.clone()),
        _ => None,
    };
    record.updated_at = now.max(record.updated_at);
    persist_mutable(tx, &record).map_err(possible_dispatch_error)?;
    Ok(record)
}

impl ChainSubmissionStore for SqliteChainSubmissionStore {
    fn coordination(&self) -> &SubmissionCoordination {
        &self.db.chain_submission_coordination
    }

    fn admit(
        &self,
        request: &StoreAdvancementRequest,
        work_allowed: bool,
        reservation_ordinal: u64,
        now: u64,
    ) -> Result<StoreAdmission, ChainSubmissionFailure> {
        request.validate_target()?;
        let mut normalizes_abandoned_reservation = false;
        let result = self.transact(|tx| {
            if let Some(guard) = load_guard(tx, request.identity())? {
                return Ok(StoreAdmission::Authoritative(guard));
            }
            if !request.is_batch() {
                for identity in &request.legacy_identities {
                    if let Some(guard) = load_guard(tx, identity)? {
                        return Ok(StoreAdmission::Authoritative(guard));
                    }
                }
            }
            let existing = load_native(tx, request.identity())?;
            if request.is_batch() && !work_allowed {
                for identity in &request.legacy_identities {
                    if let Some(guard) = load_guard(tx, identity)? {
                        return Err(ChainSubmissionFailure::with_durable_state(
                            ChainSubmissionFailureKind::InvalidInput,
                            guard.durable_state(),
                            "atomic vote batch overlaps a migration-only singleton guard",
                        ));
                    }
                }
            }
            if !work_allowed {
                return match existing {
                    Some(record)
                        if matches!(record.state(), SubmissionRecordState::Submitting) =>
                    {
                        normalizes_abandoned_reservation = true;
                        Ok(StoreAdmission::Authoritative(normalize_abandoned(
                            tx, record, now,
                        )?))
                    }
                    Some(record) => Ok(StoreAdmission::Authoritative(record)),
                    None => Ok(StoreAdmission::NoAuthoritativeState),
                };
            }
            if existing
                .as_ref()
                .is_some_and(|record| matches!(record.state(), SubmissionRecordState::Submitting))
            {
                normalizes_abandoned_reservation = true;
                return Ok(StoreAdmission::Authoritative(normalize_abandoned(
                    tx,
                    existing.expect("submitting record was just observed"),
                    now,
                )?));
            }
            let mut batch_derived = if request.is_batch() {
                let derived = derive(tx, request.derivation())
                    .map_err(|error| preserve_loaded_state(error, existing.as_ref()))?;
                request
                    .verify_batch_roster(derived.ordered_proposal_ids())
                    .map_err(|error| preserve_loaded_state(error, existing.as_ref()))?;
                for proposal_id in derived.ordered_proposal_ids() {
                    let identity = request.singleton_identity(*proposal_id)?;
                    if let Some(guard) = load_guard(tx, &identity)? {
                        return Err(ChainSubmissionFailure::with_durable_state(
                            ChainSubmissionFailureKind::InvalidInput,
                            guard.durable_state(),
                            "atomic vote batch overlaps a migration-only singleton guard",
                        ));
                    }
                }
                Some(derived)
            } else {
                None
            };
            if let Some(record) = existing {
                let derived = match batch_derived.take() {
                    Some(derived) => derived,
                    None => derive(tx, request.derivation())
                        .map_err(|error| preserve_loaded_state(error, Some(&record)))?,
                };
                request.verify_batch_roster(derived.ordered_proposal_ids()).map_err(|error| preserve_loaded_state(error, Some(&record)))?;
                ensure_generation(&record, derived.generation())?;
                if matches!(record.state(), SubmissionRecordState::Confirmed(_) | SubmissionRecordState::Rejected(_)) {
                    return Ok(StoreAdmission::Authoritative(record));
                }
                return Ok(StoreAdmission::Ready { derived: Box::new(derived), record, fresh_reservation: false });
            }
            let predecessor: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM chain_submissions WHERE wallet_id=:wallet AND network=:network
                  AND round_id=:round AND bundle_index=:bundle
                  AND state IN ('submitting','tracking','recovering')
                  AND NOT (vote_chain_id IS NULL AND kind='delegation'
                           AND EXISTS(SELECT 1 FROM chain_submissions successor
                                       WHERE successor.wallet_id=chain_submissions.wallet_id
                                         AND successor.network=chain_submissions.network
                                         AND successor.round_id=chain_submissions.round_id
                                         AND successor.bundle_index=chain_submissions.bundle_index
                                         AND successor.vote_chain_id IS NULL
                                         AND successor.kind='vote'
                                         AND successor.state='legacy_confirmed'))) ",
                named_params! { ":wallet": request.identity().wallet_id(), ":network": network_name(request.identity().network()),
                    ":round": hex::encode(request.identity().vote_round_id()), ":bundle": request.identity().bundle_index() },
                |row| row.get(0),
            ).map_err(storage_error)?;
            if predecessor { return Err(ChainSubmissionFailure::without_state(ChainSubmissionFailureKind::InvalidInput, "another submission for this bundle has not established an authoritative successor")); }
            let derived = match batch_derived {
                Some(derived) => derived,
                None => derive(tx, request.derivation())?,
            };
            request.verify_batch_roster(derived.ordered_proposal_ids())?;
            let record = StoredChainSubmission::fresh(derived.generation(), reservation_ordinal, now);
            insert_fresh(tx, &record)?;
            Ok(StoreAdmission::Ready { derived: Box::new(derived), record, fresh_reservation: true })
        });
        result.map_err(|error| {
            if normalizes_abandoned_reservation {
                possible_dispatch_error(error)
            } else {
                error
            }
        })
    }

    fn remove_fresh_reservation(
        &self,
        generation: &ChainSubmissionGeneration,
        _now: u64,
    ) -> Result<(), ChainSubmissionFailure> {
        self.transact(|tx| {
            let record = load_native(tx, generation.identity())?.ok_or_else(|| {
                ChainSubmissionFailure::without_state(
                    ChainSubmissionFailureKind::InvariantViolation,
                    "fresh reservation disappeared",
                )
            })?;
            ensure_generation(&record, generation)?;
            if !matches!(record.state(), SubmissionRecordState::Submitting) {
                return Err(ChainSubmissionFailure::with_durable_state(
                    ChainSubmissionFailureKind::InvariantViolation,
                    record.durable_state(),
                    "only a fresh Submitting reservation can be removed",
                ));
            }
            tx.execute(
                "DELETE FROM chain_submissions WHERE identity_key=?1",
                [native_identity_key(generation.identity())],
            )
            .map_err(storage_error)?;
            Ok(())
        })
    }

    fn classify_post(
        &self,
        generation: &ChainSubmissionGeneration,
        observation: SubmissionObservation,
        now: u64,
    ) -> Result<StoredChainSubmission, ChainSubmissionFailure> {
        self.apply_observation(generation, observation, None, now)
    }

    fn reconcile(
        &self,
        generation: &ChainSubmissionGeneration,
        observation: SubmissionObservation,
        diagnostic: Option<ChainSubmissionDiagnostic>,
        now: u64,
    ) -> Result<StoredChainSubmission, ChainSubmissionFailure> {
        self.apply_observation(generation, observation, diagnostic, now)
    }

    fn confirm_committed(
        &self,
        request: &StoreAdvancementRequest,
        expected_generation: &ChainSubmissionGeneration,
        candidate: CandidateTransactionHash,
        committed: &crate::chain_submission::protocol::CommittedTransaction,
        commit_allowed: &dyn Fn() -> bool,
        now: u64,
    ) -> Result<ConfirmationCommit, ChainSubmissionFailure> {
        self.transact(|tx| {
            let mut record = load_native(tx, expected_generation.identity())?.ok_or_else(|| {
                ChainSubmissionFailure::without_state(
                    ChainSubmissionFailureKind::InvariantViolation,
                    "submission disappeared before confirmation",
                )
            })?;
            ensure_generation(&record, expected_generation)?;
            let derived = derive(tx, request.derivation())
                .map_err(|error| preserve_loaded_state(error, Some(&record)))?;
            if derived.generation() != expected_generation {
                return Err(ChainSubmissionFailure::with_durable_state(
                    ChainSubmissionFailureKind::InvalidInput,
                    record.durable_state(),
                    "semantic generation changed before confirmation",
                ));
            }
            if !commit_allowed() {
                return Ok(ConfirmationCommit::Interrupted(record));
            }
            let confirmation = validate_hash_confirmation(&derived, candidate, &committed.events)
                .map_err(|error| {
                ChainSubmissionFailure::with_durable_state(
                    ChainSubmissionFailureKind::Protocol,
                    record.durable_state(),
                    error.to_string(),
                )
            })?;
            let previous = record.durable_state();
            record.state = apply_submission_observation(
                Some(record.state),
                SubmissionObservation::Confirmed(confirmation.clone()),
            )
            .map_err(|error| transition_failure(previous, error))?
            .unwrap();
            record.diagnostic = None;
            record.updated_at = now.max(record.updated_at);
            apply_confirmed_generation(tx, &derived, &confirmation)
                .map_err(map_generation_error)?;
            persist_mutable(tx, &record)?;
            Ok(ConfirmationCommit::Confirmed(record))
        })
    }
}

impl SqliteChainSubmissionStore {
    fn apply_observation(
        &self,
        generation: &ChainSubmissionGeneration,
        observation: SubmissionObservation,
        diagnostic: Option<ChainSubmissionDiagnostic>,
        now: u64,
    ) -> Result<StoredChainSubmission, ChainSubmissionFailure> {
        self.transact(|tx| {
            let mut record = load_native(tx, generation.identity())?.ok_or_else(|| {
                ChainSubmissionFailure::without_state(
                    ChainSubmissionFailureKind::InvariantViolation,
                    "submission disappeared during lifecycle transition",
                )
            })?;
            ensure_generation(&record, generation)?;
            let previous = record.durable_state();
            let observation = match observation {
                SubmissionObservation::UsableCandidateHash(candidate) => {
                    let owned_elsewhere: bool = tx
                        .query_row(
                            "SELECT EXISTS(SELECT 1 FROM chain_submissions
                              WHERE identity_key != :key
                                AND (candidate_transaction_hash = :candidate
                                     OR confirmed_transaction_hash = :candidate))",
                            named_params! {
                                ":key": native_identity_key(generation.identity()),
                                ":candidate": candidate.as_bytes(),
                            },
                            |row| row.get(0),
                        )
                        .map_err(storage_error)?;
                    if owned_elsewhere {
                        SubmissionObservation::PossiblyDispatched(
                            ChainSubmissionDiagnostic::from_redacted_message(
                                ChainSubmissionDiagnosticKind::InvalidProtocolResponse,
                                "vote-chain returned a transaction hash owned by another semantic generation",
                            ),
                        )
                    } else {
                        SubmissionObservation::UsableCandidateHash(candidate)
                    }
                }
                observation => observation,
            };
            record.state = apply_submission_observation(Some(record.state), observation)
                .map_err(|error| transition_failure(previous, error))?
                .ok_or_else(|| {
                    transition_failure(previous, "transition unexpectedly removed durable row")
                })?;
            if let Some(value) = diagnostic {
                record.diagnostic = Some(value);
            }
            record.diagnostic = match &record.state {
                SubmissionRecordState::Recovering {
                    ambiguity_diagnostic,
                    ..
                }
                | SubmissionRecordState::Rejected(ambiguity_diagnostic) => {
                    Some(ambiguity_diagnostic.clone())
                }
                _ => record.diagnostic,
            };
            let effective_now = now.max(record.updated_at);
            if matches!(record.state(), SubmissionRecordState::Tracking { .. })
                && record.tracking_started_at.is_none()
            {
                record.tracking_started_at = Some(effective_now);
            }
            record.updated_at = effective_now;
            persist_mutable(tx, &record)?;
            Ok(record)
        })
    }
}
