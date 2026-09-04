use crate::MAX_CHAIN_SUBMISSION_DIAGNOSTIC_BYTES;
use std::{sync::Arc, time::Duration};

use crate::{
    ChainSubmissionClientConfig, ChainSubmissionControl, HelperClient, HelperHealth,
    HyperTransport, Network, RoundExecutor, VoteRecoveryDisposition, VoteRecoveryRequest,
};

use super::execution::{bounded_message, parse_round_id};

#[test]
fn failure_messages_are_bounded_and_escape_control_characters() {
    let message = format!(
        "secret\n{}",
        "x".repeat(MAX_CHAIN_SUBMISSION_DIAGNOSTIC_BYTES * 2)
    );
    let bounded = bounded_message(&message);
    assert!(!bounded.contains('\n'));
    assert!(bounded.len() <= MAX_CHAIN_SUBMISSION_DIAGNOSTIC_BYTES);
}

#[test]
fn round_id_parser_rejects_noncanonical_input() {
    let error = parse_round_id(&"A".repeat(64)).unwrap_err();
    assert!(error.to_string().contains("lowercase hex"));
}

#[tokio::test]
async fn no_persisted_vote_work_returns_without_network_io() {
    let database = Arc::new(crate::round::VotingDb::open_in_memory().unwrap());
    database.set_wallet_id("wallet");
    let helper_client = HelperClient::new(Arc::new(HyperTransport::new()), HelperHealth::default());
    let executor = RoundExecutor::new(
        database,
        ChainSubmissionClientConfig {
            network: Network::Testnet,
            vote_chain_id: "svote-1".to_string(),
            endpoints: vec!["http://chain.invalid".to_string()],
            tracking_window: Duration::from_secs(90),
            maximum_post_attempts: 1,
            retry_backoffs: Vec::new(),
        },
        helper_client,
    )
    .unwrap();
    let proposal_ids = [1];
    let helper_urls = ["http://helper.invalid".to_string()];
    let outcome = executor
        .advance(
            VoteRecoveryRequest {
                round_id: &"01".repeat(32),
                proposal_ids: &proposal_ids,
                configured_helper_urls: &helper_urls,
                now_seconds: 10,
                vote_end_time_seconds: 100,
                last_moment_buffer_seconds: Some(20),
            },
            &ChainSubmissionControl::new(1),
            &crate::NoopVoteRecoveryProgressReporter {},
        )
        .await
        .unwrap();

    assert_eq!(outcome.disposition, VoteRecoveryDisposition::NoWork);
    assert!(outcome.attempted_work.is_none());
    assert!(outcome.round_plan.open_proposals.contains(&1));
}

mod round_executor {
    use std::{sync::Arc, time::Duration};

    use crate::{
        delegate::{DelegationProgress, DelegationSubmission, SignedDelegationBundle},
        delegation_pipeline::{DelegationDriver, DelegationSigner, KeystoneSignatureSource},
        governance::BUNDLE_NOTE_SLOTS,
        pir::PirFleet,
        session::{Decision, NextStep},
        types::DelegationProgressReporter,
        wire::VotingRoundParams,
        BallotIntent, ChainAdvancePolicy, ChainSubmissionClientConfig, ChainSubmissionControl,
        DelegationStepInputs, HelperClient, HelperHealth, HyperTransport, Network,
        NoopRoundStepProgressReporter, ProposalRosterEntry, RoundBinding, RoundExecutor,
        RoundHostContext, RoundStepDisposition, RoundStepFailureKind, VotingError,
    };

    const ROUND_ID: &str = "0101010101010101010101010101010101010101010101010101010101010101";

    fn round_params() -> VotingRoundParams {
        VotingRoundParams {
            vote_round_id: ROUND_ID.to_string(),
            snapshot_height: 1000,
            ea_pk: vec![0xEA; 32],
            nc_root: vec![0xAA; 32],
            nullifier_imt_root: vec![0xBB; 32],
        }
    }

    /// One eligible note, so a choice intent has a bundle to plan against.
    fn note() -> crate::NoteInfo {
        crate::NoteInfo {
            commitment: vec![0x01; 32],
            nullifier: vec![0x02; 32],
            value: crate::governance::BALLOT_DIVISOR,
            position: 0,
            diversifier: vec![0x03; 11],
            rho: vec![0x04; 32],
            rseed: vec![0x05; 32],
            scope: 0,
            ufvk_str: "uview1test".to_string(),
        }
    }

