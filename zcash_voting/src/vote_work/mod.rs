//! SDK-owned execution of vote work that is already durable.
//!
//! The host supplies authenticated round configuration, transports, timing,
//! scheduling, and cancellation. This module owns interpretation of the
//! durable round plan and the ordering between helper-plan persistence, chain
//! advancement, confirmation, and helper-share delivery.

mod execution;
mod round_lock;
mod steps;

use std::sync::Arc;

use zeroize::Zeroizing;

use crate::delegate::{DelegationProgress, SignedDelegationBundle};
use crate::delegation_pipeline::{DelegationDriver, DelegationSigner};
use crate::pir::PirFleet;
use crate::round::VotingDb;
use crate::session::{Decision, NextStep, RoundPlan, VoteRecoveryWork};
use crate::share_tracking::{ShareBatchDeliveryReport, ShareKey};
use crate::vote::VoteCommitStage;
use crate::{
    ChainAdvancePolicy, ChainSubmissionClient, ChainSubmissionClientConfig, ChainSubmissionFailure,
    ChainSubmissionFailureState, ChainSubmissionResult, ChainTransport, HelperClient,
    HyperTransport, Network, VotingError,
};

/// One proposal from the authenticated round configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProposalRosterEntry {
    pub proposal_id: u32,
    pub num_options: u32,
}

/// Immutable per-executor scope: the round, its proposal roster, and the
/// voting hotkey that signs votes.
pub struct RoundBinding {
    /// Canonical 32-byte voting round identifier encoded as lowercase hex.
    pub round_id: String,
    /// Network the round and hotkey belong to.
    pub network: Network,
    /// Complete proposal roster from the authenticated round configuration.
    pub proposals: Vec<ProposalRosterEntry>,
    /// Stored secret of the round's voting hotkey, when votes may be cast.
    ///
    /// The hotkey is reconstructed on the proving thread; the executor holds
    /// only these bytes, zeroized on drop.
    pub hotkey_secret: Option<Zeroizing<Vec<u8>>>,
}

impl RoundBinding {
    pub fn proposal_ids(&self) -> Vec<u32> {
        self.proposals
            .iter()
            .map(|entry| entry.proposal_id)
            .collect()
    }

    fn num_options(&self, proposal_id: u32) -> Option<u32> {
        self.proposals
            .iter()
            .find(|entry| entry.proposal_id == proposal_id)
            .map(|entry| entry.num_options)
    }
}

/// One ballot decision to record before casting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BallotIntent {
    pub proposal_id: u32,
    pub decision: Decision,
}

/// Delegation inputs for `Delegate` and `AdvanceDelegation` steps.
#[derive(Clone)]
pub struct DelegationStepInputs {
    pub driver: Arc<dyn DelegationDriver>,
    pub signer: DelegationSigner,
    pub pir: Arc<PirFleet>,
}

/// Host inputs for one step: transports are bound at construction, this
/// carries what changes per call.
#[derive(Clone)]
pub struct RoundHostContext {
    /// Complete current helper fleet from authenticated configuration.
    pub configured_helper_urls: Vec<String>,
    /// Unix time captured for this pass.
    pub now_seconds: u64,
    /// Ceremony phase start, when the round timing is authenticated.
    pub ceremony_start_seconds: Option<u64>,
    /// Vote end time, when the round timing is authenticated.
    pub vote_end_time_seconds: Option<u64>,
    /// Vote-tree node URLs used by `CastVote`, tried in order. A sync that
    /// fails on one node resets the cached tree and retries on the next.
    pub vote_tree_node_urls: Vec<String>,
    /// Delegation inputs, required by `Delegate` and `AdvanceDelegation`.
    pub delegation: Option<DelegationStepInputs>,
    /// Chain policy for fresh submissions; persisted work always starts
    /// with exact-tree recovery.
    pub chain_policy: ChainAdvancePolicy,
    /// Proof concurrency for atomic vote batches.
    pub max_proof_concurrency: usize,
}

