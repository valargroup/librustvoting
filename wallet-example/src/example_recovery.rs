use std::sync::Arc;

use anyhow::{Context, Result};
use zcash_voting::prelude::{
    recover_atomic_vote_batch, resume_plan, round_snapshot, ChainSubmissionClientConfig,
    ChainSubmissionControl, CommittedVote, HelperClient, HelperHealth, Network, NextStep,
    NoopRoundStepProgressReporter, ProposalRosterEntry, RoundBinding, RoundExecutor,
    RoundHostContext, RoundPlan, RoundRecoverySnapshot, RoundStepDisposition, RoundStepFailure,
    RoundStepOutcome, SignedVoteBatch, VotingDb,
};
use zcash_voting::wire::PirLayout;
use zcash_voting::{
    ChainSubmissionFailure, HelperTransport, HyperTransport, PirFleet, RouteHttp, VotingError,
};

/// Why [`advance_round_until_idle`] stopped before the plan went idle.
///
/// The step variant keeps the executor's complete [`RoundStepFailure`]: the
/// chain outcome, strongest durable state, helper delivery reports that did
/// reach the helpers, and the refreshed plan. A caller that only wants text
/// can use `Display`; one that must act on what already happened matches on
/// the variant.
#[derive(Debug)]
pub enum RoundAdvanceError {
    /// The executor could not be built over the chain configuration.
    Executor(ChainSubmissionFailure),
    /// The round binding was refused.
    Binding(VotingError),
    /// A step failed; its durable effects are on the failure.
    Step(RoundStepFailure),
}

impl std::fmt::Display for RoundAdvanceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Executor(failure) => write!(f, "build round executor: {}", failure.message()),
            Self::Binding(error) => write!(f, "bind round executor: {error}"),
            Self::Step(failure) => f.write_str(&failure.message),
        }
    }
}

impl std::error::Error for RoundAdvanceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Executor(failure) => Some(failure),
            Self::Binding(error) => Some(error),
            Self::Step(_) => None,
        }
    }
}

/// A PIR fleet whose requests travel `route`, for the `host` closure passed
/// to [`advance_round_until_idle`].
///
/// Delegation PIR runs over the fleet in `RoundHostContext::delegation`,
/// which the host builds, not over the executor's transports; a wallet that
/// requires a private route builds its fleet here with the same route it
/// passes to the loop, so no PIR request falls back to a direct connection.
pub fn routed_pir_fleet<R: RouteHttp>(
    route: Arc<R>,
    endpoints: &[String],
    layout: PirLayout,
) -> Result<PirFleet, VotingError> {
    PirFleet::new(
        endpoints,
        layout,
        Arc::new(HyperTransport::with_shared_route(route)),
    )
}

