//! Stable wire-format DTOs for vote-chain and helper endpoints.
//!
//! This module is intentionally **struct-only** and is the canonical owner of
//! protocol field names so wallet integrations do not duplicate payload-shaping
//! logic.
//!
//! FRB scans `zcash_voting::wire` directly from `vizor-wallet` to generate
//! Dart bindings. Keeping only plain DTO structs in this module prevents FRB
//! from traversing behavior-level APIs that depend on internal crate types.
//!
//! All conversions, validation, and serialization helpers live in
//! `crate::wire_codec`, while `wire.rs` remains the stable cross-language
//! schema surface.

use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use serde::{Deserialize, Serialize};

pub mod serde_base64_bytes {
    use super::*;

    pub fn serialize<S>(value: &[u8], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&BASE64_STANDARD.encode(value))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let encoded = String::deserialize(deserializer)?;
        let decoded = BASE64_STANDARD
            .decode(encoded.as_bytes())
            .map_err(serde::de::Error::custom)?;
        if BASE64_STANDARD.encode(&decoded) != encoded {
            return Err(serde::de::Error::custom(
                "byte string must use canonical padded standard Base64",
            ));
        }
        Ok(decoded)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BoundedU32(pub u32);

impl TryFrom<usize> for BoundedU32 {
    type Error = std::num::TryFromIntError;

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        Ok(Self(u32::try_from(value)?))
    }
}

impl TryFrom<u64> for BoundedU32 {
    type Error = std::num::TryFromIntError;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        Ok(Self(u32::try_from(value)?))
    }
}

pub use crate::config::{
    ConfigCondition, ConfigConditionKind, ConfigSwitchDecision, ConfigSwitchKind,
    DynamicConfigAttempt, DynamicConfigMirrorFailure, PinnedConfigSource, PirLayout,
    ResolveVotingConfigOptions, ResolvedStaticVotingConfig, ResolvedVotingConfig,
    ResolvedVotingConfigSummary, ServiceEndpoint, SupportedVersions, VotingConfigError,
    WalletCapabilities,
};
pub use crate::delegate::KeystoneSigningRequest;
pub use crate::note_bundling::PrivacyTrim;
pub use crate::round::BundleLayout;
pub use crate::share_policy::{ImmediateShareKey, ShareSubmissionPlan};
pub use crate::types::WireEncryptedShare;
pub use crate::vote::VanWitness;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegationSubmissionWire {
    pub rk: String,
    pub spend_auth_sig: String,
    /// Base64-encoded versioned Ironwood TX1 effecting data.
    pub tx1_effects: String,
    #[serde(rename = "signed_note_nullifier")]
    pub nf_signed: String,
    pub cmx_new: String,
    #[serde(rename = "van_cmx")]
    pub gov_comm: String,
    pub gov_nullifiers: Vec<String>,
    pub proof: String,
    pub vote_round_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoteCommitmentWire {
    pub van_nullifier: String,
    pub vote_authority_note_new: String,
    pub vote_commitment: String,
    pub proposal_id: u32,
    pub proof: String,
    pub vote_round_id: String,
    #[serde(rename = "vote_comm_tree_anchor_height")]
    pub anchor_height: u32,
    pub r_vpk: String,
    pub vote_auth_sig: String,
}

/// Canonical request body for an atomic cast-vote batch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoteCommitmentBatchWire {
    /// Ordered actions. Their order is part of the batch digest and authority chain.
    pub votes: Vec<VoteCommitmentWire>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoteShareWire {
    /// Voting round ID as 32 bytes encoded in lowercase hex.
    pub vote_round_id: String,
    /// Poseidon output encoded as a canonical Pallas base-field element.
    pub shares_hash: String,
    pub proposal_id: u32,
    pub vote_decision: u32,
    #[serde(rename = "enc_share")]
    pub encrypted_share: WireEncryptedShare,
    pub share_index: u32,
    #[serde(rename = "tree_position")]
    pub vc_tree_position: u64,
    /// All 16 per-share commitments as canonical Pallas base-field elements.
    pub share_comms: Vec<String>,
    pub primary_blind: String,
    pub submit_at: u64,
}

/// Version 1 public handoff for a round-bound voting hotkey target.
///
/// Use [`VotingHotkeyTargetV1::from_json`], [`VotingHotkeyTargetV1::to_json`],
/// and [`VotingHotkeyTargetV1::validate_for`] rather than deserializing this
/// DTO without validation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VotingHotkeyTargetV1 {
    /// Wire-format version. Version 1 requires the JSON number `1`.
    pub format_version: u32,
    /// Configured vote chain identifier.
    pub vote_chain_id: String,
    /// Exact lowercase network name.
    pub network: String,
    /// Canonical 32-byte Pallas base-field encoding as lowercase hex.
    pub vote_round_id: String,
    /// Version 1 Orchard address index. This must be zero.
    pub address_index: u32,
    /// Canonical padded standard Base64 encoding of 43 raw Orchard address bytes.
    pub raw_orchard_address: String,
}

