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

#[test]
fn step_control_treats_an_epoch_change_like_cancellation() {
    use super::step_control::StepControl;
    use crate::ChainSubmissionControl;

    let control = ChainSubmissionControl::new(3);
    let captured = StepControl::capture(&control);
    assert!(!captured.interrupted());

    control.set_operation_epoch(4);
    assert!(captured.interrupted(), "a later epoch invalidates the pass");
    assert!(!control.is_cancelled());

    // A pass captured under the new epoch is live until cancelled.
    let recaptured = StepControl::capture(&control);
    assert!(!recaptured.interrupted());
    control.cancel();
    assert!(recaptured.interrupted());
    assert!(std::ptr::eq(recaptured.chain(), &control));
}

#[test]
fn vote_tree_node_urls_must_be_http_urls_and_https_on_mainnet() {
    use super::cast_vote::validate_vote_tree_node_urls;
    use crate::{Network, VotingError};

    let urls = |list: &[&str]| list.iter().map(|url| url.to_string()).collect::<Vec<_>>();

    validate_vote_tree_node_urls(
        &urls(&["http://node.test:8080", "https://node.test"]),
        Network::Testnet,
    )
    .unwrap();
    validate_vote_tree_node_urls(&urls(&["https://node.example"]), Network::Mainnet).unwrap();

    for (list, network, needle) in [
        (urls(&[]), Network::Testnet, "at least one"),
        (
            urls(&["https://ok.example", "http://node.example"]),
            Network::Mainnet,
            "must use HTTPS",
        ),
        (
            urls(&["ftp://node.test"]),
            Network::Testnet,
            "http or https",
        ),
        (urls(&["not a url"]), Network::Testnet, "is invalid"),
        (urls(&["/relative/path"]), Network::Testnet, "with a host"),
    ] {
        let error = validate_vote_tree_node_urls(&list, network).unwrap_err();
        assert!(
            matches!(error, VotingError::InvalidInput { .. }),
            "{list:?}: {error}"
        );
        assert!(error.to_string().contains(needle), "{list:?}: {error}");
    }
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
        executor_over(host_database()).0
    }

    /// The host's own handle: wallet "wallet" with one bundle in the round.
    fn host_database() -> Arc<crate::round::VotingDb> {
        host_database_for("wallet")
    }

    fn host_database_for(wallet_id: &str) -> Arc<crate::round::VotingDb> {
        let database = Arc::new(crate::round::VotingDb::open_in_memory().unwrap());
        database.set_wallet_id(wallet_id);
        database
            .create_round(Network::Testnet, &round_params(), None)
            .unwrap();
        database.ensure_bundles(ROUND_ID, &[note()]).unwrap();
        database
    }

    fn executor_over(
        database: Arc<crate::round::VotingDb>,
    ) -> (RoundExecutor<HyperTransport>, Arc<crate::round::VotingDb>) {
        bound_executor(database, None)
    }

    fn bound_executor_unbound(
        database: Arc<crate::round::VotingDb>,
    ) -> (RoundExecutor<HyperTransport>, Arc<crate::round::VotingDb>) {
        let helper_client =
            HelperClient::new(Arc::new(HyperTransport::new()), HelperHealth::default());
        let executor = RoundExecutor::new(
            Arc::clone(&database),
            ChainSubmissionClientConfig::for_network(
                Network::Testnet,
                vec!["http://chain.invalid".to_string()],
            ),
            helper_client,
        )
        .unwrap();
        (executor, database)
    }

    fn bound_executor(
        database: Arc<crate::round::VotingDb>,
        hotkey_secret: Option<zeroize::Zeroizing<Vec<u8>>>,
    ) -> (RoundExecutor<HyperTransport>, Arc<crate::round::VotingDb>) {
        let helper_client =
            HelperClient::new(Arc::new(HyperTransport::new()), HelperHealth::default());
        let executor = RoundExecutor::new(
            Arc::clone(&database),
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
            hotkey_secret,
        })
        .unwrap();
        (executor, database)
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
        let first = super::super::round_lock::acquire(
            "w".to_string(),
            ROUND_ID,
            Some(0),
            &control,
            control.operation_epoch(),
        )
        .await
        .unwrap()
        .unwrap();
        let second = tokio::time::timeout(
            Duration::from_millis(200),
            super::super::round_lock::acquire(
                "w".to_string(),
                ROUND_ID,
                Some(1),
                &control,
                control.operation_epoch(),
            ),
        )
        .await
        .expect("a different bundle must not wait")
        .unwrap();
        assert!(second.is_some());
        let round_scope = tokio::time::timeout(
            Duration::from_millis(200),
            super::super::round_lock::acquire(
                "w".to_string(),
                ROUND_ID,
                None,
                &control,
                control.operation_epoch(),
            ),
        )
        .await
        .expect("the round scope is independent of bundle scopes")
        .unwrap();
        assert!(round_scope.is_some());
        let same_bundle = tokio::time::timeout(
            Duration::from_millis(100),
            super::super::round_lock::acquire(
                "w".to_string(),
                ROUND_ID,
                Some(0),
                &control,
                control.operation_epoch(),
            ),
        )
        .await;
        assert!(same_bundle.is_err(), "the same bundle must wait");
        drop(first);
        let _ = RoundStepFailureKind::Busy;
    }

    /// How the mock driver interrupts the host control from inside signing.
    #[derive(Clone, Copy)]
    enum Interrupt {
        Cancel,
        NewOperationEpoch,
    }

    /// A driver that signs instantly and interrupts the host control from
    /// inside the signing thread, so the executor observes the interruption
    /// after the payload is signed and before chain dispatch.
    struct CancelAfterSigningDriver {
        control: ChainSubmissionControl,
        interrupt: Interrupt,
        network: Network,
        target: Option<crate::VotingHotkeyTarget>,
        wallet_id: String,
        database: Arc<crate::round::VotingDb>,
    }

    fn hotkey_target(secret_byte: u8) -> crate::VotingHotkeyTarget {
        crate::VotingHotkey::from_stored_secret(&[secret_byte; 64], Network::Testnet)
            .unwrap()
            .delegation_target()
    }

    impl CancelAfterSigningDriver {
        fn apply_interrupt(&self) {
            match self.interrupt {
                Interrupt::Cancel => self.control.cancel(),
                Interrupt::NewOperationEpoch => self
                    .control
                    .set_operation_epoch(self.control.operation_epoch() + 1),
            }
        }
    }

    impl DelegationDriver for CancelAfterSigningDriver {
        fn round_id(&self) -> &str {
            ROUND_ID
        }

        fn network(&self) -> Network {
            self.network
        }

        fn delegation_target(&self) -> Option<crate::VotingHotkeyTarget> {
            self.target
        }

        fn wallet_id(&self) -> &str {
            &self.wallet_id
        }

        fn shares_database_with(&self, database: &crate::round::VotingDb) -> bool {
            self.database.shares_connection_with(database)
        }

        fn prove_and_sign_blocking(
            &self,
            bundle_index: u32,
            _signer: &DelegationSigner,
            _pir: &PirFleet,
            progress: &dyn DelegationProgressReporter,
        ) -> Result<SignedDelegationBundle, VotingError> {
            progress.on_progress(DelegationProgress::PayloadReady);
            self.apply_interrupt();
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
            self.apply_interrupt();
            Ok([0x68; 64])
        }
    }

    fn host_with_delegation(
        control: &ChainSubmissionControl,
        driver_wallet_id: &str,
        database: &Arc<crate::round::VotingDb>,
    ) -> RoundHostContext {
        host_with_interrupting_delegation(control, Interrupt::Cancel, driver_wallet_id, database)
    }

    fn host_with_interrupting_delegation(
        control: &ChainSubmissionControl,
        interrupt: Interrupt,
        driver_wallet_id: &str,
        database: &Arc<crate::round::VotingDb>,
    ) -> RoundHostContext {
        host_with_driver(
            control,
            interrupt,
            Network::Testnet,
            driver_wallet_id,
            database,
        )
    }

    fn host_with_driver(
        control: &ChainSubmissionControl,
        interrupt: Interrupt,
        network: Network,
        driver_wallet_id: &str,
        database: &Arc<crate::round::VotingDb>,
    ) -> RoundHostContext {
        host_with_driver_target(
            control,
            interrupt,
            network,
            Some(hotkey_target(0x21)),
            driver_wallet_id,
            database,
        )
    }

    fn host_with_driver_target(
        control: &ChainSubmissionControl,
        interrupt: Interrupt,
        network: Network,
        target: Option<crate::VotingHotkeyTarget>,
        driver_wallet_id: &str,
        database: &Arc<crate::round::VotingDb>,
    ) -> RoundHostContext {
        RoundHostContext {
            delegation: Some(DelegationStepInputs {
                driver: Arc::new(CancelAfterSigningDriver {
                    control: control.clone(),
                    interrupt,
                    network,
                    target,
                    wallet_id: driver_wallet_id.to_string(),
                    database: Arc::clone(database),
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
                &host_with_delegation(&control, "wallet", &executor.database()),
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

    #[tokio::test]
    async fn a_host_wallet_switch_does_not_retarget_a_bound_executor() {
        let (executor, host_handle) = executor_over(host_database());
        let bound_plan = executor.plan().unwrap();
        assert!(!bound_plan.delegation_statuses.is_empty());

        // The host moves its own handle to an account with no state in this round.
        host_handle.set_wallet_id("other-wallet");

        assert_eq!(executor.database().wallet_id(), "wallet");
        assert!(executor.database().shares_connection_with(&host_handle));
        let plan_after_switch = executor.plan().unwrap();
        assert_eq!(
            plan_after_switch.delegation_statuses,
            bound_plan.delegation_statuses
        );
        let control = ChainSubmissionControl::new(1);
        let outcome = executor
            .advance_next(&host(), &control, &NoopRoundStepProgressReporter {})
            .await
            .unwrap();
        assert_eq!(outcome.disposition, RoundStepDisposition::NoWork);
    }

    #[tokio::test]
    async fn re_scoping_a_handle_from_database_does_not_reach_the_executor() {
        let executor = executor();
        let handle = executor.database();
        handle.set_wallet_id("other-wallet");

        // The executor never hands out its own handle, so the re-scope is
        // confined to the caller's copy.
        assert_eq!(executor.database().wallet_id(), "wallet");
        assert!(handle.shares_connection_with(&executor.database()));
        let plan = executor.plan().unwrap();
        assert!(!plan.delegation_statuses.is_empty());
        let control = ChainSubmissionControl::new(1);
        let outcome = executor
            .advance_next(&host(), &control, &NoopRoundStepProgressReporter {})
            .await
            .unwrap();
        assert_eq!(outcome.disposition, RoundStepDisposition::NoWork);
    }
    #[tokio::test]
    async fn a_cast_vote_selected_ahead_of_its_delegation_is_rejected_before_any_work() {
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
        let cast = NextStep::CastVote {
            bundle_index: 0,
            proposal_id: 1,
            choice: 0,
        };
        let plan = executor.plan().unwrap();
        assert_eq!(
            plan.next_steps,
            vec![NextStep::Delegate { bundle_index: 0 }, cast.clone()]
        );

        // The only node URL is unreachable, so reaching tree sync would fail
        // with a transport error rather than InvalidInput.
        let control = ChainSubmissionControl::new(1);
        let failure = executor
            .advance_step(
                cast.clone(),
                &host(),
                &control,
                &NoopRoundStepProgressReporter {},
            )
            .await
            .expect_err("a step with an unresolved delegation prerequisite must not run");

        assert_eq!(failure.kind, RoundStepFailureKind::InvalidInput);
        assert_eq!(failure.step, Some(cast));
        assert!(failure.message.contains("Delegate"), "{}", failure.message);
    }

    /// A vote-tree transport with no reachable node: every request fails
    /// after being counted, so a sync creates the round's tree client and then
    /// errors out of it. It can also cancel the host control from inside the
    /// request, modelling a cancellation that arrives while a sync is in flight.
    struct UnreachableTreeTransport {
        requests: std::sync::atomic::AtomicUsize,
        cancel_on_request: Option<ChainSubmissionControl>,
    }

    impl vote_commitment_tree_client::transport::Transport for UnreachableTreeTransport {
        fn get(
            &self,
            _url: &str,
        ) -> Result<
            vote_commitment_tree_client::transport::TransportResponse,
            vote_commitment_tree_client::transport::TransportError,
        > {
            self.requests
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Some(control) = &self.cancel_on_request {
                control.cancel();
            }
            Err(
                vote_commitment_tree_client::transport::TransportError::Request(
                    "node unreachable".to_string(),
                ),
            )
        }
    }

    /// An executor whose bundle 0 delegation is confirmed and whose ballot is
    /// decided, so `CastVote` is the plan head and reaches tree sync.
    fn executor_ready_to_cast(
        wallet_id: &str,
    ) -> (RoundExecutor<HyperTransport>, Arc<UnreachableTreeTransport>) {
        executor_ready_to_cast_with(wallet_id, None)
    }

    fn executor_ready_to_cast_with(
        wallet_id: &str,
        cancel_on_request: Option<ChainSubmissionControl>,
    ) -> (RoundExecutor<HyperTransport>, Arc<UnreachableTreeTransport>) {
        let database = host_database_for(wallet_id);
        // A confirmed delegation carries its VAN commitment; the tree sync
        // reads it to place the bundle in the synced tree.
        let van_commitment = {
            use crate::backend::pasta_curves::{group::ff::PrimeField, pallas};
            pallas::Base::from(9u64).to_repr().to_vec()
        };
        crate::storage::queries::store_delegation_data(
            &database.conn(),
            ROUND_ID,
            wallet_id,
            0,
            &[0x41; 32],
            &[],
            &[0x42; 32],
            &[],
            &[0x43; 32],
            &[0x44; 32],
            &[0x45; 32],
            &[0x46; 32],
            &[0x47; 32],
            &van_commitment,
            crate::governance::BALLOT_DIVISOR,
            0,
            &[],
            &[0x49; 32],
            &crate::tx1::placeholder_tx1_effects(),
        )
        .unwrap();
        database
            .store_delegation_tx_hash(ROUND_ID, 0, "dtx")
            .unwrap();
        database.store_van_position(ROUND_ID, 0, 7).unwrap();
        let (executor, _) = bound_executor(database, Some(zeroize::Zeroizing::new(vec![0x21; 64])));
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
        let transport = Arc::new(UnreachableTreeTransport {
            requests: std::sync::atomic::AtomicUsize::new(0),
            cancel_on_request,
        });
        let executor = executor.with_tree_transport(transport.clone());
        (executor, transport)
    }

    async fn cast_against_unreachable_nodes(wallet_id: &str, node_urls: Vec<String>) -> usize {
        let (executor, transport) = executor_ready_to_cast(wallet_id);
        let cast = NextStep::CastVote {
            bundle_index: 0,
            proposal_id: 1,
            choice: 0,
        };
        assert_eq!(executor.plan().unwrap().next_steps.first(), Some(&cast));
        let host = RoundHostContext {
            vote_tree_node_urls: node_urls,
            ..host()
        };
        let control = ChainSubmissionControl::new(1);
        let failure = executor
            .advance_step(cast, &host, &control, &NoopRoundStepProgressReporter {})
            .await
            .expect_err("no node is reachable");
        assert!(
            failure.message.contains("vote tree sync"),
            "failure must come from tree sync, got: {} ({:?})",
            failure.message,
            failure.kind
        );

        let cached = crate::precompute::cached_vote_tree_rounds(&executor.database())
            .contains(&ROUND_ID.to_string());
        crate::precompute::reset_vote_tree(&executor.database(), "").unwrap();
        assert!(
            !cached,
            "a failed sync must not leave the round's tree client behind"
        );
        transport.requests.load(std::sync::atomic::Ordering::SeqCst)
    }

    #[tokio::test]
    async fn a_failed_sync_on_the_only_node_clears_the_cached_round_tree() {
        let requests = cast_against_unreachable_nodes(
            "wallet-single-node",
            vec!["http://node-a.invalid".to_string()],
        )
        .await;
        assert_eq!(requests, 1);
    }

    #[tokio::test]
    async fn a_failed_sync_on_the_last_node_clears_the_cached_round_tree() {
        let requests = cast_against_unreachable_nodes(
            "wallet-two-nodes",
            vec![
                "http://node-a.invalid".to_string(),
                "http://node-b.invalid".to_string(),
            ],
        )
        .await;
        assert_eq!(requests, 2, "both nodes are tried in order");
    }

    fn decided_ballot(executor: &RoundExecutor<HyperTransport>) {
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
    }

    #[tokio::test]
    async fn a_driver_scoped_to_another_wallet_is_refused_before_proving() {
        let executor = executor();
        decided_ballot(&executor);
        let control = ChainSubmissionControl::new(1);
        let step = NextStep::Delegate { bundle_index: 0 };

        let failure = executor
            .advance_step(
                step.clone(),
                &host_with_delegation(&control, "other-wallet", &executor.database()),
                &control,
                &NoopRoundStepProgressReporter {},
            )
            .await
            .expect_err("a driver for another wallet must not run under this wallet's lock");

        assert_eq!(failure.kind, RoundStepFailureKind::InvalidInput);
        assert!(
            failure.message.contains("other-wallet"),
            "{}",
            failure.message
        );
        assert!(
            !control.is_cancelled(),
            "the driver must not have been invoked"
        );
    }

    #[tokio::test]
    async fn a_driver_over_another_database_is_refused_before_proving() {
        let executor = executor();
        decided_ballot(&executor);
        let control = ChainSubmissionControl::new(1);
        let foreign = host_database_for("wallet");

        let failure = executor
            .advance_step(
                NextStep::Delegate { bundle_index: 0 },
                &host_with_delegation(&control, "wallet", &foreign),
                &control,
                &NoopRoundStepProgressReporter {},
            )
            .await
            .expect_err("a driver over another sidecar must not run");

        assert_eq!(failure.kind, RoundStepFailureKind::InvalidInput);
        assert!(
            failure.message.contains("different voting database"),
            "{}",
            failure.message
        );
        assert!(!control.is_cancelled());
    }

    #[test]
    fn a_binding_requires_a_nonempty_distinct_roster() {
        let binding = |proposals: Vec<ProposalRosterEntry>| RoundBinding {
            round_id: ROUND_ID.to_string(),
            network: Network::Testnet,
            proposals,
            hotkey_secret: None,
        };
        let unbound = || {
            let (executor, _) = bound_executor_unbound(host_database());
            executor
        };

        let error = unbound()
            .with_binding(binding(Vec::new()))
            .err()
            .expect("an empty roster must be rejected");
        assert!(matches!(error, VotingError::InvalidInput { .. }), "{error}");
        assert!(error.to_string().contains("nonempty"), "{error}");

        let error = unbound()
            .with_binding(binding(vec![
                ProposalRosterEntry {
                    proposal_id: 4,
                    num_options: 2,
                },
                ProposalRosterEntry {
                    proposal_id: 4,
                    num_options: 3,
                },
            ]))
            .err()
            .expect("a repeated proposal must be rejected");
        assert!(matches!(error, VotingError::InvalidInput { .. }), "{error}");
        assert!(error.to_string().contains("proposal 4"), "{error}");

        assert!(unbound()
            .with_binding(binding(vec![ProposalRosterEntry {
                proposal_id: 4,
                num_options: 2,
            }]))
            .is_ok());
    }

    #[tokio::test]
    async fn a_delegate_step_stops_when_the_host_moves_to_a_new_operation_epoch() {
        let executor = executor();
        decided_ballot(&executor);
        let control = ChainSubmissionControl::new(7);
        let step = NextStep::Delegate { bundle_index: 0 };

        let outcome = executor
            .advance_step(
                step.clone(),
                &host_with_interrupting_delegation(
                    &control,
                    Interrupt::NewOperationEpoch,
                    "wallet",
                    &executor.database(),
                ),
                &control,
                &NoopRoundStepProgressReporter {},
            )
            .await
            .unwrap();

        // Not cancelled, but the epoch the step started under is gone: the
        // step must not dispatch to the chain on behalf of epoch 7.
        assert!(!control.is_cancelled());
        assert_eq!(control.operation_epoch(), 8);
        assert_eq!(outcome.disposition, RoundStepDisposition::Cancelled);
        assert!(outcome.delegation.is_some());
    }

    #[tokio::test]
    async fn a_sync_that_fails_after_cancellation_reports_cancelled_not_a_transport_error() {
        let control = ChainSubmissionControl::new(1);
        let (executor, transport) =
            executor_ready_to_cast_with("wallet-cancel-in-flight", Some(control.clone()));
        let cast = NextStep::CastVote {
            bundle_index: 0,
            proposal_id: 1,
            choice: 0,
        };
        let host = RoundHostContext {
            vote_tree_node_urls: vec![
                "http://node-a.invalid".to_string(),
                "http://node-b.invalid".to_string(),
            ],
            ..host()
        };

        let outcome = executor
            .advance_step(
                cast.clone(),
                &host,
                &control,
                &NoopRoundStepProgressReporter {},
            )
            .await
            .expect("a cancelled step is an outcome, not a failure");

        let cached = crate::precompute::cached_vote_tree_rounds(&executor.database())
            .contains(&ROUND_ID.to_string());
        crate::precompute::reset_vote_tree(&executor.database(), "").unwrap();
        assert_eq!(outcome.disposition, RoundStepDisposition::Cancelled);
        assert_eq!(outcome.step, Some(cast));
        assert_eq!(
            transport.requests.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the second node must not be tried after cancellation"
        );
        assert!(!cached, "the poisoned tree is still reset before returning");
    }

    #[test]
    fn a_binding_for_another_network_is_refused() {
        let (executor, _) = bound_executor_unbound(host_database());
        let error = executor
            .with_binding(RoundBinding {
                round_id: ROUND_ID.to_string(),
                network: Network::Mainnet,
                proposals: vec![ProposalRosterEntry {
                    proposal_id: 1,
                    num_options: 2,
                }],
                hotkey_secret: None,
            })
            .err()
            .expect("the chain client is configured for Testnet");
        assert!(matches!(error, VotingError::InvalidInput { .. }), "{error}");
        assert!(error.to_string().contains("Mainnet"), "{error}");
    }

    #[tokio::test]
    async fn a_driver_for_another_network_is_refused_before_proving() {
        let executor = executor();
        decided_ballot(&executor);
        let control = ChainSubmissionControl::new(1);

        let failure = executor
            .advance_step(
                NextStep::Delegate { bundle_index: 0 },
                &host_with_driver(
                    &control,
                    Interrupt::Cancel,
                    Network::Mainnet,
                    "wallet",
                    &executor.database(),
                ),
                &control,
                &NoopRoundStepProgressReporter {},
            )
            .await
            .expect_err("a Mainnet driver must not prove for a Testnet binding");

        assert_eq!(failure.kind, RoundStepFailureKind::InvalidInput);
        assert!(failure.message.contains("Mainnet"), "{}", failure.message);
        assert!(
            !control.is_cancelled(),
            "the driver must not have been invoked"
        );
    }

    #[tokio::test]
    async fn an_epoch_change_during_resigning_stops_delegation_advancement() {
        let executor = executor();
        // A submitted delegation with no confirmed position plans as
        // AdvanceDelegation.
        executor
            .database()
            .store_delegation_tx_hash(ROUND_ID, 0, "dtx")
            .unwrap();
        let step = NextStep::AdvanceDelegation { bundle_index: 0 };
        assert!(executor.plan().unwrap().next_steps.contains(&step));
        let control = ChainSubmissionControl::new(7);

        let outcome = executor
            .advance_step(
                step.clone(),
                &host_with_interrupting_delegation(
                    &control,
                    Interrupt::NewOperationEpoch,
                    "wallet",
                    &executor.database(),
                ),
                &control,
                &NoopRoundStepProgressReporter {},
            )
            .await
            .expect("an interrupted step is an outcome, not a chain failure");

        // The chain endpoint is unreachable, so reaching it would have failed
        // with a transport error; the epoch check must come first.
        assert_eq!(control.operation_epoch(), 8);
        assert_eq!(outcome.disposition, RoundStepDisposition::Cancelled);
        assert_eq!(outcome.step, Some(step));
    }

    #[tokio::test]
    async fn a_driver_for_another_hotkey_than_the_binding_is_refused_before_proving() {
        // The binding votes with hotkey 0x21; the driver would delegate to 0x22.
        let (executor, _) = bound_executor(
            host_database(),
            Some(zeroize::Zeroizing::new(vec![0x21; 64])),
        );
        decided_ballot(&executor);
        let control = ChainSubmissionControl::new(1);

        let failure = executor
            .advance_step(
                NextStep::Delegate { bundle_index: 0 },
                &host_with_driver_target(
                    &control,
                    Interrupt::Cancel,
                    Network::Testnet,
                    Some(hotkey_target(0x22)),
                    "wallet",
                    &executor.database(),
                ),
                &control,
                &NoopRoundStepProgressReporter {},
            )
            .await
            .expect_err("a delegation for another hotkey must not be proved");
        assert_eq!(failure.kind, RoundStepFailureKind::InvalidInput);
        assert!(
            failure.message.contains("voting hotkey"),
            "{}",
            failure.message
        );
        assert!(
            !control.is_cancelled(),
            "the driver must not have been invoked"
        );

        // The matching hotkey proceeds to the driver.
        let control = ChainSubmissionControl::new(1);
        let outcome = executor
            .advance_step(
                NextStep::Delegate { bundle_index: 0 },
                &host_with_driver_target(
                    &control,
                    Interrupt::Cancel,
                    Network::Testnet,
                    Some(hotkey_target(0x21)),
                    "wallet",
                    &executor.database(),
                ),
                &control,
                &NoopRoundStepProgressReporter {},
            )
            .await
            .unwrap();
        assert_eq!(outcome.disposition, RoundStepDisposition::Cancelled);
    }

    #[test]
    fn a_binding_for_a_network_other_than_the_stored_round_is_refused() {
        // The fixture round is stored for Testnet; the host binds Mainnet with
        // a Mainnet chain client, which the chain-network check alone accepts.
        let database = host_database();
        let helper_client =
            HelperClient::new(Arc::new(HyperTransport::new()), HelperHealth::default());
        let executor = RoundExecutor::new(
            database,
            ChainSubmissionClientConfig::for_network(
                Network::Mainnet,
                vec!["https://chain.invalid".to_string()],
            ),
            helper_client,
        )
        .unwrap();

        let error = executor
            .with_binding(RoundBinding {
                round_id: ROUND_ID.to_string(),
                network: Network::Mainnet,
                proposals: vec![ProposalRosterEntry {
                    proposal_id: 1,
                    num_options: 2,
                }],
                hotkey_secret: None,
            })
            .err()
            .expect("the stored round is Testnet");
        assert!(matches!(error, VotingError::InvalidInput { .. }), "{error}");
        assert!(
            error.to_string().contains("stored for network Testnet"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_queued_lock_wait_stops_when_the_operation_epoch_changes() {
        let control = ChainSubmissionControl::new(1);
        let held = super::super::round_lock::acquire(
            "epoch-wait-wallet".to_string(),
            ROUND_ID,
            None,
            &control,
            control.operation_epoch(),
        )
        .await
        .unwrap()
        .unwrap();

        let waiter = super::super::round_lock::acquire(
            "epoch-wait-wallet".to_string(),
            ROUND_ID,
            None,
            &control,
            1,
        );
        let switch = async {
            tokio::time::sleep(Duration::from_millis(120)).await;
            control.set_operation_epoch(2);
        };
        let (outcome, ()) = tokio::join!(waiter, switch);

        assert!(
            outcome.unwrap().is_none(),
            "a stale caller must stop queuing"
        );
        assert!(!control.is_cancelled());
        drop(held);
    }

    #[tokio::test]
    async fn recovery_on_a_round_stored_for_another_network_is_refused_before_helper_io() {
        let database = host_database();
        let helper_client =
            HelperClient::new(Arc::new(HyperTransport::new()), HelperHealth::default());
        let executor = RoundExecutor::new(
            database,
            ChainSubmissionClientConfig::for_network(
                Network::Mainnet,
                vec!["https://chain.invalid".to_string()],
            ),
            helper_client,
        )
        .unwrap();
        let control = ChainSubmissionControl::new(1);
        let proposal_ids = [1u32, 2];
        let helper_urls = vec!["http://helper.invalid".to_string()];

        let failure = executor
            .advance(
                crate::VoteRecoveryRequest {
                    round_id: ROUND_ID,
                    proposal_ids: &proposal_ids,
                    configured_helper_urls: &helper_urls,
                    now_seconds: 10,
                    vote_end_time_seconds: 100_000,
                    last_moment_buffer_seconds: None,
                },
                &control,
                &crate::NoopVoteRecoveryProgressReporter {},
            )
            .await
            .expect_err("the stored round is Testnet");

        assert_eq!(failure.kind, crate::VoteRecoveryFailureKind::InvalidInput);
        assert!(
            failure.message.contains("stored for network Testnet"),
            "{}",
            failure.message
        );
    }
}