    fn executor() -> RoundExecutor<HyperTransport> {
        let database = Arc::new(crate::round::VotingDb::open_in_memory().unwrap());
        database.set_wallet_id("wallet");
        database
            .create_round(Network::Testnet, &round_params(), None)
            .unwrap();
        database.ensure_bundles(ROUND_ID, &[note()]).unwrap();
        let helper_client =
            HelperClient::new(Arc::new(HyperTransport::new()), HelperHealth::default());
        RoundExecutor::new(
            database,
            ChainSubmissionClientConfig::for_network(
                Network::Testnet,
                vec!["http://chain.invalid".to_string()],
            ),
            helper_client,
        )
        .unwrap()
        .with_binding(RoundBinding {
            round_id: ROUND_ID.to_string(),
            network: Network::Testnet,
            proposals: vec![
                ProposalRosterEntry {
                    proposal_id: 1,
                    num_options: 2,
                },
                ProposalRosterEntry {
                    proposal_id: 2,
                    num_options: 3,
                },
            ],
            hotkey_secret: None,
        })
        .unwrap()
    }

    fn host() -> RoundHostContext {
        RoundHostContext {
            configured_helper_urls: vec!["http://helper.invalid".to_string()],
            now_seconds: 10,
            ceremony_start_seconds: Some(0),
            vote_end_time_seconds: Some(100_000),
            vote_tree_node_urls: vec!["http://node.invalid".to_string()],
            delegation: None,
            chain_policy: ChainAdvancePolicy {
                pending_repoll: Duration::from_millis(1),
                ..ChainAdvancePolicy::default()
            },
            max_proof_concurrency: 1,
        }
    }

    #[test]
    fn unbound_executor_rejects_the_step_api() {
        let database = Arc::new(crate::round::VotingDb::open_in_memory().unwrap());
        database.set_wallet_id("wallet");
        let helper_client =
            HelperClient::new(Arc::new(HyperTransport::new()), HelperHealth::default());
        let executor = RoundExecutor::new(
            database,
            ChainSubmissionClientConfig::for_network(
                Network::Testnet,
                vec!["http://chain.invalid".to_string()],
            ),
            helper_client,
        )
        .unwrap();
        let error = executor.plan().unwrap_err();
        assert_eq!(error.kind(), crate::VotingErrorKind::InvalidInput);
    }

    #[test]
    fn ballot_intents_use_the_bound_roster() {
        let executor = executor();
        let plan = executor.plan().unwrap();
        assert_eq!(plan.open_proposals, vec![1, 2]);

        let plan = executor
            .set_ballot_intents(&[BallotIntent {
                proposal_id: 2,
                decision: Decision::Skipped,
            }])
            .unwrap();
        assert_eq!(plan.open_proposals, vec![1]);

        let error = executor
            .set_ballot_intents(&[BallotIntent {
                proposal_id: 9,
                decision: Decision::Choice(0),
            }])
            .unwrap_err();
        assert_eq!(error.kind(), crate::VotingErrorKind::InvalidInput);
        assert!(error.to_string().contains("roster"));
    }

    #[test]
    fn a_batch_naming_an_unknown_proposal_writes_nothing() {
        let executor = executor();
        let error = executor
            .set_ballot_intents(&[
                BallotIntent {
                    proposal_id: 1,
                    decision: Decision::Choice(0),
                },
                BallotIntent {
                    proposal_id: 9,
                    decision: Decision::Choice(0),
                },
            ])
            .unwrap_err();
        assert_eq!(error.kind(), crate::VotingErrorKind::InvalidInput);
        assert!(error.to_string().contains("roster"));

        // The valid leading intent must not have been applied.
        assert_eq!(executor.plan().unwrap().open_proposals, vec![1, 2]);
    }

    #[test]
    fn a_batch_deciding_one_proposal_twice_is_rejected() {
        let executor = executor();
        let error = executor
            .set_ballot_intents(&[
                BallotIntent {
                    proposal_id: 1,
                    decision: Decision::Choice(0),
                },
                BallotIntent {
                    proposal_id: 1,
                    decision: Decision::Skipped,
                },
            ])
            .unwrap_err();
        assert_eq!(error.kind(), crate::VotingErrorKind::InvalidInput);
        assert!(error.to_string().contains("twice"));
        assert_eq!(executor.plan().unwrap().open_proposals, vec![1, 2]);
    }