/// Parameters for a voting round, sourced from vote chain.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VotingRoundParams {
    pub vote_round_id: String,
    pub snapshot_height: u64,
    pub ea_pk: Vec<u8>,
    pub nc_root: Vec<u8>,
    pub nullifier_imt_root: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VotingNoteRefView {
    pub pool: String,
    pub txid_hex: String,
    pub output_index: u32,
    pub value_zatoshi: u64,
    pub voting_weight_zatoshi: u64,
    pub commitment_tree_position: u64,
    pub mined_height: u64,
    pub anchor_height: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VotingNoteSelectionResultView {
    pub note_count: u32,
    pub eligible_weight_zatoshi: u64,
    pub snapshot_height: u64,
    pub anchor_height: u64,
    pub notes: Vec<VotingNoteRefView>,
    /// Raw value of notes the privacy trim excludes from delegation, not their
    /// bundle-quantized voting weight. Surface this distinction to the voter.
    #[serde(default)]
    pub privacy_trim: PrivacyTrim,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegationPirPrecomputeResultView {
    pub cached_count: u32,
    pub fetched_count: u32,
    pub bundle_count: u32,
    pub bundle_index: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedDelegationPayloadView {
    pub pczt_bytes: Vec<u8>,
    pub status: String,
    pub message: Option<String>,
    pub submission: DelegationSubmissionWire,
    pub eligible_weight_zatoshi: u64,
    pub delegated_weight_zatoshi: u64,
    pub bundle_count: u32,
    pub bundle_index: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeystoneSignatureRecord {
    pub bundle_index: u32,
    pub sig: Vec<u8>,
    pub sighash: Vec<u8>,
    pub rk: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftVote {
    pub proposal_id: u32,
    pub choice: u32,
    pub num_options: u32,
    pub vc_tree_position: u64,
    pub single_share: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedVoteCommitmentView {
    pub proposal_id: u32,
    pub wire: VoteCommitmentWire,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedVoteCommitmentsView {
    pub bundle_index: u32,
    pub commitments: Vec<SignedVoteCommitmentView>,
}

/// FFI-safe representation of one atomic vote batch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedVoteBatchView {
    pub bundle_index: u32,
    pub commitments: Vec<SignedVoteCommitmentView>,
    /// Raw 32-byte batch digest signed by every action.
    pub batch_digest: Vec<u8>,
    /// Canonical JSON body to POST to the batch vote endpoint.
    pub batch_json: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoteRecord {
    pub proposal_id: u32,
    pub bundle_index: u32,
    pub choice: u32,
}

/// Stored lifecycle diagnostic of an authoritative chain-submission row.
///
/// `kind` is the stable discriminator from
/// `ChainSubmissionDiagnosticKind::as_str`; `message` is the bounded,
/// redacted text the lifecycle persisted. Present on terminal
/// `submitted_without_hash` and `rejected` rows, which schedule no further
/// lifecycle call, so this is what a host shows for manual handling.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmissionDiagnosticView {
    pub kind: String,
    pub message: String,
}

/// Discriminator of a [`NextStepView`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NextStepKind {
    Delegate,
    AdvanceDelegation,
    AdvanceImportedDelegation,
    CastVote,
    AdvanceVote,
    AdvanceVoteBatch,
    SubmitShares,
    ConfirmShare,
}

/// High-level work area a wallet should show or resume for a round.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoundPlanActionKind {
    Idle,
    Delegate,
    Vote,
    SubmitShares,
    Done,
}

/// Kind of grouped delegation recovery work.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationRecoveryWorkKindView {
    Delegate,
    AdvanceDelegation,
    AdvanceImportedDelegation,
}

/// Kind of grouped vote recovery work.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoteRecoveryWorkKindView {
    AdvanceVote,
    AdvanceVoteBatch,
    SubmitShares,
}

/// Cross-stage workflow phase of a delegation, vote, or share record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowPhaseView {
    Prepared,
    Signed,
    SubmittedDelegation,
    SubmittedVote,
    SubmittedShare,
    SubmissionManaged,
    SubmissionRejected,
    Confirmed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegationRecoveryView {
    pub bundle_index: u32,
    pub phase: WorkflowPhaseView,
    pub tx_hash: Option<String>,
    /// Confirmed VAN leaf position, if delegation has been projected.
    pub van_leaf_position: Option<u64>,
    #[serde(default)]
    pub submission_diagnostic: Option<SubmissionDiagnosticView>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoteRecoveryView {
    pub bundle_index: u32,
    pub proposal_id: u32,
    pub choice: u32,
    pub phase: WorkflowPhaseView,
    pub tx_hash: Option<String>,
    pub vc_tree_position: Option<u64>,
    pub has_commitment_bundle: bool,
    #[serde(default)]
    pub submission_diagnostic: Option<SubmissionDiagnosticView>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoverableCommitmentBundle {
    pub bundle_index: u32,
    pub proposal_id: u32,
    pub commitment_bundle_json: String,
    pub vc_tree_position: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareDelegationRecordView {
    pub round_id: String,
    pub bundle_index: u32,
    pub proposal_id: u32,
    pub share_index: u32,
    pub sent_to_urls: Vec<String>,
    #[serde(default)]
    pub ambiguous_urls: Vec<String>,
    #[serde(default)]
    pub target_count: u32,
    pub nullifier: Vec<u8>,
    pub phase: WorkflowPhaseView,
    pub confirmed: bool,
    pub submit_at: u64,
    pub created_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareWorkflowRecoveryView {
    pub bundle_index: u32,
    pub proposal_id: u32,
    pub share_index: u32,
    pub phase: WorkflowPhaseView,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NextStepView {
    pub kind: NextStepKind,
    pub bundle_index: u32,
    pub proposal_id: u32,
    pub choice: u32,
    pub share_index: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoundRecoveryStateView {
    pub round_id: String,
    pub bundle_count: u32,
    pub delegation: Vec<DelegationRecoveryView>,
    pub votes: Vec<VoteRecoveryView>,
    pub commitment_bundles: Vec<RecoverableCommitmentBundle>,
    pub shares: Vec<ShareWorkflowRecoveryView>,
    pub share_delegations: Vec<ShareDelegationRecordView>,
    pub unconfirmed_share_delegations: Vec<ShareDelegationRecordView>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegationStatusView {
    pub bundle_index: u32,
    pub phase: WorkflowPhaseView,
    pub tx_hash: Option<String>,
    #[serde(default)]
    pub submission_diagnostic: Option<SubmissionDiagnosticView>,
    /// True when this bundle's delegation ended without a confirmation and no
    /// further delegation step will be planned for it; `submission_diagnostic`
    /// says why. A confirmed bundle is not terminal in this sense: it
    /// succeeded, and `phase` says so.
    ///
    /// Read this rather than inferring from `phase`: a dispatch that reached
    /// the chain without a usable transaction hash reports the same phase as a
    /// healthy submission, and retrying it would resubmit.
    #[serde(default)]
    pub terminal: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegationRecoveryWorkView {
    pub kind: DelegationRecoveryWorkKindView,
    pub bundle_index: u32,
    pub phase: WorkflowPhaseView,
    pub tx_hash: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoteRecoveryWorkView {
    pub kind: VoteRecoveryWorkKindView,
    pub bundle_index: u32,
    pub proposal_id: u32,
    pub tx_hash: Option<String>,
    pub vc_tree_position: Option<u64>,
    pub share_indexes: Vec<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletedVoteChoiceView {
    pub proposal_id: u32,
    pub choice: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletedVoteDisplayView {
    pub choices: Vec<CompletedVoteChoiceView>,
    pub voted_at: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoundPlanView {
    pub round_id: String,
    pub pending_recovery: bool,
    pub blocking_recovery: bool,
    pub blocking_share_work: bool,
    /// True when any helper-share row is still unconfirmed. Schedule
    /// background share tracking from this instead of holding share rows.
    pub has_unconfirmed_shares: bool,
    pub hotkey_bound: bool,
    pub completed_vote_artifact: bool,
    pub completed_for_display: bool,
    pub completed_vote_display: Option<CompletedVoteDisplayView>,
    pub needs_draft_setup: bool,
    /// True when the round holds a ballot choice but no bundle rows yet, so
    /// the host must persist the bundle plan before any vote work is planned.
    pub needs_bundle_setup: bool,
    /// True when delegation work needs fresh or restored wallet signing material.
    ///
    /// Read these derived flags instead of matching `NextStepView::kind`
    /// strings: the SDK computes them from an exhaustive match, so a new step
    /// kind cannot silently read as "no work" in a host allowlist.
    pub needs_delegation_signing: bool,
    /// True when a delegation is in flight. Consult `needs_delegation_signing`
    /// to learn whether the next pass also needs signing material.
    pub has_in_flight_delegation: bool,
    /// Bundles this plan owes any delegation step for, ascending.
    ///
    /// The per-bundle form of the two flags above. A host that shows
    /// delegation progress per bundle reads this instead of filtering
    /// `delegation_statuses` against rules the planner already applied.
    #[serde(default)]
    pub delegation_bundles_needing_work: Vec<u32>,
    /// Bundles whose delegation still needs the voter's signing material,
    /// ascending.
    ///
    /// The per-bundle form of `needs_delegation_signing`, and a subset of
    /// `delegation_bundles_needing_work`. A delegation in flight is not signed
    /// and done — advancing one re-signs its locked generation — so it belongs
    /// here too. The exclusion is an imported delegation, which is already
    /// broadcast and never asks the voter for a signer.
    #[serde(default)]
    pub delegation_bundles_needing_signing: Vec<u32>,
    /// True when vote or helper-share submission work remains to drive.
    pub needs_vote_polling: bool,
    /// True when any vote or share work remains, counting share confirmation
    /// only when it is blocking.
    pub has_remaining_vote_or_share_work: bool,
    /// True when any vote or share work remains, counting share confirmation
    /// unconditionally.
    pub has_recoverable_vote_or_share_work: bool,
    pub primary_action: RoundPlanActionKind,
    pub next_steps: Vec<NextStepView>,
    pub delegation_statuses: Vec<DelegationStatusView>,
    pub recovered_delegation_work: Vec<DelegationRecoveryWorkView>,
    pub recovered_vote_work: Vec<VoteRecoveryWorkView>,
    pub open_proposals: Vec<u32>,
    /// Durable intents for proposals outside the authenticated roster; casting
    /// is withheld until the host clears them.
    #[serde(default)]
    pub unrostered_intents: Vec<u32>,
    /// The round's single immediate helper-share submission, if designated.
    pub immediate_share_key: Option<ImmediateShareKey>,
    pub immediate_share_confirmed: bool,
    pub all_decided: bool,
}

/// Stable error category exposed across the wallet boundary.
///
/// Mirrors [`crate::VotingErrorKind`]; `Other` covers categories added to the
/// crate after this view was generated for a host.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VotingErrorKindView {
    InvalidInput,
    KeystoneSignatureConflict,
    ProofFailed,
    Busy,
    Storage,
    Internal,
    InsufficientEligibility,
    NoSpendableNotes,
    SetupAlreadyPersisted,
    DelegationPcztUnavailable,
    DbBusy,
    PirUnavailable,
    DelegationTargetMismatch,
    DelegationAlreadyBroadcast,
    /// Any category this host does not know. Serde deserializes unknown
    /// category strings into it, so a newer crate can add kinds without
    /// breaking an older host's view.
    #[serde(other)]
    Other,
}

/// Wallet-facing view of a [`crate::VotingError`].
///
/// `kind`, `retryable`, and `message` are always populated. The remaining
/// fields carry the structured payload of the kinds that have one:
/// `bundle_index` for `KeystoneSignatureConflict`, `SetupAlreadyPersisted`,
/// and `DelegationPcztUnavailable`;
/// `snapshot_height`, the weight fields, the selected note count, and the
/// bundle slot capacity for
/// `InsufficientEligibility` and `NoSpendableNotes`; `http_status` and
/// `endpoint` for `PirUnavailable`.
///
/// Unknown fields are accepted on purpose: a newer crate may add a structured
/// field for a category an older host reads as `Other`, and the whole payload
/// must still parse for that fallback to mean anything.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VotingErrorView {
    pub kind: VotingErrorKindView,
    pub retryable: bool,
    pub message: String,
    pub bundle_index: Option<u32>,
    /// For `SetupAlreadyPersisted`, the setup column that already held a
    /// value. Sighash and effects conflicts are reusable after validation;
    /// a padded-note-secrets conflict is not.
    pub setup_field: Option<DelegationSetupFieldView>,
    pub snapshot_height: Option<u64>,
    pub required_weight_zatoshi: Option<u64>,
    pub selected_weight_zatoshi: Option<u64>,
    pub bundle_note_slots: Option<u32>,
    pub selected_notes: Option<u32>,
    pub http_status: Option<u16>,
    pub endpoint: Option<String>,
}

/// Wire form of [`crate::types::DelegationSetupField`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationSetupFieldView {
    DelegationPczt,
    PaddedNoteSecrets,
    PcztSighash,
    Tx1Effects,
}

/// One pending helper-share round for one wallet, as returned by
/// [`crate::share::pending_rounds_for_accounts`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingShareRoundView {
    pub wallet_id: String,
    pub round_id: String,
    pub session_json: Option<String>,
}

/// Discriminator of a [`ChainSubmissionOutcomeView`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChainSubmissionOutcomeKind {
    Confirmed,
    Tracking,
    Recovering,
    SubmittedWithoutHash,
    Rejected,
    Cancelled,
}

/// How a confirmation was established.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChainConfirmationSourceView {
    Hash,
    Tree,
}

/// Category of a chain submission diagnostic.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChainDiagnosticKindView {
    AmbiguousDispatch,
    AmbiguousAttemptsExhausted,
    NullifierAlreadySpent,
    TrackingWindowExpired,
    ChainRejected,
    ReconciliationPending,
    InvalidProtocolResponse,
    StorageFailure,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainDiagnosticView {
    pub kind: ChainDiagnosticKindView,
    pub message: String,
}

/// Flat view of one chain submission result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainSubmissionOutcomeView {
    pub kind: ChainSubmissionOutcomeKind,
    pub confirmation_source: Option<ChainConfirmationSourceView>,
    pub transaction_hash: Option<String>,
    pub candidate_transaction_hash: Option<String>,
    pub final_van_position: Option<u64>,
    pub vote_commitment_positions: Vec<u64>,
    pub diagnostic: Option<ChainDiagnosticView>,
}

/// Durable chain submission state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChainSubmissionStateView {
    Submitting,
    Tracking,
    Recovering,
    SubmittedWithoutHash,
    Confirmed,
    Rejected,
}

/// How strongly a failure's state is known.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChainSubmissionStateEvidenceView {
    Durable,
    KnownPossiblyDispatched,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainSubmissionFailureStateView {
    pub state: ChainSubmissionStateView,
    pub evidence: ChainSubmissionStateEvidenceView,
}

/// Durable identity of one committed vote.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct VoteKeyView {
    pub bundle_index: u32,
    pub proposal_id: u32,
}

/// Durable identity of one helper share.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ShareKeyView {
    pub bundle_index: u32,
    pub proposal_id: u32,
    pub share_index: u32,
}

/// How one PIR endpoint answered a snapshot-height probe.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PirSnapshotEndpointStatusView {
    /// Serving exactly the round's snapshot height.
    Matched,
    /// Serving an older height; it has not caught up yet.
    Behind,
    /// Serving a newer height; the round's snapshot is gone from it.
    Ahead,
    /// Answered without a height field.
    MissingHeight,
    /// Answered with a body that is not the expected JSON.
    MalformedJson,
    /// Answered with a non-success HTTP status.
    NonSuccessStatus,
    /// Did not answer within the probe deadline.
    TimeoutOrNetworkError,
}

/// One endpoint's probe result.
///
/// The full set is part of a resolution, not debug output: a caller builds its
/// failover list from the endpoints that matched, and explains a failed
/// resolution from the heights the others reported.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PirSnapshotEndpointDiagnosticView {
    pub endpoint: String,
    pub status: PirSnapshotEndpointStatusView,
    pub reported_height: Option<u64>,
    pub http_status_code: Option<u16>,
    pub message: Option<String>,
}

/// One share that reached a new helper during recovery.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResubmittedShareView {
    pub share: ShareKeyView,
    pub server_url: String,
}

/// What one helper-share tracking pass did.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareTrackingPassReportView {
    /// Unconfirmed shares the round held when the pass began. Zero means the
    /// round owed nothing at that moment, which the effect lists alone cannot
    /// establish. Absent when the pass failed before it could look, which is
    /// not the same as zero.
    #[serde(default)]
    pub unconfirmed_at_entry: Option<u32>,
    #[serde(default)]
    pub confirmed: Vec<ShareKeyView>,
    #[serde(default)]
    pub resubmitted: Vec<ResubmittedShareView>,
    #[serde(default)]
    pub ambiguous: Vec<ResubmittedShareView>,
    /// Shares skipped because their recovery material is missing. Retrying
    /// cannot repair these, but a helper that already accepted one may still
    /// confirm it, so a host keeps polling.
    #[serde(default)]
    pub unrecoverable: Vec<ShareKeyView>,
    pub cancelled: bool,
    /// Seconds until the next pass, absent when nothing is pending.
    pub next_delay_seconds: Option<u64>,
}

