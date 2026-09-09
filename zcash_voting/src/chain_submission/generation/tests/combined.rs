use std::sync::{Arc, Mutex};

use super::{
    derive_vote_batch,
    tests::{persisted_delegation, recovery},
};
use crate::{chain_submission::*, round::VotingDb, vote::VoteBatchRecovery};

const ROUND: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

#[path = "combined/helper_delivery.rs"]
mod helper_delivery;

#[path = "combined/recovery_boundaries.rs"]
mod recovery_boundaries;

fn fixture(count: u32) -> (Arc<VotingDb>, AdvanceVoteBatch) {
    let (db, _, signature) = persisted_delegation();
    let db = Arc::new(db);
    let request = persist_combined_batch(&db, signature, count, 0);
    (db, request)
}

/// Persists a signed combined batch of `count` members over the fixture's
/// delegation. `variant` perturbs every vote commitment so two batches over
/// the same delegation get distinct digests.
fn persist_combined_batch(
    db: &VotingDb,
    signature: [u8; 64],
    count: u32,
    variant: u8,
) -> AdvanceVoteBatch {
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
            vote.vote_commitment[31] = variant;
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
        crate::vote::insert_recovery_fixture(db, vote).unwrap();
    }
    authorization.persist(&db.conn(), &digest).unwrap();
    AdvanceVoteBatch {
        vote_round_id: [0x11; 32],
        bundle_index: 0,
        ordered_batch_digest: digest,
        ordered_proposal_ids: (1..=count).collect(),
    }
}

fn event(request: &AdvanceVoteBatch) -> serde_json::Value {
    let attributes = [
        ("round_id", ROUND.to_owned()),
        (
            "nullifier_count",
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
    for count in [1, 2, 37, crate::vote::MAX_VOTE_BATCH_ACTIONS as u32] {
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

/// An already-submitted 37-question ballot must recover by its saved hash
/// after restart, without another POST or access to the delegation signer.
#[tokio::test]
async fn combined_chain_receipt_recovers_after_reopen_without_resubmission() {
    let (db, request) = fixture(37);
    let transport = Arc::new(Transport {
        request: request.clone(),
        calls: Mutex::new(Vec::new()),
    });
    let config = || {
        ChainSubmissionClientConfig::for_network(
            crate::Network::Testnet,
            vec!["https://vote.example".to_owned()],
        )
    };
    let client =
        ChainSubmissionClient::with_transport(db.clone(), transport.clone(), config()).unwrap();
    let control = ChainSubmissionControl::new(1);
    let pending = client
        .advance_delegate_and_vote_batch(request.clone(), ChainRecoveryMode::StatusOnly, &control)
        .await
        .unwrap();
    assert!(matches!(pending, ChainSubmissionResult::Pending(_)));
    // A malformed committed receipt must leave the saved hash available to
    // a later client. It is not permission to POST the batch again.
    struct MissingCountReceipt(Arc<Transport>);
    impl ChainTransport for MissingCountReceipt {
        fn chain_get<'a>(&'a self, request: ChainHttpRequest) -> ChainTransportFuture<'a> {
            Box::pin(async move {
                let response = self.0.chain_get(request).await?;
                let mut receipt: serde_json::Value =
                    serde_json::from_slice(response.body()).unwrap();
                receipt["events"][0]["attributes"]
                    .as_array_mut()
                    .unwrap()
                    .retain(|attribute| attribute["key"] != "nullifier_count");
                Ok(ChainHttpResponse::json(
                    200,
                    serde_json::to_vec(&receipt).unwrap(),
                ))
            })
        }
        fn chain_post_json<'a>(
            &'a self,
            _: ChainHttpRequest,
            _: Vec<u8>,
        ) -> ChainTransportFuture<'a> {
            panic!("a saved candidate must be polled, never resubmitted");
        }
    }
    let malformed_client = ChainSubmissionClient::with_transport(
        db.clone(),
        Arc::new(MissingCountReceipt(transport.clone())),
        config(),
    )
    .unwrap();
    let error = malformed_client
        .advance_delegate_and_vote_batch(request.clone(), ChainRecoveryMode::StatusOnly, &control)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("nullifier_count"));
    assert_eq!(
        db.delegation_phase(ROUND, 0).unwrap(),
        crate::phases::DelegationPhase::SubmissionManaged
    );
    for proposal in 1..=37 {
        assert_eq!(
            db.vote_phase(ROUND, 0, proposal).unwrap(),
            crate::phases::VotePhase::SubmissionManaged
        );
    }
    drop(malformed_client);
    let path = std::env::temp_dir().join(format!(
        "combined-confirmation-reopen-{}.sqlite",
        rand::random::<u64>(),
    ));
    db.conn()
        .execute("VACUUM INTO ?1", [path.to_str().unwrap()])
        .unwrap();
    drop(client);
    drop(db);
    let reopened = Arc::new(VotingDb::open(path.to_str().unwrap()).unwrap());
    reopened.set_wallet_id("wallet-1");
    let resumed =
        ChainSubmissionClient::with_transport(reopened.clone(), transport.clone(), config())
            .unwrap();
    let confirmed = resumed
        .advance_delegate_and_vote_batch(request.clone(), ChainRecoveryMode::StatusOnly, &control)
        .await
        .unwrap();
    assert!(matches!(confirmed, ChainSubmissionResult::Confirmed(_)));
    assert_eq!(
        reopened.delegation_phase(ROUND, 0).unwrap(),
        crate::phases::DelegationPhase::Confirmed
    );
    for proposal in 1..=37 {
        assert_eq!(
            reopened.vote_phase(ROUND, 0, proposal).unwrap(),
            crate::phases::VotePhase::Confirmed
        );
    }
    resumed
        .advance_delegate_and_vote_batch(request, ChainRecoveryMode::StatusOnly, &control)
        .await
        .unwrap();
    let calls = transport.calls.lock().unwrap();
    assert_eq!(
        calls.len(),
        4,
        "one POST, pending and malformed polls, then one recovery poll"
    );
    assert_eq!(
        calls
            .iter()
            .filter(|url| url.ends_with("/delegate-and-cast-vote-batch"))
            .count(),
        1
    );
    assert!(calls[3].ends_with(HASH));
    drop(calls);
    drop(resumed);
    drop(reopened);
    std::fs::remove_file(path).unwrap();
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

