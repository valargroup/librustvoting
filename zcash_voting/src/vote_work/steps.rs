//! Step execution for [`RoundExecutor`]: the public step API and dispatch.
//!
//! Every step runs under a lock, re-plans from durable state, and reports
//! progress at durable and network boundaries. Proving never runs on the
//! async runtime: delegation and vote proofs run on dedicated large-stack
//! threads and stream their progress back through channels.
//!
//! Mechanism lives in sibling children, one per responsibility:
//! `delegation_steps` (prove, sign, and advance delegations), `cast_vote`
//! (tree sync, VAN witness, vote proving, persistence), `vote_completion`
//! (helper plans, chain advancement, share delivery and confirmation), and
//! `step_outcomes` (outcome construction and failure projection).

use std::sync::Arc;

use crate::{
    session::{resume_plan, NextStep, RoundPlan, VoteRecoveryWorkKind},
    share_tracking::ShareKey,
    AdvanceImportedDelegation, ChainAdvancePolicy, ChainAdvanceRequest, ChainRecoveryMode,
    ChainSubmissionControl, ChainTransport, VotingError,
};

use super::{
    round_lock, step_control::StepControl, step_scope::parse_round_id, BallotIntent, RoundExecutor,
    RoundHostContext, RoundStepFailure, RoundStepFailureKind, RoundStepOutcome, RoundStepProgress,
    RoundStepProgressReporter,
};

// Matches the keygen warm-up threads in voting-circuits.
pub(super) const PROVING_STACK_BYTES: usize = 64 * 1024 * 1024;

impl<T: ChainTransport> RoundExecutor<T> {
    /// Plans the bound round from durable state.
    pub fn plan(&self) -> Result<RoundPlan, VotingError> {
        self.wallet_scope()?;
        let binding = self.binding()?;
        // The round may have been created after binding through a handle
        // from `database()`; a network mismatch must fail before any step
        // syncs and caches a tree for it.
        self.ensure_stored_round_network(&binding.round_id, "the binding")?;
        resume_plan(&self.database, &binding.round_id, &binding.proposal_ids())
    }

    /// Records ballot decisions and returns the refreshed plan.
    ///
    /// Option counts come from the bound roster, so a decision for an unknown
    /// proposal is rejected before anything is written. The whole batch is
    /// resolved against the roster first and then written in one transaction,
    /// so a rejected batch leaves durable intent unchanged. The stored round's
    /// network is checked inside that transaction against the binding's, so
    /// a round created for another network after the binding was made is
    /// refused before any intent is written.
    pub fn set_ballot_intents(&self, intents: &[BallotIntent]) -> Result<RoundPlan, VotingError> {
        self.wallet_scope()?;
        let binding = self.binding()?;
        let resolved = intents
            .iter()
            .map(|intent| {
                let num_options = binding.num_options(intent.proposal_id).ok_or_else(|| {
                    VotingError::InvalidInput {
                        message: format!(
                            "proposal {} is not in the round roster",
                            intent.proposal_id
                        ),
                    }
                })?;
                Ok((intent.proposal_id, intent.decision, num_options))
            })
            .collect::<Result<Vec<_>, VotingError>>()?;
        self.database
            .set_ballot_intents(&binding.round_id, self.chain_network, &resolved)?;
        self.plan()
    }

    /// Runs the first planned step, if any.
    ///
    /// The operation epoch is captured before planning, so an epoch change
    /// while the initial plan waits on the database interrupts the step that
    /// follows instead of being adopted by it.
    pub async fn advance_next(
        &self,
        host: &RoundHostContext,
        control: &ChainSubmissionControl,
        progress: &dyn RoundStepProgressReporter,
    ) -> Result<RoundStepOutcome, RoundStepFailure> {
        let step_control = StepControl::capture(control);
        let plan = self
            .plan()
            .map_err(|error| self.step_voting_failure(error, None))?;
        let Some(step) = plan.next_steps.first().cloned() else {
            return Ok(self.no_work(None, plan));
        };
        self.advance_step_under(step, host, &step_control, progress)
            .await
    }

    /// Runs one planned step by one bounded pass.
    ///
    /// The step is re-validated against a fresh plan under the lock; a step
    /// another pass already completed returns `NoWork`. A step whose bundle
    /// still has a delegation step ahead of it in the plan fails with
    /// `InvalidInput` naming that prerequisite, before any lock-scoped work
    /// or network I/O; run the prerequisite first or use `advance_next`.
    /// `Delegate` and `AdvanceDelegation` lock their bundle; every other step
    /// locks the round. A `ConfirmShare` whose share no helper has accepted
    /// yet (the plan's `blocking_share_work`) runs the share's delivery from
    /// its durable plan instead of polling for a confirmation no helper can
    /// give. The operation epoch is captured on entry: cancellation
    /// or an epoch change is observed at every boundary where the step decides
    /// to continue, and either ends the step as `Cancelled`.
    pub async fn advance_step(
        &self,
        step: NextStep,
        host: &RoundHostContext,
        control: &ChainSubmissionControl,
        progress: &dyn RoundStepProgressReporter,
    ) -> Result<RoundStepOutcome, RoundStepFailure> {
        self.advance_step_under(step, host, &StepControl::capture(control), progress)
            .await
    }