/// Discriminator of a [`ShareTrackingQuiescenceView`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShareTrackingQuiescenceKind {
    NothingToTrack,
    AllConfirmed,
    VoteEndReached,
    Cancelled,
    AlreadyDriving,
    Failing,
    PassBudgetExhausted,
}

/// Why a tracking run stopped, flattened for host bindings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareTrackingQuiescenceView {
    pub kind: ShareTrackingQuiescenceKind,
    /// `Failing`: the consecutive failures that ended the run.
    #[serde(default)]
    pub messages: Vec<String>,
    /// `PassBudgetExhausted`: shares the last pass could
    /// not repair.
    #[serde(default)]
    pub unrecoverable: Vec<ShareKeyView>,
}

/// Discriminator of a [`ShareTrackingEventView`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShareTrackingEventKind {
    PassStarted,
    PassFinished,
    PassFailed,
    AwaitingNextPass,
}

/// One observation from a tracking run, flattened for host bindings.
///
/// Not `Eq`, because a delay is fractional seconds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ShareTrackingEventView {
    pub kind: ShareTrackingEventKind,
    /// The pass this belongs to, counting from 1. Absent on
    /// `AwaitingNextPass`, which sits between two passes.
    pub pass: Option<u32>,
    /// `PassFinished`: what that pass did. `PassFailed`: what it had already
    /// committed before it stopped, whose `unrecoverable` and
    /// `next_delay_seconds` are meaningless because the walk did not finish.
    pub report: Option<ShareTrackingPassReportView>,
    /// `PassFailed`: why.
    pub message: Option<String>,
    /// `AwaitingNextPass`: how long the driver waits before the next one.
    /// Fractional, because a policy may set a subsecond retry.
    pub delay_seconds: Option<f64>,
}