/// Drives one round to its next idle point with the SDK-owned executor.
///
/// This is the recommended recovery loop: bind the round once, then call
/// `advance_next` until the plan has nothing actionable, re-scheduling on
/// `Pending`. The executor owns step interpretation, helper-plan persistence,
/// chain advancement, confirmation, and share delivery; the host supplies
/// transports, the fleet, timing, and cancellation.
///
/// The last step's full outcome is returned rather than only its plan. After a
/// terminal chain result (`ChainTerminal`) the plan deliberately schedules no
/// retry and carries no vote diagnostic, so `RoundStepOutcome::chain_outcome`
/// is the only place the rejection or hashless-submission diagnostic survives.
///
/// `route` carries every request the executor makes itself: helper POSTs,
/// vote-chain calls, and vote-tree sync all run through it, so a wallet that
/// requires Tor or another privacy route passes its executor once and none of
/// those fall back to a direct connection. Pass `Arc::new(DirectRoute::default())`
/// when no route is required. Delegation PIR is the exception: it runs over
/// the `PirFleet` inside the `RoundHostContext` that `host` returns, which
/// this helper never sees. Build that fleet with [`routed_pir_fleet`] over
/// the same `route`; a fleet built over a direct transport sends PIR
/// requests directly regardless of `route`.
///
/// `helper_health` is the wallet's helper score table. It is caller-owned so
/// that failures and cooldowns observed in one call still steer helper
/// selection in the next: a wallet that schedules this helper repeatedly
/// keeps one `HelperHealth` per wallet and passes a clone each time.
///
/// `host` is called before every step so each pass sees the current time and
/// fleet: a long proof can cross the last-moment or vote-end boundary, and the
/// following `CastVote` must plan against the clock it actually runs under.
/// A `NoWork` outcome whose refreshed plan still lists steps (another
/// executor finished the selected step first) continues rather than returns,
/// so the helper really runs until the plan is idle.
pub async fn advance_round_until_idle<R: RouteHttp>(
    voting_db: Arc<VotingDb>,
    network: Network,
    chain_endpoints: Vec<String>,
    route: Arc<R>,
    helper_health: HelperHealth,
    binding: RoundBinding,
    host: impl Fn() -> RoundHostContext,
    control: &ChainSubmissionControl,
) -> Result<RoundStepOutcome, RoundAdvanceError> {
    // One transport, and so one blocking runtime, serves helpers, the chain,
    // and the vote tree; each `HyperTransport` owns worker threads.
    let transport = Arc::new(HyperTransport::with_shared_route(route));
    let helper_transport: Arc<dyn HelperTransport> = transport.clone();
    let helper_client = HelperClient::new(helper_transport, helper_health);
    let executor = RoundExecutor::with_transport(
        voting_db,
        Arc::clone(&transport),
        ChainSubmissionClientConfig::for_network(network, chain_endpoints),
        helper_client,
    )
    .map_err(RoundAdvanceError::Executor)?
    .with_binding(binding)
    .map_err(RoundAdvanceError::Binding)?
    .with_tree_transport(transport);
    loop {
        let outcome = executor
            .advance_next(&host(), control, &NoopRoundStepProgressReporter {})
            .await
            .map_err(RoundAdvanceError::Step)?;
        match outcome.disposition {
            RoundStepDisposition::Advanced => continue,
            RoundStepDisposition::NoWork if !outcome.plan.next_steps.is_empty() => continue,
            _ => return Ok(outcome),
        }
    }
}

/// Builds the executor binding from the authenticated proposal roster.
pub fn round_binding(
    round_id: &str,
    network: Network,
    proposals: &[(u32, u32)],
    hotkey_secret: Option<Vec<u8>>,
) -> RoundBinding {
    RoundBinding {
        round_id: round_id.to_string(),
        network,
        proposals: proposals
            .iter()
            .map(|(proposal_id, num_options)| ProposalRosterEntry {
                proposal_id: *proposal_id,
                num_options: *num_options,
            })
            .collect(),
        hotkey_secret: hotkey_secret.map(zeroize::Zeroizing::new),
    }
}

/// One round-level recovery payload fetched in a single caller entrypoint.
pub struct RoundRecoveryContext {
    pub snapshot: RoundRecoverySnapshot,
    pub plan: RoundPlan,
}

/// Recovery material needed to execute one planner step without rebuilding proofs.
pub struct RecoveredVoteStep {
    pub step: NextStep,
    pub committed_vote: CommittedVote,
}

/// One recovered atomic batch keyed by its planner step.
pub struct RecoveredVoteBatchStep {
    pub step: NextStep,
    pub signed_batch: SignedVoteBatch,
}

/// Loads the typed recovery snapshot for one round.
pub fn load_round_recovery_snapshot(
    voting_db: &VotingDb,
    round_id: &str,
) -> Result<RoundRecoverySnapshot> {
    round_snapshot(voting_db, round_id).context("load round recovery snapshot")
}

/// Returns both the recovery snapshot and resumable step plan for one round.
pub fn snapshot_with_resume_plan(
    voting_db: &VotingDb,
    round_id: &str,
    proposal_ids: &[u32],
) -> Result<(RoundRecoverySnapshot, RoundPlan)> {
    let snapshot = load_round_recovery_snapshot(voting_db, round_id)?;
    let plan = resume_plan(voting_db, round_id, proposal_ids).context("build resume plan")?;
    Ok((snapshot, plan))
}

