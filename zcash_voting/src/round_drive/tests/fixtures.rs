//! Shared fixtures: a bound executor over an in-memory sidecar, a host source
//! that never changes, and a reporter that records every driver event.
//!
//! These deliberately do not reuse `vote_work`'s fixtures. Those are scoped to
//! that module's tests, and the driver is meant to be exercised through the
//! same public surface a host has.

pub(super) use std::sync::{Arc, Mutex};
pub(super) use std::time::Duration;

pub(super) use crate::round_drive::tally::BallotBaseline;
pub(super) use crate::{
    round_drive::{
        FailureIsolation, ProgressBaseline, RoundDriveEvent, RoundDrivePolicy, RoundDriveReporter,
        RoundDriver, RoundHostSource, RoundQuiescence, RoundRunReport, RoundWorkTally,
    },
    session::{Decision, NextStep},
    BallotIntent, ChainAdvancePolicy, ChainSubmissionClientConfig, ChainSubmissionControl,
    HelperClient, HelperFuture, HelperHealth, HelperResponse, HelperTransport, HyperTransport,
    Network, ProposalRosterEntry, RoundBinding, RoundExecutor, RoundHostContext,
};

pub(super) const WALLET_ID: &str = "wallet";

pub(super) const ROUND_ID: &str =
    "0101010101010101010101010101010101010101010101010101010101010101";

fn round_params() -> crate::wire::VotingRoundParams {
    crate::wire::VotingRoundParams {
        vote_round_id: ROUND_ID.to_string(),
        snapshot_height: 1000,
        ea_pk: vec![0xEA; 32],
        nc_root: vec![0xAA; 32],
        nullifier_imt_root: vec![0xBB; 32],
    }
}

/// One eligible note. Distinct `index` values give distinct notes, so a
/// round can be given more bundles than one.
fn note(index: u8) -> crate::NoteInfo {
    crate::NoteInfo {
        commitment: [0x01, index].repeat(16),
        nullifier: [0x02, index].repeat(16),
        value: crate::governance::BALLOT_DIVISOR,
        position: u64::from(index),
        diversifier: vec![0x03; 11],
        rho: [0x04, index].repeat(16),
        rseed: [0x05, index].repeat(16),
        scope: 0,
        ufvk_str: "uview1test".to_string(),
    }
}

/// An in-memory sidecar for wallet "wallet" holding one bundle in the round.
pub(super) fn database() -> Arc<crate::round::VotingDb> {
    database_with_bundles(1)
}

/// An initialized round before eligibility setup has persisted any bundles.
pub(super) fn database_without_bundles() -> Arc<crate::round::VotingDb> {
    let database = Arc::new(crate::round::VotingDb::open_in_memory().unwrap());
    database.set_wallet_id(WALLET_ID);
    database
        .create_round(Network::Testnet, &round_params(), None)
        .unwrap();
    database
}

/// The same sidecar with enough notes to fill `bundle_count` bundles.
pub(super) fn database_with_bundles(bundle_count: usize) -> Arc<crate::round::VotingDb> {
    let database = Arc::new(crate::round::VotingDb::open_in_memory().unwrap());
    database.set_wallet_id(WALLET_ID);
    database
        .create_round(Network::Testnet, &round_params(), None)
        .unwrap();
    let notes: Vec<crate::NoteInfo> = (0..bundle_count * crate::governance::BUNDLE_NOTE_SLOTS)
        .map(|index| note(index as u8))
        .collect();
    let layout = database.ensure_bundles(ROUND_ID, &notes).unwrap();
    assert_eq!(layout.bundle_count as usize, bundle_count);
    database
}

/// An executor bound to the round with a two-proposal roster.
pub(super) fn executor() -> RoundExecutor<HyperTransport> {
    executor_over(database())
}

pub(super) fn executor_over(
    database: Arc<crate::round::VotingDb>,
) -> RoundExecutor<HyperTransport> {
    let helper_client = HelperClient::new(Arc::new(HyperTransport::new()), HelperHealth::default());
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
        hotkey_secret: Some(zeroize::Zeroizing::new(vec![HOTKEY_SECRET_BYTE; 64])),
    })
    .unwrap()
}

/// The stored hotkey secret the fixture binds, and the delegation target it
/// derives. A driver must claim the same target or the executor refuses it.
pub(super) const HOTKEY_SECRET_BYTE: u8 = 0x21;