/// Everything one tracking run did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareTrackingRunReportView {
    pub quiescence: ShareTrackingQuiescenceView,
    pub passes: u32,
    #[serde(default)]
    pub confirmed: Vec<ShareKeyView>,
    #[serde(default)]
    pub resubmitted: Vec<ResubmittedShareView>,
    #[serde(default)]
    pub ambiguous: Vec<ResubmittedShareView>,
    /// From the most recent pass, not accumulated: a share can stop being
    /// unrecoverable once its material is restored.
    #[serde(default)]
    pub unrecoverable: Vec<ShareKeyView>,
    #[serde(default)]
    pub failures: Vec<String>,
}

/// Delivery result for one share of a batch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareDeliveryOutcomeView {
    pub share_index: u32,
    pub accepted_urls: Vec<String>,
    pub ambiguous_urls: Vec<String>,
    pub target_count: u32,
}

/// Result of one initial helper delivery for a confirmed vote.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareBatchDeliveryReportView {
    pub vote: VoteKeyView,
    pub deliveries: Vec<ShareDeliveryOutcomeView>,
    pub pending_share_indices: Vec<u32>,
    pub cancelled: bool,
    /// True when the persisted plan predates complete-plan persistence.
    pub legacy_best_effort: bool,
}

/// What one round step call accomplished.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoundStepDispositionView {
    NoWork,
    Advanced,
    Pending,
    Cancelled,
    ChainTerminal,
}

