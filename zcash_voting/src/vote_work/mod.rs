//! SDK-owned execution of vote work that is already durable.
//!
//! The host supplies authenticated round configuration, transports, timing,
//! scheduling, and cancellation. This module owns interpretation of the
//! durable round plan and the ordering between helper-plan persistence, chain
//! advancement, confirmation, and helper-share delivery.

mod cast_vote;
mod delegation_steps;
mod round_lock;
mod share_confirmation;
mod step_control;
mod step_ledger;
pub(crate) mod step_outcomes;
mod step_scope;
mod steps;
mod vote_completion;

use std::sync::Arc;

use zeroize::Zeroizing;

use crate::delegate::{DelegationProgress, SignedDelegationBundle};
use crate::delegation_pipeline::{DelegationDriver, DelegationSigner};
use crate::pir::PirFleet;
use crate::round::VotingDb;
use crate::session::{Decision, NextStep, RoundPlan};
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

    pub(super) fn num_options(&self, proposal_id: u32) -> Option<u32> {
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
    /// Vote-tree node URLs used by `CastVote`, tried in order. Every failed
    /// sync drops the round's cached tree, including the last node's, before
    /// the next node or the next pass tries again.
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
    /// The wallet's eligible weight is below the round minimum.
    InsufficientEligibility,
    /// The wallet holds no spendable notes at the round snapshot.
    NoSpendableNotes,
    Busy,
    Storage,
    InvariantViolation,
    Transport,
    Protocol,
    ProofFailed,
    Signing,
    HelperDeliveryIncomplete,
    /// The authenticated vote-end time has passed, so a new vote cannot be
    /// cast. Steps that advance or recover work already on the wire still
    /// run; only `CastVote` is refused.
    VoteEnded,
    /// A bundle's stored delegation setup does not reproduce from the voting
    /// hotkey the host supplied.
    ///
    /// Its own kind because retrying the step is never the answer: either the
    /// host holds the wrong key for this round, or the bundle is rebuilt
    /// against the key it does hold. Which of those applies is not settled
    /// here — the comparison is over the stored binding alone, and a
    /// delegation already on chain fails it identically. A rebuild is safe
    /// only for a bundle that was never broadcast, and the discard enforces
    /// that itself.
    DelegationTargetMismatch,
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
    /// Helper delivery reports the step accumulated before it failed. Each
    /// records network effects that did happen (accepted, ambiguous, and
    /// pending shares), so a `HelperDeliveryIncomplete` failure or a later
    /// error does not lose what earlier shares reached the helpers.
    pub share_deliveries: Vec<VoteShareDeliveryReport>,
}

impl RoundStepFailure {
    /// Attaches the delivery reports accumulated before this failure.
    #[cfg(test)]
    pub(crate) fn with_share_deliveries(
        mut self,
        share_deliveries: Vec<VoteShareDeliveryReport>,
    ) -> Self {
        self.share_deliveries = share_deliveries;
        self
    }
}

/// Durable identity of one committed vote.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct VoteRecoveryKey {
    pub bundle_index: u32,
    pub proposal_id: u32,
}

/// Helper delivery report bound to its durable vote identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoteShareDeliveryReport {
    pub vote: VoteRecoveryKey,
    pub delivery: ShareBatchDeliveryReport,
}

/// Executes round steps for one wallet and round.
///
/// The executor owns the ordering between helper-plan persistence, chain
/// advancement, confirmation, and helper-share delivery, and runs proving
/// off the async runtime. Delegation steps lock per bundle so bundles prove
/// concurrently; chain and share steps lock per round.
pub struct RoundExecutor<T> {
    /// Wallet the executor was constructed for. Every operation runs against
    /// this scope; see [`Self::wallet_scope`].
    wallet_id: String,
    /// Network of the chain client; a binding for another network is refused.
    chain_network: Network,
    database: Arc<VotingDb>,
    chain_client: ChainSubmissionClient<T>,
    helper_client: HelperClient,
    tree_transport: Option<Arc<dyn vote_commitment_tree_client::transport::Transport>>,
    binding: Option<RoundBinding>,
}

