//! Batch diagnostics exercise the same preparation, persistence and recovery paths.
use super::*;
use crate::{ObservabilityOptions, ObservationAttribution, ObservationOutcome, ObservationScope};

fn member_scope(scope: &ObservationScope) -> ObservationScope {
    scope.attributed(ObservationAttribution {
        bundle_index: Some(0),
        proposal_id: Some(2),
        share_index: Some(3),
    })
}

#[test]
fn batch_preparation_failure_retains_identity_without_a_triggering_member() {
    for options in [None, Some(ObservabilityOptions::default())] {
        let db = db_with_vote();
        let hotkey = VotingHotkey::from_stored_secret(&[0x99; 64], Network::Testnet).unwrap();
        let witness = VanWitness {
            auth_path: vec![],
            position: 0,
            anchor_height: 0,
        };
        let owner = ObservationScope::new(options).invocation();
        let parent = owner.stage("round::advance_step");
        let scope = member_scope(parent.scope());
        let batch = AtomicVoteBatch::new(ROUND_ID, 0, &[], &witness, &NoopProgressReporter);
        let error =
            observe_prepare_atomic_vote_batch(&db, VoteSigner::hotkey(&hotkey), batch, &scope)
                .err()
                .expect("empty batch must fail");
        let plain = prepare_atomic_vote_batch(
            &db,
            VoteSigner::hotkey(&hotkey),
            AtomicVoteBatch::new(ROUND_ID, 0, &[], &witness, &NoopProgressReporter),
        )
        .err()
        .unwrap();
        assert_eq!(error.to_string(), plain.to_string());
        parent.finish(ObservationOutcome::Failed, None);
        let report = owner.complete("batch", ObservationOutcome::Failed, error);
        if options.is_none() {
            assert!(report.observability.is_none());
            continue;
        }
        let diagnostics = report.observability.unwrap();
        assert_eq!(diagnostics.round_id.as_deref(), Some(ROUND_ID));
        let batch = diagnostics
            .records
            .iter()
            .find(|r| r.stage.as_ref() == "vote::prepare_atomic_vote_batch")
            .unwrap();
        assert_eq!(
            batch.attribution,
            ObservationAttribution {
                bundle_index: Some(0),
                ..Default::default()
            }
        );
        assert_eq!(batch.parent_id, Some(diagnostics.records[0].id));
        assert_eq!(batch.outcome, ObservationOutcome::Failed);
        assert!(batch.error_kind.is_some());
    }
}

#[test]
fn batch_persistence_and_recovery_preserve_results_and_bundle_attribution() {
    let plain_db = db_with_vote();
    let plain = persist_prepared_atomic_vote_batch(
        &plain_db,
        prepared_atomic_vote_batch_fixture(&plain_db),
    )
    .unwrap();
    for options in [None, Some(ObservabilityOptions::default())] {
        let db = db_with_vote();
        let owner = ObservationScope::new(options).invocation();
        let parent = owner.stage("round::advance_step");
        let scope = member_scope(parent.scope());
        let signed = observe_persist_prepared_atomic_vote_batch(
            &db,
            prepared_atomic_vote_batch_fixture(&db),
            &scope,
        )
        .unwrap();
        assert_eq!(signed.batch_json, plain.batch_json);
        for proposal in [1, 2] {
            let recovered =
                observe_recover_atomic_vote_batch(&db, ROUND_ID, 0, proposal, &scope).unwrap();
            assert_eq!(recovered.batch_json, signed.batch_json);
            assert_eq!(recovered.batch_digest, signed.batch_digest);
        }
        parent.finish(ObservationOutcome::Succeeded, None);
        let report = owner.complete("batch", ObservationOutcome::Succeeded, signed);
        if options.is_none() {
            assert!(report.observability.is_none());
            continue;
        }
        let diagnostics = report.observability.unwrap();
        assert_eq!(diagnostics.round_id.as_deref(), Some(ROUND_ID));
        assert_eq!(diagnostics.records.len(), 4);
        for record in &diagnostics.records[1..] {
            assert_eq!(
                record.attribution,
                ObservationAttribution {
                    bundle_index: Some(0),
                    ..Default::default()
                }
            );
            assert_eq!(record.parent_id, Some(diagnostics.records[0].id));
            assert_eq!(record.outcome, ObservationOutcome::Succeeded);
        }
    }
}

#[test]
fn batch_workers_keep_member_identity_and_one_parent_on_validation_failure() {
    let db = db_with_vote();
    configure_prepared_vote_fixture_bundle(&db);
    let state =
        queries::load_vote_preparation_state(&db.conn(), ROUND_ID, WALLET_ID, 0, 1).unwrap();
    let plans = [1, 2]
        .into_iter()
        .map(|proposal_id| BatchProofPlan {
            draft: DraftVote {
                proposal_id,
                num_options: 0,
                ..draft_vote_fixture()
            },
            state: state.clone(),
            auth_path: [[0; 32]; VAN_AUTH_PATH_LEN],
            position: 0,
            proposal_authority: 0,
            expected_new_van: [0; 32],
        })
        .collect::<Vec<_>>();
    for concurrency in [1, 2] {
        let owner = ObservationScope::new(Some(ObservabilityOptions::default())).invocation();
        owner.scope().bind_round_id(ROUND_ID);
        let bundle = owner.scope().for_bundle(0);
        let parent = bundle.stage("vote::prepare_atomic_vote_batch");
        let error = build_batch_proofs(
            &[],
            0,
            0,
            &[],
            &NoopProgressReporter,
            concurrency,
            &plans,
            parent.scope(),
        )
        .unwrap_err();
        assert!(matches!(error, VotingError::InvalidInput { .. }));
        parent.finish(ObservationOutcome::Failed, None);
        let diagnostics = owner
            .complete("batch", ObservationOutcome::Failed, ())
            .observability
            .unwrap();
        let mut proposals = diagnostics
            .records
            .iter()
            .filter(|record| record.stage.as_ref() == "zkp2::build_vote_commitment")
            .map(|record| {
                assert_eq!(record.parent_id, Some(diagnostics.records[0].id));
                assert_eq!(record.attribution.bundle_index, Some(0));
                assert_eq!(record.attribution.share_index, None);
                assert_eq!(record.outcome, ObservationOutcome::Failed);
                record.attribution.proposal_id.unwrap()
            })
            .collect::<Vec<_>>();
        proposals.sort();
        assert!(!proposals.is_empty());
        assert!(proposals.len() <= concurrency);
        assert!(proposals.iter().all(|proposal| [1, 2].contains(proposal)));
    }
}