/// Loads round recovery data and planner steps in one wallet-facing call.
///
/// Wallet code drives retries by iterating `context.plan.next_steps`. Recover
/// singleton vote handles and helper-share
/// identities with `recover_committed_vote_for_step`, and atomic batches with
/// `recover_vote_batch_for_step`. Helper delivery must reuse the wallet's
/// SDK-persisted complete plan through
/// `example_vote::submit_committed_vote_shares`.
pub fn load_round_recovery_context(
    voting_db: &VotingDb,
    round_id: &str,
    proposal_ids: &[u32],
) -> Result<RoundRecoveryContext> {
    let (snapshot, plan) = snapshot_with_resume_plan(voting_db, round_id, proposal_ids)?;
    Ok(RoundRecoveryContext { snapshot, plan })
}

/// Reconstructs the committed vote handle for a resume step.
///
/// Use this for singleton `NextStep::AdvanceVote` and per-vote
/// `NextStep::SubmitShares` work to avoid rebuilding proofs during recovery.
pub fn recover_committed_vote_for_step(
    voting_db: &VotingDb,
    round_id: &str,
    step: &NextStep,
) -> Result<Option<CommittedVote>> {
    match *step {
        NextStep::AdvanceVote {
            bundle_index,
            proposal_id,
        }
        | NextStep::SubmitShares {
            bundle_index,
            proposal_id,
            ..
        } => CommittedVote::recover(voting_db, round_id, bundle_index, proposal_id)
            .map(Some)
            .context("recover committed vote for resume step"),
        _ => Ok(None),
    }
}

/// Reconstructs one complete atomic batch for a batch resume step.
///
/// This is inspection and display material. Execute the chain transaction with
/// `ChainSubmissionClient::advance_vote_batch_with_recovery` and
/// `ChainRecoveryMode::ExactTree`. The client derives the same batch from
/// persisted recovery state; retain `batch_digest` to identify that generation.
pub fn recover_vote_batch_for_step(
    voting_db: &VotingDb,
    round_id: &str,
    step: &NextStep,
) -> Result<Option<SignedVoteBatch>> {
    match *step {
        NextStep::AdvanceVoteBatch {
            bundle_index,
            proposal_id,
        } => recover_atomic_vote_batch(voting_db, round_id, bundle_index, proposal_id)
            .map(Some)
            .context("recover atomic vote batch for resume step"),
        _ => Ok(None),
    }
}

/// Reconstructs committed-vote handles for all planner steps that require them.
///
/// The returned list is ordered exactly as `plan.next_steps`, so callers can
/// execute vote-chain work in planner order and route `SubmitShares` through
/// the persisted full-batch plan and durable helper-submission API.
pub fn recover_committed_votes_for_plan(
    voting_db: &VotingDb,
    round_id: &str,
    plan: &RoundPlan,
) -> Result<Vec<RecoveredVoteStep>> {
    let mut recovered = Vec::new();
    for step in &plan.next_steps {
        if let Some(committed_vote) = recover_committed_vote_for_step(voting_db, round_id, step)? {
            recovered.push(RecoveredVoteStep {
                step: step.clone(),
                committed_vote,
            });
        }
    }
    Ok(recovered)
}

/// Reconstructs complete atomic batches for all batch planner steps.
pub fn recover_vote_batches_for_plan(
    voting_db: &VotingDb,
    round_id: &str,
    plan: &RoundPlan,
) -> Result<Vec<RecoveredVoteBatchStep>> {
    let mut recovered = Vec::new();
    for step in &plan.next_steps {
        if let Some(signed_batch) = recover_vote_batch_for_step(voting_db, round_id, step)? {
            recovered.push(RecoveredVoteBatchStep {
                step: step.clone(),
                signed_batch,
            });
        }
    }
    Ok(recovered)
}
