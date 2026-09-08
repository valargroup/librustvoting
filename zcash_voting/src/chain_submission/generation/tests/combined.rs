use std::sync::{Arc, Mutex};

use super::{
    derive_vote_batch,
    tests::{persisted_delegation, recovery},
};
use crate::{chain_submission::*, round::VotingDb, vote::VoteBatchRecovery};

const ROUND: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn fixture(count: u32) -> (Arc<VotingDb>, AdvanceVoteBatch) {
    let (db, _, signature) = persisted_delegation();
    let authorization = crate::delegate_and_vote_batch::DelegationAuthorization::capture(
        &db.conn(),
        "wallet-1",
        ROUND,
        0,
        signature,
    )
    .unwrap();
    let mut votes: Vec<_> = (1..=count)
        .map(|proposal| {
            let mut vote = recovery(proposal);
            vote.anchor_height = 0;
            vote.vc_tree_position = 0;
            vote
        })
        .collect();
    let actions = votes
        .iter()
        .map(|vote| crate::vote_commitment::CastVoteBatchSighashAction {
            r_vpk: &vote.r_vpk,
            van_nullifier: &vote.van_nullifier,
            vote_authority_note_new: &vote.vote_authority_note_new,
            vote_commitment: &vote.vote_commitment,
            proposal_id: vote.proposal_id,
        })
        .collect::<Vec<_>>();
    let digest = crate::delegate_and_vote_batch::delegate_and_vote_batch_sighash(
        &[0x11; 32],
        &authorization.van,
        &actions,
    )
    .unwrap();
    for (index, vote) in votes.iter_mut().enumerate() {
        vote.batch = Some(VoteBatchRecovery {
            delegation_van: Some(authorization.van),
            digest,
            index: index as u32,
            size: count,
        });
        crate::vote::insert_recovery_fixture(&db, vote).unwrap();
    }
    authorization.persist(&db.conn(), &digest).unwrap();
    (
        Arc::new(db),
        AdvanceVoteBatch {
            vote_round_id: [0x11; 32],
            bundle_index: 0,
            ordered_batch_digest: digest,
            ordered_proposal_ids: (1..=count).collect(),
        },
    )
}

fn event(request: &AdvanceVoteBatch) -> serde_json::Value {
    let attributes = [
        ("round_id", ROUND.to_owned()),
        (
            "nullifiers",
            crate::governance::BUNDLE_NOTE_SLOTS.to_string(),
        ),
        ("batch_digest", hex::encode(request.ordered_batch_digest)),
        ("batch_size", request.ordered_proposal_ids.len().to_string()),
        ("final_van_leaf_index", "7".to_owned()),
        (
            "vote_commitment_leaf_indices",
            (0..request.ordered_proposal_ids.len())
                .map(|index| (8 + index).to_string())
                .collect::<Vec<_>>()
                .join(","),
        ),
        (
            "proposal_ids",
            request
                .ordered_proposal_ids
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(","),
        ),
        (
            "van_nullifiers",
            vec![hex::encode([0x10; 32]); request.ordered_proposal_ids.len()].join(","),
        ),
    ];
    serde_json::json!({"type":"delegate_and_cast_vote_batch", "attributes": attributes.into_iter().map(|(key,value)| serde_json::json!({"key":key,"value":value})).collect::<Vec<_>>()})
}

struct Transport {
    request: AdvanceVoteBatch,
    calls: Mutex<Vec<String>>,
}

impl ChainTransport for Transport {
    fn chain_get<'a>(&'a self, request: ChainHttpRequest) -> ChainTransportFuture<'a> {
        let first_poll = {
            let mut calls = self.calls.lock().unwrap();
            calls.push(request.url().to_owned());
            calls.len() == 2
        };
        if first_poll {
            return Box::pin(async {
                Ok(ChainHttpResponse::json(
                    404,
                    br#"{"error":"tx not found"}"#.to_vec(),
                ))
            });
        }
        Box::pin(async move {
            Ok(ChainHttpResponse::json(200, serde_json::to_vec(&serde_json::json!({"height":"10","code":0,"log":"","events":[event(&self.request)]})).unwrap()))
        })
    }
    fn chain_post_json<'a>(
        &'a self,
        request: ChainHttpRequest,
        json: Vec<u8>,
    ) -> ChainTransportFuture<'a> {
        self.calls.lock().unwrap().push(request.url().to_owned());
        assert!(request.url().ends_with("/delegate-and-cast-vote-batch"));
        let wire: crate::wire::DelegateAndVoteBatchWire = serde_json::from_slice(&json).unwrap();
        assert_eq!(
            wire.authorization_digest().unwrap(),
            self.request.ordered_batch_digest
        );
        Box::pin(async move {
            Ok(ChainHttpResponse::json(200, serde_json::to_vec(&serde_json::json!({"tx_hash":HASH,"code":0,"batch_digest":hex::encode(self.request.ordered_batch_digest)})).unwrap()))
        })
    }
}

