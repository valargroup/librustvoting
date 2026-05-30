use anyhow::{Context, Result};
use zcash_voting::prelude::{
    resume_plan, round_snapshot, CommittedVote, NextStep, RoundPlan, RoundRecoverySnapshot, VotingDb,
};

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

/// Reconstructs the committed vote payload for a resume step.
///
/// Use this for `NextStep::SubmitVote` and `NextStep::SubmitShares` to avoid
/// rebuilding proofs during recovery.
pub fn recover_committed_vote_for_step(
    voting_db: &VotingDb,
    round_id: &str,
    step: &NextStep,
) -> Result<Option<CommittedVote>> {
    match *step {
        NextStep::SubmitVote {
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