impl RoundHostContext {
    /// Last-moment buffer derived by the SDK timing policy, or `None` without
    /// authenticated timing.
    pub fn last_moment_buffer_seconds(&self) -> Option<u64> {
        match (self.ceremony_start_seconds, self.vote_end_time_seconds) {
            (Some(start), Some(end)) => {
                crate::share::policy::last_moment_buffer_seconds(start, end)
            }
            _ => None,
        }
    }

    /// Whether `now_seconds` falls inside the round's last-moment window.
    pub fn is_last_moment(&self) -> bool {
        match (self.ceremony_start_seconds, self.vote_end_time_seconds) {
            (Some(start), Some(end)) => {
                crate::share::policy::is_last_moment(self.now_seconds, start, end)
            }
            _ => false,
        }
    }

    /// Vote end used for helper planning; falls back to `now` without timing.
    pub fn planning_vote_end_seconds(&self) -> u64 {
        self.vote_end_time_seconds.unwrap_or(self.now_seconds)
    }
}

/// Progress emitted at durable and network boundaries of one step.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum RoundStepProgress {
    /// The executor is about to run this step from a fresh plan.
    Selected(NextStep),
    /// Delegation proving or signing progress for one bundle.
    Delegation {
        bundle_index: u32,
        progress: DelegationProgress,
    },
    /// The vote tree synced to this height before casting.
    TreeSynced { height: u32 },
    /// Vote proof and signing progress.
    VoteCommit(VoteCommitStage),
    /// Complete delivery plans are durable for all listed votes.
    HelperPlansPrepared(Vec<VoteRecoveryKey>),
    /// One bounded chain advancement episode produced this outcome.
    ChainOutcome(ChainSubmissionResult),
    /// Initial helper delivery completed for one confirmed vote.
    ShareOutcome(VoteShareDeliveryReport),
    /// A helper-share confirmation check completed for this share.
    ShareConfirmed { share: ShareKey, confirmed: bool },
}

/// Synchronous observer for [`RoundStepProgress`].
pub trait RoundStepProgressReporter: Send + Sync {
    fn report(&self, progress: RoundStepProgress);
}

/// Reporter for hosts that need only the terminal outcome.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopRoundStepProgressReporter {}

impl RoundStepProgressReporter for NoopRoundStepProgressReporter {
    fn report(&self, _progress: RoundStepProgress) {}
}

/// Adapts a closure to [`RoundStepProgressReporter`].
pub struct RoundStepProgressBridge<F> {
    report: F,
}

impl<F> RoundStepProgressBridge<F> {
    pub fn new(report: F) -> Self {
        Self { report }
    }
}

impl<F> RoundStepProgressReporter for RoundStepProgressBridge<F>
where
    F: Fn(RoundStepProgress) + Send + Sync,
{
    fn report(&self, progress: RoundStepProgress) {
        (self.report)(progress);
    }
}

/// What one step call accomplished.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RoundStepDisposition {
    /// The requested step is no longer in the plan; nothing ran.
    NoWork,
    /// The step cleared. More independent work may remain.
    Advanced,
    /// Chain reconciliation or share confirmation remains non-terminal;
    /// schedule the step again.
    Pending,
    /// Host cancellation stopped the step without undoing durable effects.
    Cancelled,
    /// The chain reported a terminal rejection or hashless submission.
    ChainTerminal,
}

/// Outcome of one step.
#[derive(Clone, Debug)]
pub struct RoundStepOutcome {
    pub step: Option<NextStep>,
    pub disposition: RoundStepDisposition,
    pub chain_outcome: Option<ChainSubmissionResult>,
    pub share_deliveries: Vec<VoteShareDeliveryReport>,
    /// The signed delegation a `Delegate` step produced.
    pub delegation: Option<SignedDelegationBundle>,
    pub plan: RoundPlan,
}

/// Stable category for a step failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RoundStepFailureKind {
    InvalidInput,
    Busy,
    Storage,
    InvariantViolation,
    Transport,
    Protocol,
    ProofFailed,
    Signing,
    HelperDeliveryIncomplete,
}