/// Stable category of a round step failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoundStepFailureKindView {
    InvalidInput,
    InsufficientEligibility,
    NoSpendableNotes,
    Busy,
    Storage,
    InvariantViolation,
    Transport,
    Protocol,
    ProofFailed,
    Signing,
    HelperDeliveryIncomplete,
    VoteEnded,
    DelegationTargetMismatch,
}

/// Outcome of one round step.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoundStepOutcomeView {
    pub step: Option<NextStepView>,
    pub disposition: RoundStepDispositionView,
    pub chain_outcome: Option<ChainSubmissionOutcomeView>,
    pub share_deliveries: Vec<ShareBatchDeliveryReportView>,
    pub delegation: Option<SignedDelegationPayloadView>,
    pub plan: RoundPlanView,
}

/// Failure of one round step with the refreshed plan when it could be read.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoundStepFailureView {
    pub kind: RoundStepFailureKindView,
    pub step: Option<NextStepView>,
    pub strongest_chain_state: Option<ChainSubmissionFailureStateView>,
    pub chain_outcome: Option<ChainSubmissionOutcomeView>,
    pub message: String,
    pub plan: Option<RoundPlanView>,
    /// Helper delivery reports accumulated before the failure; absent in
    /// payloads from SDKs that predate the field.
    #[serde(default)]
    pub share_deliveries: Vec<ShareBatchDeliveryReportView>,
    /// The delegation signed before the failure, for the same reason as
    /// `share_deliveries`: the bundle is durable and the step produced it.
    pub delegation: Option<SignedDelegationPayloadView>,
}