    /// Runs one step under a control captured by the public entry point.
    async fn advance_step_under(
        &self,
        step: NextStep,
        host: &RoundHostContext,
        control: &StepControl<'_>,
        progress: &dyn RoundStepProgressReporter,
    ) -> Result<RoundStepOutcome, RoundStepFailure> {
        let wallet_id = self
            .wallet_scope()
            .map(str::to_string)
            .map_err(|error| self.step_voting_failure(error, Some(step.clone())))?;
        let round_id = self
            .binding()
            .map(|binding| binding.round_id.clone())
            .map_err(|error| self.step_voting_failure(error, Some(step.clone())))?;
        let scope = match &step {
            NextStep::Delegate { bundle_index } | NextStep::AdvanceDelegation { bundle_index } => {
                Some(*bundle_index)
            }
            _ => None,
        };
        let Some(guard) = round_lock::acquire(
            self.database.sidecar_id(),
            wallet_id,
            &round_id,
            scope,
            control.chain(),
            control.entry_epoch(),
        )
        .await
        .map_err(|message| {
            self.step_failure(
                RoundStepFailureKind::InvariantViolation,
                Some(step.clone()),
                None,
                None,
                message,
            )
        })?
        else {
            return self.step_cancelled(Some(step), None, Vec::new(), None);
        };
        // Proving threads share this lock so it survives a dropped future for
        // as long as a detached prover keeps working on the round.
        let lock: round_lock::HeldRoundLock = Arc::new(guard);

        let plan = self
            .plan()
            .map_err(|error| self.step_voting_failure(error, Some(step.clone())))?;
        if !plan.next_steps.contains(&step) {
            return Ok(self.no_work(Some(step), plan));
        }
        if let Some(prerequisite) = crate::session::blocking_prerequisite(&plan.next_steps, &step) {
            return Err(self.step_failure(
                RoundStepFailureKind::InvalidInput,
                Some(step.clone()),
                None,
                None,
                format!(
                    "{step:?} requires {prerequisite:?} to complete first; run that step or advance_next"
                ),
            ));
        }
        if control.interrupted() {
            return self.step_cancelled(Some(step), None, Vec::new(), None);
        }
        progress.report(RoundStepProgress::Selected(step.clone()));

        match step.clone() {
            NextStep::Delegate { bundle_index } => {
                self.run_delegate(step, bundle_index, host, &lock, control, progress)
                    .await
            }
            NextStep::AdvanceDelegation { bundle_index } => {
                self.run_advance_delegation(step, bundle_index, host, &lock, control, progress)
                    .await
            }
            NextStep::AdvanceImportedDelegation { bundle_index } => {
                let request = AdvanceImportedDelegation {
                    vote_round_id: self.round_id_bytes(&step)?,
                    bundle_index,
                };
                let outcome = self
                    .chain_client
                    .advance_until_terminal_in_epoch(
                        ChainAdvanceRequest::ImportedDelegation(request),
                        &persisted_policy(host),
                        control.chain(),
                        control.entry_epoch(),
                    )
                    .await
                    .map_err(|failure| self.step_chain_failure(failure, Some(step.clone())))?;
                self.chain_step_outcome(step, outcome, None, progress)
            }
            NextStep::CastVote { bundle_index, .. } => {
                self.run_cast_vote(step, bundle_index, &plan, host, &lock, control, progress)
                    .await
            }
            NextStep::AdvanceVote {
                bundle_index,
                proposal_id,
            }
            | NextStep::AdvanceVoteBatch {
                bundle_index,
                proposal_id,
            }
            | NextStep::SubmitShares {
                bundle_index,
                proposal_id,
                ..
            } => {
                let kind = match &step {
                    NextStep::AdvanceVote { .. } => VoteRecoveryWorkKind::AdvanceVote,
                    NextStep::AdvanceVoteBatch { .. } => VoteRecoveryWorkKind::AdvanceVoteBatch,
                    _ => VoteRecoveryWorkKind::SubmitShares,
                };
                self.run_persisted_vote_work(
                    step,
                    kind,
                    bundle_index,
                    proposal_id,
                    host,
                    control,
                    progress,
                )
                .await
            }
            NextStep::ConfirmShare {
                bundle_index,
                proposal_id,
                share_index,
            } => {
                // A share row no helper has accepted yet (every POST failed
                // definitely, or a reservation was cleared before dispatch)
                // cannot be confirmed by polling: no helper holds it. The
                // planner classifies it as `SubmitShares` recovery work, so
                // deliver it from its durable plan before polling.
                let awaits_delivery = plan.recovered_vote_work.iter().any(|work| {
                    work.kind == VoteRecoveryWorkKind::SubmitShares
                        && work.bundle_index == bundle_index
                        && work.proposal_id == proposal_id
                        && work.share_indexes.contains(&share_index)
                });
                if awaits_delivery {
                    return self
                        .run_persisted_vote_work(
                            step,
                            VoteRecoveryWorkKind::SubmitShares,
                            bundle_index,
                            proposal_id,
                            host,
                            control,
                            progress,
                        )
                        .await;
                }
                self.run_confirm_share(
                    step,
                    ShareKey {
                        bundle_index,
                        proposal_id,
                        share_index,
                    },
                    host,
                    control,
                    progress,
                )
                .await
            }
        }
    }

    pub(super) fn round_id_bytes(&self, step: &NextStep) -> Result<[u8; 32], RoundStepFailure> {
        let round_id = self
            .binding()
            .map(|binding| binding.round_id.clone())
            .map_err(|error| self.step_voting_failure(error, Some(step.clone())))?;
        parse_round_id(&round_id)
            .map_err(|error| self.step_voting_failure(error, Some(step.clone())))
    }
}

/// Persisted work always reconciles through the exact tree from its first
/// pass, as the resume planner requires; the host's cadence still applies.
pub(super) fn persisted_policy(host: &RoundHostContext) -> ChainAdvancePolicy {
    ChainAdvancePolicy {
        initial_recovery_mode: ChainRecoveryMode::ExactTree,
        ..host.chain_policy.clone()
    }
}