/// Failure that retains the strongest truthful durable state and a
/// refreshed plan.
#[derive(Clone, Debug)]
pub struct RoundStepFailure {
    pub kind: RoundStepFailureKind,
    pub step: Option<NextStep>,
    pub strongest_chain_state: Option<ChainSubmissionFailureState>,
    pub chain_outcome: Option<ChainSubmissionResult>,
    pub message: String,
    pub plan: Option<Box<RoundPlan>>,
}

/// Complete authenticated host inputs for one bounded persisted-vote pass.
#[derive(Clone, Copy, Debug)]
pub struct VoteRecoveryRequest<'a> {
    /// Canonical 32-byte voting round identifier encoded as lowercase hex.
    pub round_id: &'a str,
    /// Complete proposal roster from the authenticated round configuration.
    pub proposal_ids: &'a [u32],
    /// Complete current helper fleet from authenticated configuration.
    pub configured_helper_urls: &'a [String],
    /// Unix time captured for this pass.
    pub now_seconds: u64,
    /// Unix vote-end time used when creating a helper delivery plan.
    pub vote_end_time_seconds: u64,
    /// Optional last-moment window derived by the SDK timing policy.
    pub last_moment_buffer_seconds: Option<u64>,
}

/// Durable identity of one committed vote.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct VoteRecoveryKey {
    pub bundle_index: u32,
    pub proposal_id: u32,
}

/// Progress emitted at durable and network boundaries of one recovery pass.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum VoteRecoveryProgress {
    /// The executor selected this SDK-grouped work from a fresh round plan.
    Selected(VoteRecoveryWork),
    /// Complete delivery plans are durable for all listed votes.
    HelperPlansPrepared(Vec<VoteRecoveryKey>),
    /// One bounded chain advancement produced this authoritative outcome.
    ChainOutcome(ChainSubmissionResult),
    /// Initial helper delivery completed for one confirmed vote.
    ShareOutcome(VoteShareDeliveryReport),
}

/// Synchronous observer for progress from an asynchronous recovery pass.
pub trait VoteRecoveryProgressReporter: Send + Sync {
    fn report(&self, progress: VoteRecoveryProgress);
}

/// Progress reporter used when a host needs only the terminal result.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopVoteRecoveryProgressReporter {}

impl VoteRecoveryProgressReporter for NoopVoteRecoveryProgressReporter {
    fn report(&self, _progress: VoteRecoveryProgress) {}
}

/// Adapts a synchronous closure to [`VoteRecoveryProgressReporter`].
pub struct VoteRecoveryProgressBridge<F> {
    report: F,
}

impl<F> VoteRecoveryProgressBridge<F> {
    pub fn new(report: F) -> Self {
        Self { report }
    }
}

impl<F> VoteRecoveryProgressReporter for VoteRecoveryProgressBridge<F>
where
    F: Fn(VoteRecoveryProgress) + Send + Sync,
{
    fn report(&self, progress: VoteRecoveryProgress) {
        (self.report)(progress);
    }
}

/// Helper delivery report bound to its durable vote identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoteShareDeliveryReport {
    pub vote: VoteRecoveryKey,
    pub delivery: ShareBatchDeliveryReport,
}

/// What one bounded executor call accomplished.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum VoteRecoveryDisposition {
    /// No persisted chain-advancement or initial-share work was actionable.
    /// The returned plan may still contain fresh cast or confirmation work.
    NoWork,
    /// The selected work unit cleared. More independent work may remain.
    Advanced,
    /// Chain reconciliation remains non-terminal and should be scheduled again.
    Pending,
    /// Host cancellation stopped the pass without undoing durable effects.
    Cancelled,
}