    #[test]
    fn a_batch_rejected_by_a_later_intent_rolls_back_the_earlier_one() {
        let executor = executor();
        let database = executor.database();
        database
            .set_ballot_intent(ROUND_ID, 2, Decision::Choice(1), 3)
            .unwrap();
        database
            .store_delegation_tx_hash(ROUND_ID, 0, "dtx")
            .unwrap();
        database.store_van_position(ROUND_ID, 0, 7).unwrap();
        crate::storage::queries::store_vote(
            &database.conn(),
            ROUND_ID,
            "wallet",
            0,
            2,
            1,
            &[0xCC; 16],
        )
        .unwrap();
        database
            .record_vote_submission(ROUND_ID, 0, 2, "vtx")
            .unwrap();

        // Proposal 1 is valid, proposal 2 now contradicts a submitted vote.
        let error = executor
            .set_ballot_intents(&[
                BallotIntent {
                    proposal_id: 1,
                    decision: Decision::Choice(0),
                },
                BallotIntent {
                    proposal_id: 2,
                    decision: Decision::Choice(2),
                },
            ])
            .unwrap_err();
        assert_eq!(error.kind(), crate::VotingErrorKind::InvalidInput);
        assert!(
            error.to_string().contains("ballot intent"),
            "unexpected error: {error}"
        );

        // Proposal 1 stays open and proposal 2 keeps its original choice.
        assert_eq!(
            database.ballot_intents(ROUND_ID).unwrap(),
            vec![(2, Decision::Choice(1))]
        );
    }

    #[test]
    fn a_valid_batch_applies_every_intent() {
        let executor = executor();
        let plan = executor
            .set_ballot_intents(&[
                BallotIntent {
                    proposal_id: 1,
                    decision: Decision::Choice(0),
                },
                BallotIntent {
                    proposal_id: 2,
                    decision: Decision::Skipped,
                },
            ])
            .unwrap();
        assert!(plan.open_proposals.is_empty());
    }

    #[tokio::test]
    async fn empty_plan_and_stale_steps_return_no_work_without_network_io() {
        let executor = executor();
        let control = ChainSubmissionControl::new(1);
        let outcome = executor
            .advance_next(&host(), &control, &NoopRoundStepProgressReporter {})
            .await
            .unwrap();
        assert_eq!(outcome.disposition, RoundStepDisposition::NoWork);
        assert!(outcome.step.is_none());

        let stale = NextStep::AdvanceVote {
            bundle_index: 0,
            proposal_id: 1,
        };
        let outcome = executor
            .advance_step(
                stale.clone(),
                &host(),
                &control,
                &NoopRoundStepProgressReporter {},
            )
            .await
            .unwrap();
        assert_eq!(outcome.disposition, RoundStepDisposition::NoWork);
        assert_eq!(outcome.step, Some(stale));
    }

    #[tokio::test]
    async fn cancelled_control_short_circuits_before_any_work() {
        let executor = executor();
        let control = ChainSubmissionControl::new(1);
        control.cancel();
        let outcome = executor
            .advance_next(&host(), &control, &NoopRoundStepProgressReporter {})
            .await
            .unwrap();
        // No step exists, so the plan wins over cancellation.
        assert_eq!(outcome.disposition, RoundStepDisposition::NoWork);
    }

    #[test]
    fn host_context_derives_timing_from_the_shared_policy() {
        let mut host = host();
        assert_eq!(
            host.last_moment_buffer_seconds(),
            crate::share::policy::last_moment_buffer_seconds(0, 100_000)
        );
        assert!(!host.is_last_moment());
        host.now_seconds = 99_999;
        assert!(host.is_last_moment());
        host.vote_end_time_seconds = None;
        assert_eq!(host.last_moment_buffer_seconds(), None);
        assert!(!host.is_last_moment());
        assert_eq!(host.planning_vote_end_seconds(), 99_999);
    }

    #[tokio::test]
    async fn bundle_scoped_locks_do_not_serialize_distinct_bundles() {
        let control = ChainSubmissionControl::new(1);
        let first = super::super::round_lock::acquire("w".to_string(), ROUND_ID, Some(0), &control)
            .await
            .unwrap()
            .unwrap();
        let second = tokio::time::timeout(
            Duration::from_millis(200),
            super::super::round_lock::acquire("w".to_string(), ROUND_ID, Some(1), &control),
        )
        .await
        .expect("a different bundle must not wait")
        .unwrap();
        assert!(second.is_some());
        let round_scope = tokio::time::timeout(
            Duration::from_millis(200),
            super::super::round_lock::acquire("w".to_string(), ROUND_ID, None, &control),
        )
        .await
        .expect("the round scope is independent of bundle scopes")
        .unwrap();
        assert!(round_scope.is_some());
        let same_bundle = tokio::time::timeout(
            Duration::from_millis(100),
            super::super::round_lock::acquire("w".to_string(), ROUND_ID, Some(0), &control),
        )
        .await;
        assert!(same_bundle.is_err(), "the same bundle must wait");
        drop(first);
        let _ = RoundStepFailureKind::Busy;
    }

