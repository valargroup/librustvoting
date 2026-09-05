//! Shared fixtures for the round-executor behaviour tests: an in-memory
//! sidecar with one bundle, a bound executor, host contexts, a mock
//! delegation driver that interrupts the host control, and an unreachable
//! vote-tree transport.

pub(super) use crate::{
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
pub(super) use std::{sync::Arc, time::Duration};

pub(super) const ROUND_ID: &str =
    "0101010101010101010101010101010101010101010101010101010101010101";

pub(super) fn round_params() -> VotingRoundParams {
    VotingRoundParams {
        vote_round_id: ROUND_ID.to_string(),
        snapshot_height: 1000,
        ea_pk: vec![0xEA; 32],
        nc_root: vec![0xAA; 32],
        nullifier_imt_root: vec![0xBB; 32],
    }
}

/// One eligible note, so a choice intent has a bundle to plan against.
pub(super) fn note() -> crate::NoteInfo {
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

pub(super) fn executor() -> RoundExecutor<HyperTransport> {
    executor_over(host_database()).0
}

/// The host's own handle: wallet "wallet" with one bundle in the round.
pub(super) fn host_database() -> Arc<crate::round::VotingDb> {
    host_database_for("wallet")
}

/// An in-memory sidecar scoped to `wallet_id` with no round stored yet.
pub(super) fn host_database_for_wallet_without_round(
    wallet_id: &str,
) -> Arc<crate::round::VotingDb> {
    let db = crate::round::VotingDb::open_in_memory().unwrap();
    db.set_wallet_id(wallet_id);
    Arc::new(db)
}

pub(super) fn host_database_for(wallet_id: &str) -> Arc<crate::round::VotingDb> {
    let database = Arc::new(crate::round::VotingDb::open_in_memory().unwrap());
    database.set_wallet_id(wallet_id);
    database
        .create_round(Network::Testnet, &round_params(), None)
        .unwrap();
    database.ensure_bundles(ROUND_ID, &[note()]).unwrap();
    database
}

pub(super) fn executor_over(
    database: Arc<crate::round::VotingDb>,
) -> (RoundExecutor<HyperTransport>, Arc<crate::round::VotingDb>) {
    bound_executor(database, None)
}

pub(super) fn bound_executor_unbound(
    database: Arc<crate::round::VotingDb>,
) -> (RoundExecutor<HyperTransport>, Arc<crate::round::VotingDb>) {
    let helper_client = HelperClient::new(Arc::new(HyperTransport::new()), HelperHealth::default());
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

pub(super) fn bound_executor(
    database: Arc<crate::round::VotingDb>,
    hotkey_secret: Option<zeroize::Zeroizing<Vec<u8>>>,
) -> (RoundExecutor<HyperTransport>, Arc<crate::round::VotingDb>) {
    let helper_client = HelperClient::new(Arc::new(HyperTransport::new()), HelperHealth::default());
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

pub(super) fn host() -> RoundHostContext {
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

/// How the mock driver interrupts the host control from inside signing.
#[derive(Clone, Copy)]
pub(super) enum Interrupt {
    Cancel,
    NewOperationEpoch,
}

/// A driver that signs instantly and interrupts the host control from
/// inside the signing thread, so the executor observes the interruption
/// after the payload is signed and before chain dispatch.
pub(super) struct CancelAfterSigningDriver {
    pub(super) control: ChainSubmissionControl,
    pub(super) interrupt: Interrupt,
    pub(super) network: Network,
    pub(super) target: Option<crate::VotingHotkeyTarget>,
    pub(super) wallet_id: String,
    pub(super) database: Arc<crate::round::VotingDb>,
}

pub(super) fn hotkey_target(secret_byte: u8) -> crate::VotingHotkeyTarget {
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

pub(super) fn host_with_delegation(
    control: &ChainSubmissionControl,
    driver_wallet_id: &str,
    database: &Arc<crate::round::VotingDb>,
) -> RoundHostContext {
    host_with_interrupting_delegation(control, Interrupt::Cancel, driver_wallet_id, database)
}

pub(super) fn host_with_interrupting_delegation(
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

pub(super) fn host_with_driver(
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

pub(super) fn host_with_driver_target(
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
                        pir_depth: u32::try_from(pir_types::COMPILED_PIR_LAYOUT.pir_depth).unwrap(),
                        tier0_layers: u32::try_from(pir_types::COMPILED_PIR_LAYOUT.tier0_layers)
                            .unwrap(),
                        tier1_layers: u32::try_from(pir_types::COMPILED_PIR_LAYOUT.tier1_layers)
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

/// A vote-tree transport with no reachable node: every request fails
/// after being counted, so a sync creates the round's tree client and then
/// errors out of it. It can also cancel the host control from inside the
/// request, modelling a cancellation that arrives while a sync is in flight.
pub(super) struct UnreachableTreeTransport {
    pub(super) requests: std::sync::atomic::AtomicUsize,
    pub(super) cancel_on_request: Option<ChainSubmissionControl>,
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
pub(super) fn executor_ready_to_cast(
    wallet_id: &str,
) -> (RoundExecutor<HyperTransport>, Arc<UnreachableTreeTransport>) {
    executor_ready_to_cast_with(wallet_id, None)
}

pub(super) fn executor_ready_to_cast_with(
    wallet_id: &str,
    cancel_on_request: Option<ChainSubmissionControl>,
) -> (RoundExecutor<HyperTransport>, Arc<UnreachableTreeTransport>) {
    executor_ready_to_cast_with_hotkey_and_control(wallet_id, 0x21, cancel_on_request)
}

pub(super) fn executor_ready_to_cast_with_hotkey_and_control(
    wallet_id: &str,
    bound_hotkey_byte: u8,
    cancel_on_request: Option<ChainSubmissionControl>,
) -> (RoundExecutor<HyperTransport>, Arc<UnreachableTreeTransport>) {
    // Regtest activates NU6.3 at a low height, which the Ironwood
    // governance-output derivation below requires of the round snapshot.
    let network = Network::Regtest;
    let round_params = VotingRoundParams {
        snapshot_height: u64::from(crate::types::REGTEST_NU6_3_ACTIVATION_HEIGHT),
        ..round_params()
    };
    let database = Arc::new(crate::round::VotingDb::open_in_memory().unwrap());
    database.set_wallet_id(wallet_id);
    database.create_round(network, &round_params, None).unwrap();
    database.ensure_bundles(ROUND_ID, &[note()]).unwrap();

    // The stored delegation rows derive from hotkey 0x21's target: the
    // governance output commitment and the target-bound VAN commitment
    // both have to reproduce for CastVote to accept the bound hotkey.
    let target = crate::VotingHotkey::from_stored_secret(&[0x21; 64], network)
        .unwrap()
        .delegation_target();
    let (rho_signed, van_comm_rand) = {
        use crate::backend::pasta_curves::{group::ff::PrimeField, pallas};
        (
            pallas::Base::from(5u64).to_repr(),
            pallas::Base::from(9u64).to_repr(),
        )
    };
    let rseed_output = [0x47u8; 32];
    let cmx_new = crate::action::derive_governance_output_cmx(
        target.raw_orchard_address(),
        &rho_signed,
        &rseed_output,
        network,
        round_params.snapshot_height,
    )
    .unwrap();
    let van_commitment = {
        let (g_d_x, pk_d_x) =
            crate::action::derive_hotkey_x_coords_from_raw_address(target.raw_orchard_address())
                .unwrap();
        crate::governance::construct_van(
            &g_d_x,
            &pk_d_x,
            crate::governance::BALLOT_DIVISOR,
            &hex::decode(ROUND_ID).unwrap(),
            &van_comm_rand,
        )
        .unwrap()
    };
    crate::storage::queries::store_delegation_data(
        &database.conn(),
        ROUND_ID,
        wallet_id,
        0,
        &van_comm_rand,
        &[],
        &rho_signed,
        &[],
        &[0x43; 32],
        &cmx_new,
        &[0x45; 32],
        &[0x46; 32],
        &rseed_output,
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

    let helper_client = HelperClient::new(Arc::new(HyperTransport::new()), HelperHealth::default());
    let executor = RoundExecutor::new(
        database,
        ChainSubmissionClientConfig::for_network(network, vec!["http://chain.invalid".to_string()]),
        helper_client,
    )
    .unwrap()
    .with_binding(RoundBinding {
        round_id: ROUND_ID.to_string(),
        network,
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
        hotkey_secret: Some(zeroize::Zeroizing::new(vec![bound_hotkey_byte; 64])),
    })
    .unwrap();
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

pub(super) async fn cast_against_unreachable_nodes(
    wallet_id: &str,
    node_urls: Vec<String>,
) -> usize {
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

pub(super) fn decided_ballot(executor: &RoundExecutor<HyperTransport>) {
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

/// Records every progress event a step reports.
#[derive(Default)]
pub(super) struct RecordingProgress {
    pub(super) events: std::sync::Mutex<Vec<crate::RoundStepProgress>>,
}

impl crate::RoundStepProgressReporter for RecordingProgress {
    fn report(&self, progress: crate::RoundStepProgress) {
        self.events.lock().unwrap().push(progress);
    }
}

/// A chain transport that never reaches a node: every request fails as
/// definitely unsent, and the first one cancels `control` so the step ends
/// as soon as the chain was consulted.
pub(super) struct UnreachableChainTransport {
    pub(super) requests: std::sync::atomic::AtomicUsize,
    control: ChainSubmissionControl,
}

impl UnreachableChainTransport {
    pub(super) fn cancelling(control: &ChainSubmissionControl) -> Arc<Self> {
        Arc::new(Self {
            requests: std::sync::atomic::AtomicUsize::new(0),
            control: control.clone(),
        })
    }

    fn fail(&self) -> crate::chain_submission::ChainTransportFuture<'_> {
        self.requests
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.control.cancel();
        Box::pin(async {
            Err(
                crate::chain_submission::ChainTransportError::definitely_unsent(
                    "chain unreachable",
                ),
            )
        })
    }
}

impl crate::chain_submission::ChainTransport for UnreachableChainTransport {
    fn chain_get<'a>(
        &'a self,
        _request: crate::chain_submission::ChainHttpRequest,
    ) -> crate::chain_submission::ChainTransportFuture<'a> {
        self.fail()
    }

    fn chain_post_json<'a>(
        &'a self,
        _request: crate::chain_submission::ChainHttpRequest,
        _json: Vec<u8>,
    ) -> crate::chain_submission::ChainTransportFuture<'a> {
        self.fail()
    }
}