pub(super) fn hotkey_target() -> crate::VotingHotkeyTarget {
    crate::VotingHotkey::from_stored_secret(&[HOTKEY_SECRET_BYTE; 64], Network::Testnet)
        .unwrap()
        .delegation_target()
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

/// A host source that returns the shared test context on every dispatch.
pub(super) struct FixedHost;

impl RoundHostSource for FixedHost {
    fn host_context(&self) -> RoundHostContext {
        host()
    }
}

/// Records every driver event in order.
#[derive(Default)]
pub(super) struct RecordingReporter {
    pub(super) events: Mutex<Vec<RoundDriveEvent>>,
}

impl RoundDriveReporter for RecordingReporter {
    fn report(&self, event: RoundDriveEvent) {
        self.events.lock().unwrap().push(event);
    }
}

/// A step failure with nothing durable attached, for tests about what the run
/// does with a failure rather than what produced it.
pub(super) fn step_failure(message: &str) -> crate::RoundStepFailure {
    crate::RoundStepFailure {
        kind: crate::RoundStepFailureKind::Storage,
        step: None,
        strongest_chain_state: None,
        chain_outcome: None,
        message: message.to_string(),
        plan: None,
        share_deliveries: Vec::new(),
        delegation: None,
    }
}

/// A signed delegation bundle, for tests about what a run keeps rather than
/// what produced it.
pub(super) fn signed_delegation() -> crate::delegate::SignedDelegationBundle {
    crate::delegate::SignedDelegationBundle {
        submission: crate::delegate::DelegationSubmission {
            proof: vec![0x61; 96],
            rk: [0x62; 32],
            nf_signed: [0x63; 32],
            cmx_new: [0x64; 32],
            gov_comm: [0x65; 32],
            gov_nullifiers: [[0x66; 32]; crate::governance::BUNDLE_NOTE_SLOTS],
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
        bundle_index: 0,
    }
}

/// Runs `executor`'s round to quiescence with the default policy.
pub(super) async fn drive(
    executor: &RoundExecutor<HyperTransport>,
    control: &ChainSubmissionControl,
) -> (RoundRunReport, RecordingReporter) {
    let events = RecordingReporter::default();
    let report = RoundDriver::new(executor)
        .run(&FixedHost, control, &events)
        .await;
    (report, events)
}

/// A delegation driver that proves and signs without touching a network.
///
/// Paired with [`UnreachableChain`], a bundle that gets this far still fails at
/// dispatch. That is what the failure isolation tests need: a step that runs
/// and then fails, per bundle.
pub(super) struct SigningDriver {
    pub(super) database: Arc<crate::round::VotingDb>,
}

impl crate::DelegationDriver for SigningDriver {
    fn round_id(&self) -> &str {
        ROUND_ID
    }

    fn network(&self) -> Network {
        Network::Testnet
    }

    fn delegation_target(&self) -> Option<crate::VotingHotkeyTarget> {
        Some(hotkey_target())
    }

    fn wallet_id(&self) -> &str {
        WALLET_ID
    }

    fn shares_database_with(&self, database: &crate::round::VotingDb) -> bool {
        self.database.shares_connection_with(database)
    }

    fn prove_and_sign_blocking(
        &self,
        bundle_index: u32,
        _signer: &crate::DelegationSigner,
        _pir: &crate::PirFleet,
        progress: &dyn crate::types::DelegationProgressReporter,
    ) -> Result<crate::delegate::SignedDelegationBundle, crate::VotingError> {
        progress.on_progress(crate::delegate::DelegationProgress::PayloadReady);
        Ok(crate::delegate::SignedDelegationBundle {
            submission: crate::delegate::DelegationSubmission {
                proof: vec![0x61; 96],
                rk: [0x62; 32],
                nf_signed: [0x63; 32],
                cmx_new: [0x64; 32],
                gov_comm: [0x65; 32],
                gov_nullifiers: [[0x66; 32]; crate::governance::BUNDLE_NOTE_SLOTS],
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
        _signer: &crate::DelegationSigner,
    ) -> Result<[u8; 64], crate::VotingError> {
        Ok([0x68; 64])
    }
}

/// A host source whose context carries the signing driver above.
pub(super) struct SigningHost {
    pub(super) database: Arc<crate::round::VotingDb>,
}

impl RoundHostSource for SigningHost {
    fn host_context(&self) -> RoundHostContext {
        signing_host_context(
            &self.database,
            crate::DelegationSigner::Keystone(crate::KeystoneSignatureSource::Provided {
                sig: vec![0x68; 64],
                sighash: vec![0x69; 32],
            }),
        )
    }
}

/// A host that asks the pipeline to recover Keystone signatures from storage.
pub(super) struct StoredSigningHost {
    pub(super) database: Arc<crate::round::VotingDb>,
}

impl RoundHostSource for StoredSigningHost {
    fn host_context(&self) -> RoundHostContext {
        signing_host_context(
            &self.database,
            crate::DelegationSigner::Keystone(crate::KeystoneSignatureSource::Stored),
        )
    }
}

/// A host whose signer changes between samples.
///
/// `RoundHostSource` is read once per dispatch and nothing requires two
/// samples to agree, so a driver that treated the first as representative
/// would mis-handle every later bundle in the same wave. The first sample
/// carries its own signature; every one after it reads stored material, which
/// must already exist for every bundle the round owes.
pub(super) struct DriftingSigningHost {
    pub(super) database: Arc<crate::round::VotingDb>,
    pub(super) samples: std::sync::atomic::AtomicUsize,
}

impl DriftingSigningHost {
    pub(super) fn new(database: Arc<crate::round::VotingDb>) -> Self {
        Self {
            database,
            samples: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

impl RoundHostSource for DriftingSigningHost {
    fn host_context(&self) -> RoundHostContext {
        let sample = self
            .samples
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let signer = if sample == 0 {
            crate::DelegationSigner::Keystone(crate::KeystoneSignatureSource::Provided {
                sig: vec![0x68; 64],
                sighash: vec![0x69; 32],
            })
        } else {
            crate::DelegationSigner::Keystone(crate::KeystoneSignatureSource::Stored)
        };
        signing_host_context(&self.database, signer)
    }
}

/// The context `StoredSigningHost` returns, for tests that mix it with others.
pub(super) fn stored_signing_context(database: &Arc<crate::round::VotingDb>) -> RoundHostContext {
    signing_host_context(
        database,
        crate::DelegationSigner::Keystone(crate::KeystoneSignatureSource::Stored),
    )
}

fn signing_host_context(
    database: &Arc<crate::round::VotingDb>,
    signer: crate::DelegationSigner,
) -> RoundHostContext {
    RoundHostContext {
        delegation: Some(crate::DelegationStepInputs {
            driver: Arc::new(SigningDriver {
                database: Arc::clone(database),
            }),
            signer,
            pir: Arc::new(
                crate::PirFleet::new(
                    &["http://pir.invalid".to_string()],
                    compiled_pir_layout(),
                    Arc::new(HyperTransport::new()),
                )
                .unwrap(),
            ),
        }),
        ..host()
    }
}

/// Records both proposals as choices, so the bundle's cast is due and its
/// delegation becomes the plan's first step.
pub(super) fn decide_ballot<T: crate::ChainTransport>(executor: &RoundExecutor<T>) {
    executor
        .set_ballot_intents(&[
            BallotIntent {
                proposal_id: 1,
                decision: Decision::Choice(0),
            },
            BallotIntent {
                proposal_id: 2,
                decision: Decision::Choice(1),
            },
        ])
        .unwrap();
}

/// The layout this build's PIR parameters were compiled for. A fleet refuses
/// an unknown layout, and these tests never reach a PIR request anyway.
fn compiled_pir_layout() -> crate::config::PirLayout {
    crate::config::PirLayout {
        pir_depth: u32::try_from(pir_types::COMPILED_PIR_LAYOUT.pir_depth).unwrap(),
        tier0_layers: u32::try_from(pir_types::COMPILED_PIR_LAYOUT.tier0_layers).unwrap(),
        tier1_layers: u32::try_from(pir_types::COMPILED_PIR_LAYOUT.tier1_layers).unwrap(),
        poly_len: pir_types::DEFAULT_YPIR_POLY_LEN as u32,
    }
}

/// A chain transport that answers from a scripted queue and counts requests.
///
/// The driver's chain-facing behaviour — re-polling a tracking submission,
/// stopping on a terminal one — is only observable against real chain
/// responses, and an imported delegation reaches them with GETs alone: no
/// proving, no signer, and no POST body to construct.
#[derive(Default)]
pub(super) struct ScriptedChain {
    responses: Mutex<std::collections::VecDeque<crate::ChainHttpResponse>>,
    pub(super) gets: Mutex<usize>,
}

impl ScriptedChain {
    pub(super) fn queue(&self, status: u16, body: impl Into<Vec<u8>>) {
        self.responses
            .lock()
            .unwrap()
            .push_back(crate::ChainHttpResponse::json(status, body.into()));
    }

    /// A poll that finds nothing yet: the submission stays `Tracking`.
    pub(super) fn queue_not_found(&self) {
        self.queue(404, br#"{"error":"tx not found"}"#.to_vec());
    }

    /// A poll that confirms the delegation at VAN position 7.
    pub(super) fn queue_confirmed(&self) {
        self.queue(
            200,
            format!(
                r#"{{"height":"42","code":0,"log":"","events":[{{"type":"delegate_vote","attributes":[{{"key":"vote_round_id","value":"{ROUND_ID}","index":true}},{{"key":"leaf_index","value":"7","index":true}}]}}]}}"#
            ),
        );
    }

    /// A poll that reports the transaction rejected.
    pub(super) fn queue_rejected(&self) {
        self.queue(
            422,
            br#"{"height":"9","code":12,"log":"rejected","events":[]}"#.to_vec(),
        );
    }
}

impl crate::ChainTransport for Arc<ScriptedChain> {
    fn chain_get<'a>(
        &'a self,
        _request: crate::ChainHttpRequest,
    ) -> crate::ChainTransportFuture<'a> {
        Box::pin(async move {
            *self.gets.lock().unwrap() += 1;
            Ok(self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("a scripted chain response for every poll"))
        })
    }

    fn chain_post_json<'a>(
        &'a self,
        _request: crate::ChainHttpRequest,
        _json: Vec<u8>,
    ) -> crate::ChainTransportFuture<'a> {
        Box::pin(async move { panic!("these tests never dispatch a transaction") })
    }
}

/// A chain that is never reachable, without involving a network.
///
/// The failure-isolation and concurrency tests need a step that runs and then
/// fails, once per bundle. They got that by pointing the executor at a
/// hostname under `.invalid` and relying on the machine's resolver to refuse
/// it, which makes a hermetic test depend on how and how fast DNS fails. This
/// returns the same `DefinitelyUnsent` a refused connection produces, with
/// nothing outside the process involved.
pub(super) struct UnreachableChain;

impl crate::ChainTransport for Arc<UnreachableChain> {
    fn chain_get<'a>(
        &'a self,
        _request: crate::ChainHttpRequest,
    ) -> crate::ChainTransportFuture<'a> {
        Box::pin(async {
            Err(crate::ChainTransportError::definitely_unsent(
                "the chain is unreachable",
            ))
        })
    }

    fn chain_post_json<'a>(
        &'a self,
        _request: crate::ChainHttpRequest,
        _json: Vec<u8>,
    ) -> crate::ChainTransportFuture<'a> {
        Box::pin(async {
            Err(crate::ChainTransportError::definitely_unsent(
                "the chain is unreachable",
            ))
        })
    }
}

/// The two-proposal executor of [`executor_over`], over a chain that always
/// refuses, so a dispatch fails for a reason the test states rather than one
/// the environment supplies.
pub(super) fn executor_over_unreachable_chain(
    database: Arc<crate::round::VotingDb>,
) -> RoundExecutor<Arc<UnreachableChain>> {
    let helper_client = HelperClient::new(Arc::new(HyperTransport::new()), HelperHealth::default());
    RoundExecutor::with_transport(
        database,
        Arc::new(UnreachableChain),
        ChainSubmissionClientConfig::for_network(
            Network::Testnet,
            vec!["https://vote.example".to_string()],
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
        hotkey_secret: Some(zeroize::Zeroizing::new(vec![HOTKEY_SECRET_BYTE; 64])),
    })
    .unwrap()
}

/// The transaction hash the imported delegation below is adopted from.
pub(super) const IMPORTED_TX_HASH: &str =
    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// A sidecar whose single bundle carries an already-broadcast delegation, so
/// the plan lists `AdvanceImportedDelegation` and nothing else.
pub(super) fn database_with_imported_delegation() -> Arc<crate::round::VotingDb> {
    let database = Arc::new(crate::round::VotingDb::open_in_memory().unwrap());
    database.set_wallet_id(WALLET_ID);
    database
        .create_round(Network::Testnet, &round_params(), None)
        .unwrap();
    database
        .conn()
        .execute(
            "INSERT INTO bundles
             (round_id, wallet_id, bundle_index, van_comm_rand, gov_comm,
              total_note_value, address_index, delegation_tx_hash)
             VALUES (:round, :wallet, 0, :randomizer, :commitment, 100000000, 0, :hash)",
            rusqlite::named_params! {
                ":round": ROUND_ID,
                ":wallet": WALLET_ID,
                ":randomizer": vec![0x21_u8; 32],
                ":commitment": vec![0x31_u8; 32],
                ":hash": IMPORTED_TX_HASH,
            },
        )
        .unwrap();
    database
}

/// An executor over the scripted chain, bound to the same round.
pub(super) fn executor_over_chain(
    database: Arc<crate::round::VotingDb>,
    chain: Arc<ScriptedChain>,
) -> RoundExecutor<Arc<ScriptedChain>> {
    let helper_client = HelperClient::new(Arc::new(HyperTransport::new()), HelperHealth::default());
    RoundExecutor::with_transport(
        database,
        chain,
        ChainSubmissionClientConfig::for_network(
            Network::Testnet,
            vec!["https://vote.example".to_string()],
        ),
        helper_client,
    )
    .unwrap()
    .with_binding(RoundBinding {
        round_id: ROUND_ID.to_string(),
        network: Network::Testnet,
        proposals: vec![ProposalRosterEntry {
            proposal_id: 1,
            num_options: 2,
        }],
        hotkey_secret: Some(zeroize::Zeroizing::new(vec![HOTKEY_SECRET_BYTE; 64])),
    })
    .unwrap()
}

/// A host whose chain episodes end after one pass.
///
/// `ChainAdvancePolicy::max_passes` bounds one `advance_step` call, and its
/// default of 45 lets a submission confirm inside a single dispatch. Capping
/// it at one pass is what makes the driver's own re-poll — the wait *between*
/// episodes — observable at all.
pub(super) struct SinglePassHost;

impl RoundHostSource for SinglePassHost {
    fn host_context(&self) -> RoundHostContext {
        RoundHostContext {
            chain_policy: ChainAdvancePolicy {
                max_passes: 1,
                ..host().chain_policy
            },
            ..host()
        }
    }
}

/// An executor bound to the single-proposal round `db_with_share` builds.
pub(super) fn executor_over_share_round(
    database: Arc<crate::round::VotingDb>,
) -> RoundExecutor<HyperTransport> {
    let helper_client = HelperClient::new(Arc::new(HyperTransport::new()), HelperHealth::default());
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
        proposals: vec![ProposalRosterEntry {
            proposal_id: 1,
            num_options: 3,
        }],
        hotkey_secret: None,
    })
    .unwrap()
}