#[tokio::test]
async fn combined_lifecycle_confirms_delegation_and_every_vote_together() {
    for count in [1, 2, crate::vote::MAX_VOTE_BATCH_ACTIONS as u32] {
        let (db, request) = fixture(count);
        let transport = Arc::new(Transport {
            request: request.clone(),
            calls: Mutex::new(Vec::new()),
        });
        let client = ChainSubmissionClient::with_transport(
            db.clone(),
            transport.clone(),
            ChainSubmissionClientConfig::for_network(
                crate::Network::Testnet,
                vec!["https://vote.example".to_owned()],
            ),
        )
        .unwrap();
        let control = ChainSubmissionControl::new(1);
        let submitted = client
            .advance_delegate_and_vote_batch(
                request.clone(),
                ChainRecoveryMode::StatusOnly,
                &control,
            )
            .await
            .unwrap();
        assert!(
            matches!(submitted, ChainSubmissionResult::Pending(_)),
            "{submitted:?}"
        );
        // The round snapshot must accept an in-flight combined hash and
        // project it to both of the obligations the transaction owns.
        let hashes = crate::chain_submission::planning::lifecycle_transaction_hashes(
            &db.conn(),
            "wallet-1",
            ROUND,
        )
        .unwrap();
        for target in [
            crate::chain_submission::planning::PlanningTarget::Delegation,
            crate::chain_submission::planning::PlanningTarget::VoteBatch {
                ordered_batch_digest: request.ordered_batch_digest,
            },
        ] {
            assert_eq!(hashes.hash(0, target), Some(HASH.to_owned()));
        }
        assert_eq!(
            db.delegation_phase(ROUND, 0).unwrap(),
            crate::phases::DelegationPhase::SubmissionManaged
        );
        for proposal in 1..=count {
            assert_eq!(
                db.vote_phase(ROUND, 0, proposal).unwrap(),
                crate::phases::VotePhase::SubmissionManaged
            );
        }
        let confirmed = client
            .advance_delegate_and_vote_batch(
                request.clone(),
                ChainRecoveryMode::StatusOnly,
                &control,
            )
            .await
            .unwrap();
        assert!(matches!(confirmed, ChainSubmissionResult::Confirmed(_)));
        assert_eq!(
            db.delegation_phase(ROUND, 0).unwrap(),
            crate::phases::DelegationPhase::Confirmed
        );
        for proposal in 1..=count {
            assert_eq!(
                db.vote_phase(ROUND, 0, proposal).unwrap(),
                crate::phases::VotePhase::Confirmed
            );
            assert_eq!(
                crate::vote::recovery_bundle(&db, ROUND, 0, proposal)
                    .unwrap()
                    .unwrap()
                    .vc_tree_position,
                7 + u64::from(proposal)
            );
        }
        client
            .advance_delegate_and_vote_batch(request, ChainRecoveryMode::StatusOnly, &control)
            .await
            .unwrap();
        assert_eq!(
            transport.calls.lock().unwrap().len(),
            3,
            "replayed confirmation must not dispatch"
        );
        let rows: u32 = db
            .conn()
            .query_row("SELECT count(*) FROM chain_submissions", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(rows, 1);
    }
}

#[test]
fn ordinary_batch_identity_cannot_dispatch_combined_signatures() {
    let (db, request) = fixture(2);
    let identity = ChainSubmissionIdentity::new(
        "wallet-1",
        crate::Network::Testnet,
        [0x11; 32],
        0,
        ChainSubmissionTarget::VoteBatch {
            ordered_batch_digest: request.ordered_batch_digest,
        },
    )
    .unwrap();
    assert!(derive_vote_batch(&db.conn(), &identity).is_err());
    let recovered = crate::vote::recover_atomic_vote_batch(&db, ROUND, 0, 1).unwrap();
    assert!(matches!(
        recovered.advance_request().unwrap(),
        ChainAdvanceRequest::DelegateAndVoteBatch(_)
    ));
    assert!(
        serde_json::from_str::<crate::wire::DelegateAndVoteBatchWire>(&recovered.batch_json)
            .is_ok()
    );
}

#[test]
fn repeated_combined_preparation_directs_callers_to_recovery_without_writes() {
    use crate::delegate_and_vote_batch::{
        prepare_delegate_and_vote_batch, recover_delegate_and_vote_batch,
        DelegateAndVoteBatchRequest,
    };
    use crate::vote::{DraftVote, VoteSigner};

    for count in [1, 2] {
        let (db, _) = fixture(count);
        let before = recover_delegate_and_vote_batch(&db, ROUND, 0, 1).unwrap();
        let signature: Vec<u8> = db
            .conn()
            .query_row(
                "SELECT spend_auth_signature FROM delegate_cast_recovery",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let hotkey =
            crate::VotingHotkey::from_stored_secret(&[0x43; 64], crate::Network::Testnet).unwrap();
        let drafts = (1..=count)
            .map(|proposal_id| DraftVote {
                proposal_id,
                choice: 2,
                num_options: 3,
                single_share: false,
                vc_tree_position: 0,
            })
            .collect::<Vec<_>>();
        let error = prepare_delegate_and_vote_batch(
            &db,
            VoteSigner::hotkey(&hotkey),
            DelegateAndVoteBatchRequest {
                round_id: ROUND,
                bundle_index: 0,
                drafts: &drafts,
                spend_auth_signature: signature.try_into().unwrap(),
                stages: &crate::types::NoopProgressReporter,
                max_proof_concurrency: 1,
            },
        )
        .err()
        .expect("preparation must direct callers to recovery");
        assert!(matches!(error, crate::VotingError::InvalidInput { .. }));
        assert!(
            error.to_string().contains(
                "combined batch is already persisted; use recover_delegate_and_vote_batch"
            ),
            "{error}"
        );
        let after = recover_delegate_and_vote_batch(&db, ROUND, 0, count).unwrap();
        assert_eq!(after.batch_json, before.batch_json);
        assert_eq!(after.batch_digest, before.batch_digest);
        let submissions: u32 = db
            .conn()
            .query_row("SELECT count(*) FROM chain_submissions", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(submissions, 0);
    }
}

#[test]
fn changed_delegation_generation_refuses_combined_recovery() {
    let (db, request) = fixture(2);
    db.conn()
        .execute("UPDATE proofs SET proof = ?1", [vec![0x22; 96]])
        .unwrap();
    let identity = ChainSubmissionIdentity::new(
        "wallet-1",
        crate::Network::Testnet,
        [0x11; 32],
        0,
        ChainSubmissionTarget::DelegateAndVoteBatch {
            ordered_batch_digest: request.ordered_batch_digest,
        },
    )
    .unwrap();
    assert!(derive_vote_batch(&db.conn(), &identity).is_err());
}

#[tokio::test]
async fn persisted_combined_authorization_excludes_a_standalone_delegation() {
    let (db, _) = fixture(2);
    let (_, _, signature) = persisted_delegation();
    let transport = Arc::new(Transport {
        request: fixture(1).1,
        calls: Mutex::new(Vec::new()),
    });
    let client = ChainSubmissionClient::with_transport(
        db.clone(),
        transport.clone(),
        ChainSubmissionClientConfig::for_network(
            crate::Network::Testnet,
            vec!["https://vote.example".to_owned()],
        ),
    )
    .unwrap();
    let request = AdvanceDelegation {
        vote_round_id: [0x11; 32],
        bundle_index: 0,
        spend_auth_signature: signature,
    };
    assert!(client
        .advance_delegation(request, &ChainSubmissionControl::new(1))
        .await
        .is_err());
    assert!(transport.calls.lock().unwrap().is_empty());
}

#[test]
fn retiring_combined_members_removes_their_authorization() {
    let (db, _) = fixture(2);
    let retired = crate::vote::retire_undispatched_votes_outside_roster_with_conn(
        &db.conn(),
        "wallet-1",
        ROUND,
        0,
        &[],
    )
    .unwrap();
    assert_eq!(retired, vec![1, 2]);
    let count: u32 = db
        .conn()
        .query_row("SELECT count(*) FROM delegate_cast_recovery", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count, 0);
}

/// Real ZKP2 proofs exercise the synthetic delegation output and successor
/// chain. The delegation fixture carries a verified SpendAuth signature; the
/// independent ZKP1 proof test covers its circuit and proving key.
#[test]
#[ignore = "generates two real ZKP2 proofs; run make proofs"]
fn combined_batch_builds_real_proofs_and_binds_the_initial_delegation_van() {
    use crate::backend::orchard::primitives::redpallas::{Signature, SpendAuth, VerificationKey};
    use crate::backend::pasta_curves::{group::Group, group::GroupEncoding, pallas};
    use crate::vote::{DraftVote, VoteSigner};
    let (db, _, signature) = persisted_delegation();
    let hotkey =
        crate::VotingHotkey::from_stored_secret(&[0x43; 64], crate::Network::Testnet).unwrap();
    db.conn()
        .execute(
            "UPDATE bundles SET total_note_value=?1",
            [crate::governance::BALLOT_DIVISOR * 2],
        )
        .unwrap();
    db.conn()
        .execute(
            "UPDATE rounds SET ea_pk=?1",
            [pallas::Point::generator().to_bytes().to_vec()],
        )
        .unwrap();
    let state =
        crate::storage::queries::load_zkp2_inputs(&db.conn(), ROUND, "wallet-1", 0).unwrap();
    let transition = crate::zkp2::plan_vote_authority_transition(
        hotkey.stored_secret(),
        crate::Network::Testnet,
        state.address_index,
        state.total_note_value,
        &state.gov_comm_rand,
        &[0x11; 32],
        1,
        state.proposal_authority,
    )
    .unwrap();
    let drafts = [1, 2].map(|proposal_id| DraftVote {
        proposal_id,
        choice: 0,
        num_options: 2,
        single_share: true,
        vc_tree_position: 0,
    });
    let prepare = || {
        crate::delegate_and_vote_batch::prepare_delegate_and_vote_batch(
            &db,
            VoteSigner::hotkey(&hotkey),
            crate::delegate_and_vote_batch::DelegateAndVoteBatchRequest {
                round_id: ROUND,
                bundle_index: 0,
                drafts: &drafts,
                spend_auth_signature: signature,
                stages: &crate::types::NoopProgressReporter,
                max_proof_concurrency: 1,
            },
        )
    };
    // The validly signed fixture initially names a different VAN. Refuse that
    // cross-circuit mismatch before spending time generating proofs.
    assert!(prepare().is_err());
    db.conn()
        .execute(
            "UPDATE bundles SET gov_comm=?1",
            [transition.vote_authority_note_old.to_vec()],
        )
        .unwrap();
    let prepared = prepare().unwrap();
    let signed =
        crate::delegate_and_vote_batch::persist_delegate_and_vote_batch(&db, prepared).unwrap();
    assert_eq!(signed.commitments.len(), 2);
    for vote in &signed.commitments {
        assert_eq!(vote.anchor_height, 0);
        assert!(!vote.proof.is_empty());
        let key = VerificationKey::<SpendAuth>::try_from(vote.r_vpk).unwrap();
        let signature = Signature::<SpendAuth>::from(vote.vote_auth_sig);
        key.verify(&signed.batch_digest, &signature).unwrap();
        assert!(key.verify(&[0; 32], &signature).is_err());
    }
    let recovered =
        crate::delegate_and_vote_batch::recover_delegate_and_vote_batch(&db, ROUND, 0, 2).unwrap();
    assert_eq!(recovered.batch_json, signed.batch_json);
}

#[test]
fn combined_confirmation_rejects_wrong_kind_partial_roster_and_wrong_layout() {
    let (db, request) = fixture(2);
    let identity = ChainSubmissionIdentity::new(
        "wallet-1",
        crate::Network::Testnet,
        [0x11; 32],
        0,
        ChainSubmissionTarget::DelegateAndVoteBatch {
            ordered_batch_digest: request.ordered_batch_digest,
        },
    )
    .unwrap();
    let derived = derive_vote_batch(&db.conn(), &identity).unwrap();
    let valid: crate::confirmation::TxEvent = serde_json::from_value(event(&request)).unwrap();
    let hash = CandidateTransactionHash::from_bytes([0xaa; 32]);
    assert!(
        crate::chain_submission::confirmation::validate_hash_confirmation(
            &derived,
            hash,
            std::slice::from_ref(&valid)
        )
        .is_ok()
    );
    for (field, replacement) in [
        ("batch_digest", hex::encode([0; 32])),
        ("batch_size", "1".into()),
        ("nullifiers", "0".into()),
        ("proposal_ids", "2,1".into()),
        ("van_nullifiers", hex::encode([0x10; 32])),
        ("vote_commitment_leaf_indices", "8,10".into()),
        ("round_id", hex::encode([0x12; 32])),
    ] {
        let mut wrong = valid.clone();
        wrong
            .attributes
            .iter_mut()
            .find(|attribute| attribute.key == field)
            .unwrap()
            .value = replacement;
        assert!(
            crate::chain_submission::confirmation::validate_hash_confirmation(
                &derived,
                hash,
                &[wrong]
            )
            .is_err(),
            "accepted wrong {field}"
        );
    }
    let mut ordinary = valid;
    ordinary.event_type = "cast_vote_batch".to_owned();
    assert!(
        crate::chain_submission::confirmation::validate_hash_confirmation(
            &derived,
            hash,
            &[ordinary]
        )
        .is_err()
    );
}

#[tokio::test]
async fn a_combined_confirmation_storage_failure_rolls_back_every_projection() {
    let (db, request) = fixture(2);
    let transport = Arc::new(Transport {
        request: request.clone(),
        calls: Mutex::new(Vec::new()),
    });
    let client = ChainSubmissionClient::with_transport(
        db.clone(),
        transport,
        ChainSubmissionClientConfig::for_network(
            crate::Network::Testnet,
            vec!["https://vote.example".to_owned()],
        ),
    )
    .unwrap();
    let control = ChainSubmissionControl::new(1);
    client
        .advance_delegate_and_vote_batch(request.clone(), ChainRecoveryMode::StatusOnly, &control)
        .await
        .unwrap();
    db.conn().execute_batch("CREATE TRIGGER fail_second_vote BEFORE UPDATE OF vc_tree_position ON votes WHEN NEW.proposal_id=2 BEGIN SELECT RAISE(ABORT, 'test confirmation failure'); END;").unwrap();
    assert!(client
        .advance_delegate_and_vote_batch(request, ChainRecoveryMode::StatusOnly, &control)
        .await
        .is_err());
    let positions: u32 = db
        .conn()
        .query_row(
            "SELECT count(*) FROM votes WHERE vc_tree_position IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(positions, 0);
    let advanced: bool = db.conn().query_row("SELECT delegation_tx_hash IS NOT NULL OR van_leaf_position IS NOT NULL FROM bundles WHERE bundle_index=0", [], |row| row.get(0)).unwrap();
    assert!(!advanced);
    assert_eq!(
        db.delegation_phase(ROUND, 0).unwrap(),
        crate::phases::DelegationPhase::SubmissionManaged
    );
}

#[test]
fn combined_authorization_and_envelope_survive_database_reopen() {
    let (db, _) = fixture(2);
    let before =
        crate::delegate_and_vote_batch::recover_delegate_and_vote_batch(&db, ROUND, 0, 1).unwrap();
    let path = std::env::temp_dir().join(format!(
        "combined-recovery-{}.sqlite",
        rand::random::<u64>()
    ));
    db.conn()
        .execute("VACUUM INTO ?1", [path.to_str().unwrap()])
        .unwrap();
    drop(db);
    let reopened = VotingDb::open(path.to_str().unwrap()).unwrap();
    reopened.set_wallet_id("wallet-1");
    let after =
        crate::delegate_and_vote_batch::recover_delegate_and_vote_batch(&reopened, ROUND, 0, 2)
            .unwrap();
    assert_eq!(after.batch_json, before.batch_json);
    assert_eq!(after.batch_digest, before.batch_digest);
    drop(reopened);
    std::fs::remove_file(path).unwrap();
}