/// Delegation proving and signing stages.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationProgressKind {
    SelectingNotes,
    PcztBuilding,
    PcztBuilt,
    ProofStarting,
    WaitingForExistingProof,
    ProofProgress,
    ProofComplete,
    SigningPayload,
    PayloadReady,
}

/// Vote proving and signing stages.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoteCommitStageKind {
    ProofStarting,
    ProofProgress,
    SharePayloadsBuilding,
    Signing,
}

/// Discriminator of a [`RoundStepProgressView`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoundStepProgressKind {
    Selected,
    Delegation,
    TreeSynced,
    VoteCommit,
    HelperPlansPrepared,
    ChainOutcome,
    ShareOutcome,
    ShareConfirmed,
}

/// One progress event from a round step, flattened for the host boundary.
///
/// `kind` says which optional payload fields are populated: `step` for
/// `Selected`; `bundle_index`, `delegation_progress`, and `proof_progress`
/// for `Delegation`; `tree_height` for `TreeSynced`; `bundle_index`,
/// `proposal_id`, `vote_commit_stage`, and `proof_progress` for
/// `VoteCommit`; `vote_keys` for `HelperPlansPrepared`; `chain_outcome` for
/// `ChainOutcome`; `share_delivery` for `ShareOutcome`; `share` and
/// `share_confirmed` for `ShareConfirmed`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RoundStepProgressView {
    pub kind: RoundStepProgressKind,
    pub step: Option<NextStepView>,
    pub bundle_index: Option<u32>,
    pub proposal_id: Option<u32>,
    pub delegation_progress: Option<DelegationProgressKind>,
    pub vote_commit_stage: Option<VoteCommitStageKind>,
    pub proof_progress: Option<f64>,
    pub tree_height: Option<u32>,
    pub vote_keys: Vec<VoteKeyView>,
    pub chain_outcome: Option<ChainSubmissionOutcomeView>,
    pub share_delivery: Option<ShareBatchDeliveryReportView>,
    pub share: Option<ShareKeyView>,
    pub share_confirmed: Option<bool>,
}

