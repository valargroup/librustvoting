//! Client-side APIs for Zcash shielded voting.
//!
//! Wallet SDKs should import [`prelude`] and follow the lifecycle:
//! create a round, bind eligible notes into bundles, precompute witness/PIR
//! data, build a delegation PCZT, prove delegation, sync the vote commitment
//! tree, cast votes with `vote::commit`, drive chain submission through
//! [`ChainSubmissionClient`], then recover helper-share payloads through
//! `share`. New integrations should use `round`, `precompute`, `delegate`,
//! `vote`, `chain_submission`, `share`, and `session` rather than writing
//! storage rows directly. Exact commitment-tree recovery is explicit per
//! advancement call; status-only advancement remains the default.
//!
//! [`ChainSubmissionClient`] is the sole authority for submitting, polling,
//! recovering, and confirming delegation and cast-vote transactions. Hosts
//! supply an HTTP transport, scheduling, and cancellation; they do not build
//! chain requests, interpret chain events, or record transaction hashes and
//! tree positions. See [`chain_submission`] for the removed version-17
//! mutation APIs and the compile-time checks that keep them removed.

#[cfg(all(feature = "lrz", feature = "zakura"))]
compile_error!("features `lrz` and `zakura` cannot be enabled together");

#[cfg(not(any(feature = "lrz", feature = "zakura")))]
compile_error!("enable exactly one of the `lrz` or `zakura` features");

pub mod action;
pub mod backend;
pub mod chain_submission;
pub mod config;
pub(crate) mod confirmation;
pub mod delegate;
pub mod delegation_capability;
pub mod delegation_pipeline;
mod delegation_proof_coordination;
pub mod error;
pub mod governance;
pub mod helper;
pub mod hotkey;
mod http_transport;
pub mod lwd;
pub mod note_bundling;
pub mod observability;
pub(crate) use observability::ObservationScope;
pub use observability::{
    ObservabilityOptions, ObservationAttribution, ObservationOutcome, ObservationRecord,
    ObservationSummary, OperationObservability, OperationReport,
};
pub mod phases;
pub mod pir;
pub mod pir_snapshot;
pub mod precompute;
pub mod prelude;
pub mod recovery;
pub mod round;
pub mod round_auth;
pub mod round_drive;
mod round_planning;
pub mod selection;
pub mod session;
pub mod share;
pub mod share_policy;
pub mod share_tracking;
pub mod share_tracking_drive;
mod shielded_protocol;
pub mod storage;
pub mod transport;
pub mod tree_sync;
pub mod tx1;
pub mod types;
mod van_blinding;
pub mod vote;
pub mod vote_commitment;
pub mod vote_work;
/// Whether a bundle's due proposals are cast as one atomic batch.
///
/// Off while the vote chain has no `cast-vote-batch` route: batching posts to
/// that endpoint, and a chain without it answers 404, which the submission
/// lifecycle can only read as a possibly-dispatched ambiguity. With this off
/// the planner emits one cast per proposal, each a singleton commitment on
/// `cast-vote`, which every deployed chain accepts.
///
/// Turn back on once the batch route ships. Nothing else has to change: the
/// batch paths, their storage, and their recovery are all still here and
/// tested, and a round mid-flight under one setting keeps planning under the
/// shape its durable rows already have.
pub const ATOMIC_VOTE_BATCHES_ENABLED: bool = false;

pub mod wire;
mod wire_codec;
pub mod witness;
pub mod zkp1;
pub mod zkp2;