/// Authoritative outcome of one bounded persisted-vote pass.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoteRecoveryAdvance {
    pub attempted_work: Option<VoteRecoveryWork>,
    pub disposition: VoteRecoveryDisposition,
    pub chain_outcome: Option<ChainSubmissionResult>,
    pub share_deliveries: Vec<VoteShareDeliveryReport>,
    pub round_plan: RoundPlan,
}

/// Stable category for a persisted-vote executor failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum VoteRecoveryFailureKind {
    InvalidInput,
    Busy,
    Storage,
    InvariantViolation,
    Transport,
    Protocol,
    ChainTerminal,
    HelperDeliveryIncomplete,
}

/// Failure that retains the strongest truthful durable state and refreshed plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoteRecoveryFailure {
    pub kind: VoteRecoveryFailureKind,
    pub attempted_work: Option<VoteRecoveryWork>,
    pub strongest_chain_state: Option<ChainSubmissionFailureState>,
    pub chain_outcome: Option<ChainSubmissionResult>,
    pub message: String,
    pub round_plan: Option<Box<RoundPlan>>,
}

/// Executes round steps for one wallet and round.
///
/// The executor owns the ordering between helper-plan persistence, chain
/// advancement, confirmation, and helper-share delivery, and runs proving
/// off the async runtime. Delegation steps lock per bundle so bundles prove
/// concurrently; chain and share steps lock per round.
pub struct RoundExecutor<T> {
    database: Arc<VotingDb>,
    chain_client: ChainSubmissionClient<T>,
    helper_client: HelperClient,
    tree_transport: Option<Arc<dyn vote_commitment_tree_client::transport::Transport>>,
    binding: Option<RoundBinding>,
}

/// Former name of [`RoundExecutor`].
#[deprecated(note = "use RoundExecutor")]
pub type VoteRecoveryExecutor<T> = RoundExecutor<T>;

impl RoundExecutor<HyperTransport> {
    /// Constructs an executor using the SDK's default chain HTTP transport.
    pub fn new(
        database: Arc<VotingDb>,
        chain_config: ChainSubmissionClientConfig,
        helper_client: HelperClient,
    ) -> Result<Self, ChainSubmissionFailure> {
        let chain_client = ChainSubmissionClient::new(Arc::clone(&database), chain_config)?;
        Ok(Self {
            database,
            chain_client,
            helper_client,
            tree_transport: None,
            binding: None,
        })
    }
}

impl<T: ChainTransport> RoundExecutor<T> {
    /// Constructs an executor with an injected chain transport.
    ///
    /// Both planning and chain advancement are permanently bound to
    /// `database`; callers cannot compose clients backed by different wallets.
    pub fn with_transport(
        database: Arc<VotingDb>,
        chain_transport: T,
        chain_config: ChainSubmissionClientConfig,
        helper_client: HelperClient,
    ) -> Result<Self, ChainSubmissionFailure> {
        let chain_client = ChainSubmissionClient::with_transport(
            Arc::clone(&database),
            chain_transport,
            chain_config,
        )?;
        Ok(Self {
            database,
            chain_client,
            helper_client,
            tree_transport: None,
            binding: None,
        })
    }

    /// Binds the round, roster, and hotkey the step API operates on.
    pub fn with_binding(mut self, binding: RoundBinding) -> Result<Self, VotingError> {
        crate::types::validate_vote_round_id_hex(&binding.round_id)?;
        self.binding = Some(binding);
        Ok(self)
    }

    /// Uses `transport` for vote-tree sync instead of the SDK direct client.
    pub fn with_tree_transport(
        mut self,
        transport: Arc<dyn vote_commitment_tree_client::transport::Transport>,
    ) -> Self {
        self.tree_transport = Some(transport);
        self
    }

    pub fn database(&self) -> &Arc<VotingDb> {
        &self.database
    }

    fn binding(&self) -> Result<&RoundBinding, VotingError> {
        self.binding
            .as_ref()
            .ok_or_else(|| VotingError::InvalidInput {
                message: "round executor is not bound to a round; call with_binding".to_string(),
            })
    }
}

#[cfg(test)]
mod tests;