/// Discriminator of a [`RoundQuiescenceView`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoundQuiescenceKind {
    NoWorkLeft,
    NeedsBundleSetup,
    PersistedChainTerminal,
    NeedsBallot,
    NeedsDelegationSignatures,
    BackgroundShareWorkOnly,
    Cancelled,
    ChainTerminal,
    ChainRecoveryStalled,
    Failures,
    PassBudgetExhausted,
}

/// Why one round run stopped, flattened for host bindings.
///
/// Each field is populated only for the kinds that carry it; a host switches
/// on `kind` and reads what that variant names.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoundQuiescenceView {
    pub kind: RoundQuiescenceKind,
    /// `NeedsBallot`: proposals with no terminal decision yet.
    #[serde(default)]
    pub open_proposals: Vec<u32>,
    /// `NeedsBallot`: durable intents outside the roster the host must clear.
    #[serde(default)]
    pub unrostered_intents: Vec<u32>,
    /// `NeedsDelegationSignatures`: every bundle still awaiting a signature.
    #[serde(default)]
    pub bundles: Vec<u32>,
    /// `BackgroundShareWorkOnly`: shares requiring the host's tracking timer.
    #[serde(default)]
    pub shares: Vec<ShareKeyView>,
    /// `ChainTerminal` and `ChainRecoveryStalled`: the step and its outcome.
    pub step: Option<NextStepView>,
    pub chain_outcome: Option<ChainSubmissionOutcomeView>,
    /// `PassBudgetExhausted`: the work the run left behind.
    #[serde(default)]
    pub remaining: Vec<NextStepView>,
}