/// A helper transport that accepts every share POST and refuses unexpected
/// status polling.
///
/// Round-driver tests use it to prove that tracking-owned shares stay out of
/// the foreground while a later deliverable share can complete.
#[derive(Default)]
pub(super) struct AcceptingHelper {
    pub(super) posts: Mutex<usize>,
}

impl HelperTransport for AcceptingHelper {
    fn get<'a>(&'a self, url: &'a str, _timeout: Duration) -> HelperFuture<'a> {
        let is_share_poll = url.contains("/share-status/");
        Box::pin(async move {
            assert!(
                !is_share_poll,
                "the foreground must not poll tracking-owned shares"
            );
            Ok(HelperResponse::json(200, br#"{"status":"ok"}"#.to_vec()))
        })
    }

    fn post_json<'a>(
        &'a self,
        _url: &'a str,
        _body: Vec<u8>,
        _timeout: Duration,
    ) -> HelperFuture<'a> {
        *self.posts.lock().unwrap() += 1;
        Box::pin(async {
            Ok(HelperResponse::json(
                200,
                br#"{"status":"queued"}"#.to_vec(),
            ))
        })
    }
}

/// The single-proposal share-round executor over an accepting helper.
pub(super) fn executor_over_accepting_helper(
    database: Arc<crate::round::VotingDb>,
    helper: Arc<AcceptingHelper>,
) -> RoundExecutor<HyperTransport> {
    let helper_client = HelperClient::new(helper, HelperHealth::default());
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
        proposals: vec![ProposalRosterEntry {
            proposal_id: 1,
            num_options: 3,
        }],
        hotkey_secret: None,
    })
    .unwrap()
}