pub use chain_submission::{
    AdvanceDelegation, AdvanceImportedDelegation, AdvanceVote, AdvanceVoteBatch,
    CandidateTransactionHash, CandidateTransactionHashError, ChainAdvanceOutcome,
    ChainAdvancePolicy, ChainAdvanceRequest, ChainHttpRequest, ChainHttpResponse,
    ChainPostDispatch, ChainRecoveryMode, ChainSubmissionClient, ChainSubmissionClientConfig,
    ChainSubmissionConfirmation, ChainSubmissionConfirmationError,
    ChainSubmissionConfirmationSource, ChainSubmissionControl, ChainSubmissionDiagnostic,
    ChainSubmissionDiagnosticKind, ChainSubmissionFailure, ChainSubmissionFailureKind,
    ChainSubmissionFailureState, ChainSubmissionGeneration, ChainSubmissionGenerationDigest,
    ChainSubmissionIdentity, ChainSubmissionIdentityError, ChainSubmissionPending,
    ChainSubmissionResult, ChainSubmissionState, ChainSubmissionStateEvidence,
    ChainSubmissionTarget, ChainTransport, ChainTransportError, ChainTransportFailureKind,
    ChainTransportFuture, MAX_CHAIN_HTTP_RESPONSE_BYTES, MAX_CHAIN_SUBMISSION_DIAGNOSTIC_BYTES,
};
pub use delegation_pipeline::{
    start_proving_cache_warmup, DelegationDriver, DelegationPipeline, DelegationSigner,
    KeystoneSignatureSource, SpendAuthSigner, SqliteWalletDbOpener, VotingEligibilityReport,
    WalletDbOpener,
};
pub use error::{DelegationSetupField, VotingErrorKind, VotingErrorKindView, VotingErrorView};
pub use helper::client::{
    HelperClient, HelperClientConfig, HelperError, HelperFleetPreflight, ShareStatus,
    ShareSubmissionStatus,
};
pub use helper::health::HelperHealth;
pub use helper::transport::{
    HelperFuture, HelperResponse, HelperTransport, HelperTransportError, MAX_HELPER_RESPONSE_BYTES,
};
pub use http_transport::HyperTransport;
pub use pir::{
    connect_pir, connect_pir_blocking, negotiated_pir_layout, ImtProofData, NegotiatedPirLayout,
    PirClient, PirClientBlocking, Transport, TransportFuture, TransportResponse,
};
pub use pir::{PirFleet, PirProofSource, PirSession};
pub use round_drive::{
    FailureIsolation, NoopRoundDriveReporter, ProgressBaseline, RoundDriveEvent, RoundDrivePolicy,
    RoundDriveReporter, RoundDriveReporterBridge, RoundDriver, RoundHostSource,
    RoundHostSourceBridge, RoundQuiescence, RoundRunReport, RoundStepFailureRecord, RoundWorkTally,
};
pub use share_tracking_drive::{
    NoopShareTrackingReporter, ShareTrackingDrivePolicy, ShareTrackingDriver, ShareTrackingEvent,
    ShareTrackingHostContext, ShareTrackingHostSource, ShareTrackingHostSourceBridge,
    ShareTrackingQuiescence, ShareTrackingReporter, ShareTrackingReporterBridge,
    ShareTrackingRunReport,
};
pub use transport::{
    DirectRoute, PirHttpFailure, PirHttpFailurePhase, RouteError, RouteFuture, RouteHttp,
    RoutePhase, RouteRequest, RouteResponse,
};
pub use vote::{
    persist_prepared_vote_work, prepare_vote_work, recover_vote_commitment, ConfirmedVote,
    PreparedVoteWork, VoteCommitmentRecovery, VoteWorkRequest,
};
pub use vote_work::{
    BallotIntent, DelegationStepInputs, NoopRoundStepProgressReporter, ProposalRosterEntry,
    RoundBinding, RoundExecutor, RoundHostContext, RoundStepDisposition, RoundStepFailure,
    RoundStepFailureKind, RoundStepOutcome, RoundStepProgress, RoundStepProgressBridge,
    RoundStepProgressReporter, VoteRecoveryKey, VoteShareDeliveryReport,
};