/// Ballot progress of one run, measured against what it started owing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoundWorkTallyView {
    pub completed_proposals: u32,
    pub total_proposals: u32,
    pub remaining_obligations: u32,
}

/// One failure a run isolated, with the bundle it skipped.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoundStepFailureRecordView {
    pub step: Option<NextStepView>,
    /// The bundle this failure is attributed to, when it belongs to one.
    ///
    /// Attribution, not isolation: `StopRound` ends the run without
    /// suppressing anything, so `skipped_bundles` is the authoritative list of
    /// what was skipped.
    pub bundle_index: Option<u32>,
    pub failure: RoundStepFailureView,
}

/// Everything one round run did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoundRunReportView {
    pub quiescence: RoundQuiescenceView,
    /// The last plan the run read, absent only when it stopped before one.
    pub plan: Option<RoundPlanView>,
    pub tally: RoundWorkTallyView,
    /// Failures in dispatch order. A non-empty list does not imply a
    /// `Failures` quiescence: a run can isolate one bundle and finish the rest.
    #[serde(default)]
    pub failures: Vec<RoundStepFailureRecordView>,
    /// Bundles a failure isolated for the rest of the run. Empty under
    /// `StopRound`, which stops instead of suppressing.
    #[serde(default)]
    pub skipped_bundles: Vec<u32>,
    /// Every chain outcome the run observed, terminal or not.
    #[serde(default)]
    pub chain_outcomes: Vec<RoundChainOutcomeView>,
    #[serde(default)]
    pub share_deliveries: Vec<ShareBatchDeliveryReportView>,
    /// Delegation bundles the run signed, in the order it produced them.
    ///
    /// Signed, not necessarily submitted: a step cancelled between signing and
    /// building its chain request returns the bundle it produced, and a step
    /// that failed at dispatch carries one too. The payload's own `status` is
    /// always `ready_for_submission` and is not a submission state, so a host
    /// that reads it as one will resubmit a bundle already on the chain.
    ///
    /// What happened to bundle `n` is in `plan`: its `delegation_statuses`
    /// entry carries the durable phase, the transaction hash, and whether the
    /// submission is terminal. These are projected so a host reading only the
    /// report sees the artifacts a host watching every step would.
    #[serde(default)]
    pub delegations: Vec<SignedDelegationPayloadView>,
}

/// One chain outcome the run observed, bound to the step that produced it.
///
/// Not only terminal ones: a submission still tracking appears here too.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoundChainOutcomeView {
    pub step: NextStepView,
    pub outcome: ChainSubmissionOutcomeView,
}

/// Discriminator of a [`RoundDriveEventView`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoundDriveEventKind {
    PlanRefreshed,
    StepSelected,
    StepProgress,
    StepFinished,
    StepFailed,
    AwaitingRepoll,
    BundleSkipped,
}

/// One observation from a run, flattened for host bindings.
///
/// Every variant that describes work names its step: a run overlaps bundles,
/// and a bare progress record carries no subject of its own, so a host reading
/// one stream would otherwise misattribute it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RoundDriveEventView {
    pub kind: RoundDriveEventKind,
    /// The step every work-describing variant belongs to.
    pub step: Option<NextStepView>,
    /// `PlanRefreshed`: the plan the driver selects from, and its tally.
    pub plan: Option<RoundPlanView>,
    pub tally: Option<RoundWorkTallyView>,
    /// `StepProgress`: progress from inside the running step.
    pub progress: Option<RoundStepProgressView>,
    /// `StepFinished`: the executor's own answer for the step.
    pub disposition: Option<RoundStepDispositionView>,
    /// `StepFailed`: why it failed.
    pub failure_kind: Option<RoundStepFailureKindView>,
    pub message: Option<String>,
    /// `AwaitingRepoll`: how long the driver waits before trying the step
    /// again. Paces chain tracking and unfinished helper work alike, so read
    /// `step` rather than labelling the wait as chain work.
    pub delay_seconds: Option<f64>,
    /// `BundleSkipped`: the bundle isolated for the rest of the run.
    pub bundle_index: Option<u32>,
}