impl RoundExecutor<HyperTransport> {
    /// Constructs an executor using the SDK's default chain HTTP transport.
    pub fn new(
        database: Arc<VotingDb>,
        chain_config: ChainSubmissionClientConfig,
        helper_client: HelperClient,
    ) -> Result<Self, ChainSubmissionFailure> {
        let (wallet_id, database) = freeze_wallet_scope(&database)?;
        let chain_network = chain_config.network;
        let chain_client = ChainSubmissionClient::new(Arc::clone(&database), chain_config)?;
        Ok(Self {
            wallet_id,
            chain_network,
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
    /// Both planning and chain advancement are permanently bound to the
    /// wallet `database` is scoped to at construction. The executor works on
    /// its own handle over the same connection, so a later `set_wallet_id`
    /// on the host's handle cannot retarget in-flight or later work; callers
    /// cannot compose clients backed by different wallets.
    pub fn with_transport(
        database: Arc<VotingDb>,
        chain_transport: T,
        chain_config: ChainSubmissionClientConfig,
        helper_client: HelperClient,
    ) -> Result<Self, ChainSubmissionFailure> {
        let (wallet_id, database) = freeze_wallet_scope(&database)?;
        let chain_network = chain_config.network;
        let chain_client = ChainSubmissionClient::with_transport(
            Arc::clone(&database),
            chain_transport,
            chain_config,
        )?;
        Ok(Self {
            wallet_id,
            chain_network,
            database,
            chain_client,
            helper_client,
            tree_transport: None,
            binding: None,
        })
    }

    /// Binds the round, roster, and hotkey the step API operates on.
    ///
    /// The roster must be the complete, nonempty, distinct proposal set from
    /// the authenticated round configuration. An empty roster is rejected
    /// here because the planner would otherwise treat it as vacuously
    /// decided and never advertise `CastVote`, silently skipping the round.
    ///
    /// # Errors
    ///
    /// Returns [`VotingError::InvalidInput`] for a non-canonical round id, a
    /// network other than the chain client's or than the network the wallet
    /// already stores this round under, an empty roster, a repeated or
    /// out-of-range proposal id, an option count outside the supported
    /// range, or a hotkey secret that does not reconstruct. The network is checked here because chain identity
    /// derivation would otherwise reject it only after proving and helper
    /// plans had already been persisted.
    pub fn with_binding(mut self, binding: RoundBinding) -> Result<Self, VotingError> {
        crate::types::validate_vote_round_id_hex(&binding.round_id)?;
        if binding.network != self.chain_network {
            return Err(VotingError::InvalidInput {
                message: format!(
                    "round binding network {:?} does not match the chain client network {:?}",
                    binding.network, self.chain_network
                ),
            });
        }
        // A round this wallet already holds under another network would only
        // be rejected by prepare_vote_work after CastVote had synced and
        // cached a tree from the binding's node fleet; refuse it up front.
        self.ensure_stored_round_network(&binding.round_id, "the binding")?;
        // A malformed hotkey secret would otherwise be discovered only after
        // CastVote had synced the tree and generated a witness.
        if let Some(secret) = binding.hotkey_secret.as_ref() {
            crate::VotingHotkey::from_stored_secret(secret, binding.network)?;
        }
        if binding.proposals.is_empty() {
            return Err(VotingError::InvalidInput {
                message: "round binding requires a nonempty proposal roster".to_string(),
            });
        }
        for entry in &binding.proposals {
            crate::types::validate_proposal_id(entry.proposal_id)?;
            crate::types::validate_vote_options(entry.num_options)?;
        }
        let mut seen = std::collections::HashSet::new();
        if let Some(repeated) = binding
            .proposals
            .iter()
            .find(|entry| !seen.insert(entry.proposal_id))
        {
            return Err(VotingError::InvalidInput {
                message: format!(
                    "round binding roster lists proposal {} more than once",
                    repeated.proposal_id
                ),
            });
        }
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

    /// A handle on the executor's sidecar connection, scoped to its wallet.
    ///
    /// Each call returns a fresh handle over the same connection. The
    /// executor's own handle is never handed out, so re-scoping the returned
    /// one with `set_wallet_id` cannot move a running step's persistence to
    /// another wallet.
    pub fn database(&self) -> Arc<VotingDb> {
        // The wallet id was accepted by `scoped` at construction.
        Arc::new(crate::round::VotingDb::from_shared(
            self.database.shared_connection(),
            &self.wallet_id,
        ))
    }

    /// The wallet every operation is scoped to.
    ///
    /// The executor captured this id at construction and keys its locks,
    /// plans, and persistence by it. Its internal handle is private, so this
    /// check cannot fail through the public API; it guards the invariant
    /// against internal misuse and fails with [`VotingError::InvalidInput`]
    /// rather than letting work for one wallet run under another's lock.
    pub(super) fn wallet_scope(&self) -> Result<&str, VotingError> {
        let current = self.database.wallet_id();
        if current != self.wallet_id {
            return Err(VotingError::InvalidInput {
                message: format!(
                    "round executor is scoped to wallet {} but its database handle now selects wallet {current}",
                    self.wallet_id
                ),
            });
        }
        Ok(&self.wallet_id)
    }

    /// Rejects work on a round this wallet already stores under a network
    /// other than the chain client's, before any network I/O or durable
    /// write. `caller` names what supplied the round, for the message.
    pub(super) fn ensure_stored_round_network(
        &self,
        round_id: &str,
        caller: &str,
    ) -> Result<(), VotingError> {
        let conn = self.database.conn();
        if !crate::storage::queries::has_round(&conn, round_id, &self.wallet_id)? {
            return Ok(());
        }
        let stored = crate::storage::queries::load_round_network(&conn, round_id, &self.wallet_id)?;
        if stored != self.chain_network {
            return Err(VotingError::InvalidInput {
                message: format!(
                    "round {round_id} is stored for network {stored:?} but {caller} and the chain client use {:?}",
                    self.chain_network
                ),
            });
        }
        Ok(())
    }

    fn binding(&self) -> Result<&RoundBinding, VotingError> {
        self.binding
            .as_ref()
            .ok_or_else(|| VotingError::InvalidInput {
                message: "round executor is not bound to a round; call with_binding".to_string(),
            })
    }
}

/// Captures the wallet `database` currently selects and returns a handle
/// over the same connection that only the executor holds.
fn freeze_wallet_scope(
    database: &VotingDb,
) -> Result<(String, Arc<VotingDb>), ChainSubmissionFailure> {
    let wallet_id = database.wallet_id();
    let scoped = Arc::new(database.scoped(&wallet_id).map_err(|error| {
        ChainSubmissionFailure::without_state(
            crate::ChainSubmissionFailureKind::InvalidInput,
            error.to_string(),
        )
    })?);
    Ok((wallet_id, scoped))
}

#[cfg(test)]
mod tests;
