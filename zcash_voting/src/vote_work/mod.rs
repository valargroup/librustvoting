//! SDK-owned execution of vote work that is already durable.
//!
//! The host supplies authenticated round configuration, transports, timing,
//! scheduling, and cancellation. This module owns interpretation of the
//! durable round plan and the ordering between helper-plan persistence, chain
//! advancement, confirmation, and helper-share delivery.

mod execution;
mod round_lock;

use std::sync::Arc;

use crate::round::VotingDb;
use crate::session::{RoundPlan, VoteRecoveryWork};
use crate::share_tracking::ShareBatchDeliveryReport;
use crate::{
    ChainSubmissionClient, ChainSubmissionClientConfig, ChainSubmissionFailure,
    ChainSubmissionFailureState, ChainSubmissionResult, ChainTransport, HelperClient,
    HyperTransport,
};

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

/// Misuse-resistant owner of persisted vote recovery for one voting database.
pub struct VoteRecoveryExecutor<T> {
    database: Arc<VotingDb>,
    chain_client: ChainSubmissionClient<T>,
    helper_client: HelperClient,
}

impl VoteRecoveryExecutor<HyperTransport> {
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
        })
    }
}

impl<T: ChainTransport> VoteRecoveryExecutor<T> {
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
        })
    }
}

#[cfg(test)]
mod tests;