    /// A driver that signs instantly and cancels the host control from inside
    /// the signing thread, so the executor observes cancellation after the
    /// payload is signed and before chain dispatch.
    struct CancelAfterSigningDriver {
        control: ChainSubmissionControl,
    }

    impl DelegationDriver for CancelAfterSigningDriver {
        fn round_id(&self) -> &str {
            ROUND_ID
        }

        fn prove_and_sign_blocking(
            &self,
            bundle_index: u32,
            _signer: &DelegationSigner,
            _pir: &PirFleet,
            progress: &dyn DelegationProgressReporter,
        ) -> Result<SignedDelegationBundle, VotingError> {
            progress.on_progress(DelegationProgress::PayloadReady);
            self.control.cancel();
            Ok(SignedDelegationBundle {
                submission: DelegationSubmission {
                    proof: vec![0x61; 96],
                    rk: [0x62; 32],
                    nf_signed: [0x63; 32],
                    cmx_new: [0x64; 32],
                    gov_comm: [0x65; 32],
                    gov_nullifiers: [[0x66; 32]; BUNDLE_NOTE_SLOTS],
                    alpha: [0x67; 32],
                    vote_round_id: ROUND_ID.to_string(),
                    spend_auth_sig: [0x68; 64],
                    sighash: [0x69; 32],
                    tx1_effects: Vec::new(),
                },
                pczt_bytes: Vec::new(),
                eligible_weight_zatoshi: crate::governance::BALLOT_DIVISOR,
                delegated_weight_zatoshi: crate::governance::BALLOT_DIVISOR,
                bundle_count: 1,
                bundle_index,
            })
        }

        fn resign_blocking(
            &self,
            _bundle_index: u32,
            _signer: &DelegationSigner,
        ) -> Result<[u8; 64], VotingError> {
            Ok([0x68; 64])
        }
    }

    fn host_with_delegation(control: &ChainSubmissionControl) -> RoundHostContext {
        RoundHostContext {
            delegation: Some(DelegationStepInputs {
                driver: Arc::new(CancelAfterSigningDriver {
                    control: control.clone(),
                }),
                signer: DelegationSigner::Keystone(KeystoneSignatureSource::Provided {
                    sig: vec![0x68; 64],
                    sighash: vec![0x69; 32],
                }),
                pir: Arc::new(
                    PirFleet::new(
                        &["http://pir.invalid".to_string()],
                        crate::config::PirLayout {
                            pir_depth: u32::try_from(pir_types::COMPILED_PIR_LAYOUT.pir_depth)
                                .unwrap(),
                            tier0_layers: u32::try_from(
                                pir_types::COMPILED_PIR_LAYOUT.tier0_layers,
                            )
                            .unwrap(),
                            tier1_layers: u32::try_from(
                                pir_types::COMPILED_PIR_LAYOUT.tier1_layers,
                            )
                            .unwrap(),
                            poly_len: pir_types::DEFAULT_YPIR_POLY_LEN as u32,
                        },
                        Arc::new(HyperTransport::new()),
                    )
                    .unwrap(),
                ),
            }),
            ..host()
        }
    }

    #[tokio::test]
    async fn a_delegate_step_cancelled_after_signing_returns_the_signed_bundle() {
        let executor = executor();
        executor
            .set_ballot_intents(&[
                BallotIntent {
                    proposal_id: 1,
                    decision: Decision::Choice(0),
                },
                BallotIntent {
                    proposal_id: 2,
                    decision: Decision::Skipped,
                },
            ])
            .unwrap();
        let control = ChainSubmissionControl::new(1);
        let step = NextStep::Delegate { bundle_index: 0 };
        assert!(executor.plan().unwrap().next_steps.contains(&step));

        let outcome = executor
            .advance_step(
                step.clone(),
                &host_with_delegation(&control),
                &control,
                &NoopRoundStepProgressReporter {},
            )
            .await
            .unwrap();

        assert_eq!(outcome.disposition, RoundStepDisposition::Cancelled);
        assert_eq!(outcome.step, Some(step));
        let signed = outcome
            .delegation
            .expect("a cancelled Delegate step still hands back the signed bundle");
        assert_eq!(signed.bundle_index, 0);
        assert_eq!(signed.submission.spend_auth_sig, [0x68; 64]);
    }
}