pub use delegation_capability::{
    export_delegation_capability, import_delegation_capability, DelegationCapabilityBundleV1,
    DelegationCapabilityDigest, DelegationCapabilityV1, ExportedDelegationCapability,
    ImportDelegationCapabilityParams, MAX_DELEGATION_CAPABILITY_BUNDLES,
    MAX_DELEGATION_CAPABILITY_JSON_BYTES,
};
pub use governance::{BALLOT_DIVISOR, BUNDLE_NOTE_SLOTS};
pub use note_bundling::{
    minimum_voting_eligibility_and_plan_for_notes, minimum_voting_eligibility_for_notes,
    recoverable_bundle_policy_v1, validate_minimum_voting_eligibility_for_notes, voting_power,
    voting_power_for_round, voting_power_with_policy, BundlePolicy, ChunkResult,
    MinimumVotingEligibility, PrivacyTrim, MINIMUM_VOTING_NOTE_COUNT,
    MINIMUM_VOTING_WEIGHT_ZATOSHI,
};
pub use round::validate_bundle_index;
pub use selection::{
    gather_delegation_wallet_inputs, select_notes_with_wallet_db, select_snapshot_notes,
    DelegationWalletInputs, GatherDelegationWalletParams,
};
pub use types::{
    validate_proposal_id, validate_round_params, validate_vote_decision, validate_vote_options,
    CastVoteSignature, DelegationAction, DelegationPirPrecomputeResult, DelegationProgressBridge,
    DelegationProgressReporter, DelegationProofResult, DelegationSubmissionData, EncryptedShare,
    GovernancePczt, Network, NoopProgressReporter, NoteInfo, NoteRef, PirCachePrecomputeResult,
    PirCacheValidationReport, PirProofCacheEntry, PirProofCacheStatus, ProgressReporter,
    RoundBoundVotingHotkeyTarget, SelectedNotes, ShareDelegationRecord, SharePayload,
    VoteCommitStageBridge, VoteCommitStageReporter, VoteCommitmentBundle, VotingError,
    VotingHotkey, VotingHotkeyTarget, VotingRoundParams, WireEncryptedShare, WitnessData,
    MAX_PROPOSAL_ID, MAX_VOTE_OPTIONS, MIN_PROPOSAL_ID, MIN_VOTE_OPTIONS,
};

/// Warms the process-lifetime ZKP #2 proving-key cache.
///
/// The warm-up runs on a large-stack thread and is safe to call repeatedly.
pub fn warm_zkp2_proving_cache() -> Result<(), VotingError> {
    const KEYGEN_STACK_BYTES: usize = 64 * 1024 * 1024;

    std::thread::Builder::new()
        .name("voting-vote-proof-cache-warmup".to_string())
        .stack_size(KEYGEN_STACK_BYTES)
        .spawn(|| {
            voting_circuits::vote_proof::warm_vote_proof_keys().map_err(|e| {
                VotingError::ProofFailed {
                    message: format!("ZKP2 proving cache warm-up failed: {e}"),
                }
            })
        })
        .map_err(|e| VotingError::Internal {
            message: format!("failed to spawn ZKP2 proving cache warm-up thread: {e}"),
        })?
        .join()
        .map_err(|_| VotingError::Internal {
            message: "ZKP2 proving cache warm-up thread panicked".to_string(),
        })?
}

/// Warm process-lifetime proving-key caches used by on-device voting proofs.
///
/// This is intentionally best-effort at the cache layer: callers should invoke
/// it from a background task before the first proof is needed.
pub fn warm_proving_caches() {
    const KEYGEN_STACK_BYTES: usize = 64 * 1024 * 1024;

    let handles = [
        std::thread::Builder::new()
            .name("voting-delegation-cache-warmup".to_string())
            .stack_size(KEYGEN_STACK_BYTES)
            .spawn(|| {
                let _ = voting_circuits::delegation::warm_delegation_keys();
            })
            .expect("spawn delegation proving cache warm-up thread"),
        std::thread::Builder::new()
            .name("voting-vote-proof-cache-warmup".to_string())
            .stack_size(KEYGEN_STACK_BYTES)
            .spawn(|| {
                let _ = voting_circuits::vote_proof::warm_vote_proof_keys();
            })
            .expect("spawn vote proof cache warm-up thread"),
    ];

    for handle in handles {
        handle
            .join()
            .expect("proving cache warm-up thread panicked");
    }
}
