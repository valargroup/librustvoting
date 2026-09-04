use std::sync::Arc;

use anyhow::{Context, Result};
use zcash_voting::prelude::{
    recover_atomic_vote_batch, resume_plan, round_snapshot, ChainSubmissionClientConfig,
    ChainSubmissionControl, CommittedVote, HelperClient, HelperHealth, Network, NextStep,
    NoopRoundStepProgressReporter, ProposalRosterEntry, RoundBinding, RoundExecutor,
    RoundHostContext, RoundPlan, RoundRecoverySnapshot, RoundStepDisposition, RoundStepOutcome,
    SignedVoteBatch, VotingDb,
};
use zcash_voting::{HelperTransport, HyperTransport, RouteHttp};

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
/// `route` carries every voting-related request: helper POSTs, vote-chain
/// calls, and vote-tree sync all run through it, so a wallet that requires
/// Tor or another privacy route passes its executor once and nothing falls
/// back to a direct connection. Pass `Arc::new(DirectRoute::default())` when
/// no route is required.
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
    binding: RoundBinding,
    host: impl Fn() -> RoundHostContext,
    control: &ChainSubmissionControl,
) -> Result<RoundStepOutcome> {
    // One transport, and so one blocking runtime, serves helpers, the chain,
    // and the vote tree; each `HyperTransport` owns worker threads.
    let transport = Arc::new(HyperTransport::with_shared_route(route));
    let helper_transport: Arc<dyn HelperTransport> = transport.clone();
    let helper_client = HelperClient::new(helper_transport, HelperHealth::default());
    let executor = RoundExecutor::with_transport(
        voting_db,
        Arc::clone(&transport),
        ChainSubmissionClientConfig::for_network(network, chain_endpoints),
        helper_client,
    )
    .map_err(|failure| anyhow::anyhow!(failure.message().to_string()))?
    .with_binding(binding)
    .context("bind round executor")?
    .with_tree_transport(transport);
    loop {
        let outcome = executor
            .advance_next(&host(), control, &NoopRoundStepProgressReporter {})
            .await
            .map_err(|failure| anyhow::anyhow!(failure.message))?;
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