#[test]
fn changing_a_combined_member_removes_the_batch_authorization() {
    let (db, _) = fixture(2);

    crate::vote::invalidate_unsubmitted_vote_recoveries_for_intent(
        &db.conn(),
        "wallet-1",
        ROUND,
        2,
        Some(1),
    )
    .unwrap();

    assert_eq!(count(&db, "delegate_cast_recovery"), 0);
    for proposal_id in 1..=2 {
        assert!(crate::vote::recovery_bundle(&db, ROUND, 0, proposal_id)
            .unwrap()
            .is_none());
    }
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
        ("nullifier_count", "0".into()),
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

/// The deployed chain emits one `nullifier_count`; malformed counts cannot
/// confirm a locally locked generation, even when every other event field matches.
#[test]
fn combined_confirmation_requires_one_matching_chain_nullifier_count() {
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
    let validate = |event: &crate::confirmation::TxEvent| {
        crate::chain_submission::confirmation::validate_hash_confirmation(
            &derived,
            hash,
            std::slice::from_ref(event),
        )
    };
    assert!(validate(&valid).is_ok());
    for count in [
        "",
        "0",
        "4",
        "6",
        "-1",
        "5.0",
        "abc",
        "18446744073709551616",
    ] {
        let mut malformed = valid.clone();
        malformed
            .attributes
            .iter_mut()
            .find(|attribute| attribute.key == "nullifier_count")
            .unwrap()
            .value = count.to_owned();
        assert!(validate(&malformed).is_err(), "accepted count {count:?}");
    }
    let mut missing = valid.clone();
    missing
        .attributes
        .retain(|attribute| attribute.key != "nullifier_count");
    assert!(validate(&missing).is_err());
    let mut legacy = valid.clone();
    legacy
        .attributes
        .iter_mut()
        .find(|attribute| attribute.key == "nullifier_count")
        .unwrap()
        .key = "nullifiers".to_owned();
    assert!(
        validate(&legacy).is_err(),
        "accepted the old, unsupported field name"
    );
    for count in ["5", "4"] {
        let mut duplicate = valid.clone();
        let mut attribute = duplicate
            .attributes
            .iter()
            .find(|attribute| attribute.key == "nullifier_count")
            .unwrap()
            .clone();
        attribute.value = count.to_owned();
        duplicate.attributes.push(attribute);
        assert!(
            validate(&duplicate).is_err(),
            "accepted duplicate count {count}"
        );
    }
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

/// The fixture's delegation signature, read back from its persisted authorization.
fn stored_signature(db: &VotingDb) -> [u8; 64] {
    let signature: Vec<u8> = db
        .conn()
        .query_row(
            "SELECT spend_auth_signature FROM delegate_cast_recovery LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    signature.try_into().unwrap()
}

fn count(db: &VotingDb, table: &str) -> u32 {
    db.conn()
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
}

/// What the retirement must leave behind: no lifecycle row, no members, no
/// authorization, no helper records, and the delegation setup intact.
fn assert_retired_to_proved(db: &VotingDb, proposals: u32) {
    assert_eq!(count(db, "chain_submissions"), 0);
    assert_eq!(count(db, "delegate_cast_recovery"), 0);
    assert_eq!(count(db, "share_delegations"), 0);
    assert_eq!(count(db, "helper_share_plans"), 0);
    for proposal in 1..=proposals {
        assert!(crate::vote::recovery_bundle(db, ROUND, 0, proposal)
            .unwrap()
            .is_none());
    }
    assert_eq!(count(db, "proofs"), 1, "the delegation proof is reused");
    let (sighash, tx_hash, position): (Option<Vec<u8>>, Option<String>, Option<i64>) = db
        .conn()
        .query_row(
            "SELECT pczt_sighash, delegation_tx_hash, van_leaf_position FROM bundles",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert!(
        sighash.is_some(),
        "the PCZT sighash the signature covers stays"
    );
    assert!(tx_hash.is_none() && position.is_none());
    assert_eq!(
        db.delegation_phase(ROUND, 0).unwrap(),
        crate::phases::DelegationPhase::Proved
    );
}

/// The bundle's recorded rejection streak, with the chain's last message.
fn rejection_ledger(db: &VotingDb) -> Option<(u32, Vec<u8>, String)> {
    db.conn()
        .query_row(
            "SELECT consecutive_rejections, delegation_generation_digest, last_diagnostic
               FROM combined_cast_rejections",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)? as u32,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .ok()
}

#[tokio::test]
async fn consecutive_combined_rejections_accumulate_against_one_delegation_generation() {
    // Retirement deletes every other durable trace of the rejection, so the
    // ledger is the only thing that can tell a first failure from a delegation
    // the chain keeps refusing. It counts the generation each recast reuses,
    // not the batch digest, which is re-randomized on every cast.
    let (db, request) = fixture(2);
    let signature = stored_signature(&db);
    let reject_once = |request: &AdvanceVoteBatch| {
        Arc::new(ScriptedPosts {
            inner: Transport {
                request: request.clone(),
                calls: Mutex::new(Vec::new()),
            },
            posts: Mutex::new(vec![rejection(7, request)]),
        })
    };
    let control = ChainSubmissionControl::new(1);

    client_over(&db, reject_once(&request))
        .advance_delegate_and_vote_batch(request.clone(), ChainRecoveryMode::StatusOnly, &control)
        .await
        .unwrap();
    let (streak, generation, message) = rejection_ledger(&db).expect("a rejection is recorded");
    assert_eq!(streak, 1);
    assert!(message.contains("code 7"), "{message}");
    assert!(message.contains("round closed"), "{message}");

    // A fresh batch over the same untouched delegation setup.
    let recast = persist_combined_batch(&db, signature, 2, 1);
    assert_ne!(recast.ordered_batch_digest, request.ordered_batch_digest);
    client_over(&db, reject_once(&recast))
        .advance_delegate_and_vote_batch(recast.clone(), ChainRecoveryMode::StatusOnly, &control)
        .await
        .unwrap();

    let (streak, same_generation, _) = rejection_ledger(&db).expect("the streak survives");
    assert_eq!(
        streak, 2,
        "a different batch digest over the same delegation continues the streak"
    );
    assert_eq!(
        same_generation, generation,
        "the delegation generation is what the streak is counted against"
    );
    assert_retired_to_proved(&db, 2);

    // At the cap the wallet stops planning the cast on its own, and the
    // chain's own words survive the deleted lifecycle row.
    assert!(
        blocked_bundles(&db).contains_key(&0),
        "the second rejection reaches the cap"
    );

    // The block is advisory: the host's explicit retry lifts it, leaving the
    // delegation setup untouched so the next cast reuses the same signature.
    assert!(db.retry_blocked_combined_cast(ROUND, 0).unwrap());
    assert!(
        blocked_bundles(&db).is_empty(),
        "the host cleared the block"
    );
    assert_eq!(
        db.delegation_phase(ROUND, 0).unwrap(),
        crate::phases::DelegationPhase::Proved,
        "clearing a block touches no delegation setup"
    );
}

/// Rejects `request` once through a scripted transport.
async fn reject_combined_once(db: &Arc<VotingDb>, request: &AdvanceVoteBatch) {
    let transport = Arc::new(ScriptedPosts {
        inner: Transport {
            request: request.clone(),
            calls: Mutex::new(Vec::new()),
        },
        posts: Mutex::new(vec![rejection(7, request)]),
    });
    client_over(db, transport)
        .advance_delegate_and_vote_batch(
            request.clone(),
            ChainRecoveryMode::StatusOnly,
            &ChainSubmissionControl::new(1),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn changing_the_ballot_lifts_a_block_the_delegation_did_not_cause() {
    // The streak is counted against the delegation generation, which a ballot
    // edit leaves untouched. Nothing else would ever lift a block whose cause
    // was a vote member: re-proving reuses the same delegation, and the
    // retirement that deletes every other durable trace keeps the vote rows,
    // so the discard's own cleanup cannot reach the row either.
    let (db, request) = fixture(2);
    // Captured before the first rejection, which retires the recovery row it
    // is read from.
    let signature = stored_signature(&db);
    db.set_ballot_intent(ROUND, 1, crate::session::Decision::Choice(2), 3)
        .unwrap();

    reject_combined_once(&db, &request).await;
    let recast = persist_combined_batch(&db, signature, 2, 1);
    reject_combined_once(&db, &recast).await;

    assert_eq!(rejection_ledger(&db).expect("a streak").0, 2);
    assert!(
        blocked_bundles(&db).contains_key(&0),
        "the cap is reached before the ballot changes"
    );

    // The voter answers proposal 1 differently. The next batch is not the one
    // the chain refused, so the streak stops describing it.
    db.set_ballot_intent(ROUND, 1, crate::session::Decision::Choice(1), 3)
        .unwrap();

    assert!(
        rejection_ledger(&db).is_none(),
        "a changed ballot clears the streak it no longer describes"
    );
    assert!(
        blocked_bundles(&db).is_empty(),
        "planning offers the cast again"
    );
    assert_eq!(
        db.delegation_phase(ROUND, 0).unwrap(),
        crate::phases::DelegationPhase::Proved,
        "lifting the block touches no delegation setup"
    );
}

#[tokio::test]
async fn an_unchanged_ballot_decision_keeps_the_streak() {
    // Re-asserting the same decision changes nothing about what will be sent,
    // so it must not launder a block away.
    let (db, request) = fixture(2);
    db.set_ballot_intent(ROUND, 1, crate::session::Decision::Choice(2), 3)
        .unwrap();
    reject_combined_once(&db, &request).await;
    let before = rejection_ledger(&db).expect("a streak");

    db.set_ballot_intent(ROUND, 1, crate::session::Decision::Choice(2), 3)
        .unwrap();

    assert_eq!(
        rejection_ledger(&db).expect("the streak survives"),
        before,
        "an idempotent intent write is not a ballot change"
    );
}

/// The bundles planning refuses to cast for, with the chain's last rejection.
fn blocked_bundles(
    db: &VotingDb,
) -> std::collections::BTreeMap<u32, crate::ChainSubmissionDiagnostic> {
    let mut conn = db.conn();
    let tx = conn.transaction().unwrap();
    crate::round_planning::load_round_snapshot(&tx, "wallet-1", ROUND)
        .unwrap()
        .rejection_blocked_bundles
}

#[tokio::test]
async fn a_confirmed_combined_batch_forgets_its_rejection_streak() {
    // The streak means "this delegation keeps being refused". A batch that
    // lands disproves that, so a later unrelated rejection starts from one.
    let (db, request) = fixture(2);
    let signature = stored_signature(&db);
    let transport = Arc::new(ScriptedPosts {
        inner: Transport {
            request: request.clone(),
            calls: Mutex::new(Vec::new()),
        },
        posts: Mutex::new(vec![rejection(7, &request)]),
    });
    let control = ChainSubmissionControl::new(1);
    client_over(&db, transport)
        .advance_delegate_and_vote_batch(request.clone(), ChainRecoveryMode::StatusOnly, &control)
        .await
        .unwrap();
    assert!(rejection_ledger(&db).is_some());

    let fresh = persist_combined_batch(&db, signature, 2, 1);
    let accepting = Arc::new(Transport {
        request: fresh.clone(),
        calls: Mutex::new(Vec::new()),
    });
    let client = client_over(&db, accepting);
    client
        .advance_delegate_and_vote_batch(fresh.clone(), ChainRecoveryMode::StatusOnly, &control)
        .await
        .unwrap();
    let confirmed = client
        .advance_delegate_and_vote_batch(fresh, ChainRecoveryMode::StatusOnly, &control)
        .await
        .unwrap();
    assert!(matches!(confirmed, ChainSubmissionResult::Confirmed(_)));
    assert!(
        rejection_ledger(&db).is_none(),
        "confirmation clears the streak"
    );
}

/// Scripted POST answers ahead of the accepting `Transport`.
struct ScriptedPosts {
    inner: Transport,
    posts: Mutex<Vec<ChainHttpResponse>>,
}

impl ChainTransport for ScriptedPosts {
    fn chain_get<'a>(&'a self, request: ChainHttpRequest) -> ChainTransportFuture<'a> {
        self.inner.chain_get(request)
    }
    fn chain_post_json<'a>(
        &'a self,
        request: ChainHttpRequest,
        json: Vec<u8>,
    ) -> ChainTransportFuture<'a> {
        let scripted = self.posts.lock().unwrap().pop();
        match scripted {
            Some(response) => {
                self.inner
                    .calls
                    .lock()
                    .unwrap()
                    .push(request.url().to_owned());
                Box::pin(async move { Ok(response) })
            }
            None => self.inner.chain_post_json(request, json),
        }
    }
}

fn rejection(code: u32, request: &AdvanceVoteBatch) -> ChainHttpResponse {
    ChainHttpResponse::json(
        422,
        format!(
            r#"{{"code":{code},"log":"round closed","batch_digest":"{}"}}"#,
            hex::encode(request.ordered_batch_digest)
        )
        .into_bytes(),
    )
}

fn client_over<T: ChainTransport + 'static>(
    db: &Arc<VotingDb>,
    transport: Arc<T>,
) -> ChainSubmissionClient<Arc<T>> {
    ChainSubmissionClient::with_transport(
        db.clone(),
        transport,
        ChainSubmissionClientConfig::for_network(
            crate::Network::Testnet,
            vec!["https://vote.example".to_owned()],
        )
        .with_post_attempts(2, vec![std::time::Duration::from_millis(1)]),
    )
    .unwrap()
}

#[tokio::test]
async fn a_definitely_rejected_combined_batch_retires_its_members_and_frees_the_delegation() {
    let (db, request) = fixture(2);
    let signature = stored_signature(&db);
    let transport = Arc::new(ScriptedPosts {
        inner: Transport {
            request: request.clone(),
            calls: Mutex::new(Vec::new()),
        },
        posts: Mutex::new(vec![rejection(7, &request)]),
    });
    let control = ChainSubmissionControl::new(1);
    let client = client_over(&db, transport.clone());

    let result = client
        .advance_delegate_and_vote_batch(request.clone(), ChainRecoveryMode::StatusOnly, &control)
        .await
        .unwrap();
    let ChainSubmissionResult::Rejected(diagnostic) = result else {
        panic!("a first-POST rejection of a combined batch is terminal: {result:?}")
    };
    assert!(diagnostic.message().contains("round closed"));
    assert_eq!(transport.inner.calls.lock().unwrap().len(), 1, "no retry");
    assert_retired_to_proved(&db, 2);

    // The same signature authorizes a fresh batch over the untouched setup.
    let fresh = persist_combined_batch(&db, signature, 2, 1);
    assert_ne!(fresh.ordered_batch_digest, request.ordered_batch_digest);
    let accepting = Arc::new(Transport {
        request: fresh.clone(),
        calls: Mutex::new(Vec::new()),
    });
    let client = client_over(&db, accepting.clone());
    let pending = client
        .advance_delegate_and_vote_batch(fresh.clone(), ChainRecoveryMode::StatusOnly, &control)
        .await
        .unwrap();
    assert!(matches!(pending, ChainSubmissionResult::Pending(_)));
    let confirmed = client
        .advance_delegate_and_vote_batch(fresh, ChainRecoveryMode::StatusOnly, &control)
        .await
        .unwrap();
    assert!(matches!(confirmed, ChainSubmissionResult::Confirmed(_)));
    assert_eq!(
        db.delegation_phase(ROUND, 0).unwrap(),
        crate::phases::DelegationPhase::Confirmed
    );
    assert_eq!(count(&db, "chain_submissions"), 1);
}

#[tokio::test(start_paused = true)]
async fn a_rejection_after_an_ambiguous_combined_post_keeps_the_row_recovering() {
    // The first POST may have been dispatched, so a later rejection cannot
    // prove the generation is off the chain: the row keeps its ambiguity and
    // every member stays locked for tree recovery.
    let (db, request) = fixture(2);
    let transport = Arc::new(ScriptedPosts {
        inner: Transport {
            request: request.clone(),
            calls: Mutex::new(Vec::new()),
        },
        posts: Mutex::new(vec![
            rejection(7, &request),
            ChainHttpResponse::new(
                502,
                b"<html>bad gateway</html>".to_vec(),
                Some("text/html".to_string()),
                Vec::new(),
            ),
        ]),
    });
    let control = ChainSubmissionControl::new(1);
    let client = client_over(&db, transport.clone());

    let failure = client
        .advance_delegate_and_vote_batch(request.clone(), ChainRecoveryMode::StatusOnly, &control)
        .await
        .unwrap_err();
    assert_eq!(failure.kind(), ChainSubmissionFailureKind::Protocol);
    assert_eq!(transport.inner.calls.lock().unwrap().len(), 2);
    let state: String = db
        .conn()
        .query_row("SELECT state FROM chain_submissions", [], |row| row.get(0))
        .unwrap();
    assert_eq!(state, "recovering");
    assert_eq!(count(&db, "delegate_cast_recovery"), 1);
    for proposal in 1..=2 {
        assert!(crate::vote::recovery_bundle(&db, ROUND, 0, proposal)
            .unwrap()
            .is_some());
    }
    assert_eq!(
        db.delegation_phase(ROUND, 0).unwrap(),
        crate::phases::DelegationPhase::SubmissionManaged
    );
}

#[tokio::test]
async fn a_nullifier_spent_first_post_keeps_the_combined_row_recovering() {
    // Code 2 says the delegation notes are spent by something this wallet
    // did not classify. Only tree recovery can explain it, and a recast would
    // fail the same way, so the generation is not retired.
    let (db, request) = fixture(1);
    let transport = Arc::new(ScriptedPosts {
        inner: Transport {
            request: request.clone(),
            calls: Mutex::new(Vec::new()),
        },
        posts: Mutex::new(vec![rejection(2, &request)]),
    });
    let control = ChainSubmissionControl::new(1);
    let client = client_over(&db, transport.clone());

    let result = client
        .advance_delegate_and_vote_batch(request.clone(), ChainRecoveryMode::StatusOnly, &control)
        .await
        .unwrap();
    assert!(
        matches!(result, ChainSubmissionResult::Pending(_)),
        "{result:?}"
    );
    assert_eq!(count(&db, "chain_submissions"), 1);
    assert_eq!(count(&db, "delegate_cast_recovery"), 1);
    assert!(crate::vote::recovery_bundle(&db, ROUND, 0, 1)
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn a_combined_candidate_that_commits_unsuccessfully_retires_the_generation() {
    // The accepted hash names this one signed envelope; when the chain
    // reports it committed with an error, no other dispatch of the same
    // bytes can have landed, so the generation is terminal and retired.
    struct FailingCommit(Transport);
    impl ChainTransport for FailingCommit {
        fn chain_get<'a>(&'a self, request: ChainHttpRequest) -> ChainTransportFuture<'a> {
            self.0.calls.lock().unwrap().push(request.url().to_owned());
            Box::pin(async {
                Ok(ChainHttpResponse::json(
                    422,
                    br#"{"height":"9","code":12,"log":"rejected","events":[]}"#.to_vec(),
                ))
            })
        }
        fn chain_post_json<'a>(
            &'a self,
            request: ChainHttpRequest,
            json: Vec<u8>,
        ) -> ChainTransportFuture<'a> {
            self.0.chain_post_json(request, json)
        }
    }
    let (db, request) = fixture(2);
    let signature = stored_signature(&db);
    let transport = Arc::new(FailingCommit(Transport {
        request: request.clone(),
        calls: Mutex::new(Vec::new()),
    }));
    let control = ChainSubmissionControl::new(1);
    let client = client_over(&db, transport.clone());

    let result = client
        .advance_delegate_and_vote_batch(request.clone(), ChainRecoveryMode::StatusOnly, &control)
        .await
        .unwrap();
    assert!(
        matches!(result, ChainSubmissionResult::Rejected(_)),
        "{result:?}"
    );
    assert_eq!(
        transport.0.calls.lock().unwrap().len(),
        2,
        "one POST, one poll"
    );
    assert_retired_to_proved(&db, 2);
    persist_combined_batch(&db, signature, 2, 1);
}

#[tokio::test]
async fn combined_admission_refuses_a_bundle_with_delegation_evidence() {
    // A persisted combined batch is admitted only while the bundle carries no
    // chain submission evidence at all. Any lifecycle row for the bundle,
    // here a standalone delegation still recovering, refuses it before a
    // reservation exists and before any bytes leave.
    let (db, request) = fixture(2);
    let standalone = ChainSubmissionIdentity::new(
        "wallet-1",
        crate::Network::Testnet,
        [0x11; 32],
        0,
        ChainSubmissionTarget::Delegation,
    )
    .unwrap();
    db.conn()
        .execute(
            "INSERT INTO chain_submissions
               (identity_key, round_id, wallet_id, network, bundle_index, kind,
                generation_digest, state, committed_post_reservations,
                diagnostic_kind, diagnostic, created_at, updated_at)
             VALUES (?1, ?2, 'wallet-1', 'testnet', 0, 'delegation', ?3, 'recovering', 1,
                     'ambiguous_dispatch', 'timed out', 9, 9)",
            rusqlite::params![
                crate::chain_submission::identity::submission_identity_key(&standalone),
                ROUND,
                vec![0x33_u8; 32]
            ],
        )
        .unwrap();
    let transport = Arc::new(Transport {
        request: request.clone(),
        calls: Mutex::new(Vec::new()),
    });
    let client = client_over(&db, transport.clone());

    let failure = client
        .advance_delegate_and_vote_batch(
            request,
            ChainRecoveryMode::StatusOnly,
            &ChainSubmissionControl::new(1),
        )
        .await
        .unwrap_err();
    assert_eq!(failure.kind(), ChainSubmissionFailureKind::InvalidInput);
    assert!(
        failure
            .message()
            .contains("combined admission requires a fresh delegation"),
        "{}",
        failure.message()
    );
    assert!(transport.calls.lock().unwrap().is_empty());
    assert_eq!(
        count(&db, "chain_submissions"),
        1,
        "no combined reservation"
    );
}
