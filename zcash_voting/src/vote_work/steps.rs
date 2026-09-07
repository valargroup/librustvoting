//! Step execution for [`RoundExecutor`]: the public step API and dispatch.
//!
//! Every step captures its scope once, takes its lock, resolves the
//! requested step to the obligation a fresh plan lists for it, and runs
//! that obligation. Proving never runs on the async runtime: delegation and
//! vote proofs run on dedicated large-stack threads and stream their
//! progress back through channels.
//!
//! Mechanism lives in sibling children, one per responsibility:
//! `delegation_steps` (prove, sign, and advance delegations), `cast_vote`
//! (tree sync, VAN witness, vote proving, persistence), `vote_completion`
//! (helper plans, chain advancement, share delivery), `share_confirmation`
//! (polling a submitted share), `step_scope` and `step_ledger` (what a step
//! runs under and what it has accomplished), and `step_outcomes` (outcome
//! construction and failure projection).

use std::sync::Arc;

use crate::{
    round_planning::{
        blocking_prerequisite, plan_round_classified, resolve_step, ClassifiedPlan, Obligation,
    },
    session::{resume_plan, NextStep, RoundPlan},
    share_tracking::ShareKey,
    AdvanceImportedDelegation, ChainAdvancePolicy, ChainAdvanceRequest, ChainRecoveryMode,
    ChainSubmissionControl, ChainTransport, VotingError,
};

use super::{
    round_lock, step_control::StepControl, step_ledger::StepLedger, step_scope::StepScope,
    BallotIntent, RoundExecutor, RoundHostContext, RoundStepFailure, RoundStepFailureKind,
    RoundStepOutcome, RoundStepProgress, RoundStepProgressReporter,
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

    /// Plans the bound round, keeping the obligations beside the plan.
    ///
    /// The obligations name every member of an atomic batch, which the
    /// projected steps do not: a batch projects to one `AdvanceVoteBatch`
    /// carrying only its first member's id. The round driver reads them to
    /// report exact ballot progress.
    pub(crate) fn plan_classified(&self) -> Result<ClassifiedPlan, VotingError> {
        self.wallet_scope()?;
        let binding = self.binding()?;
        self.ensure_stored_round_network(&binding.round_id, "the binding")?;
        plan_round_classified(&self.database, &binding.round_id, &binding.proposal_ids())
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

    /// Runs the plan's first step, for tests that pin what one step does to a
    /// round without also exercising the driver's scheduling.
    ///
    /// Hosts drive a round with `RoundDriver`; this is not a shipped entry
    /// point, because a second way to advance a round is a second driver.
    #[cfg(test)]
    pub(crate) async fn advance_plan_head(
        &self,
        host: &RoundHostContext,
        control: &ChainSubmissionControl,
        progress: &dyn RoundStepProgressReporter,
    ) -> Result<RoundStepOutcome, RoundStepFailure> {
        let step_control = StepControl::capture(control);
        let plan = self
            .plan()
            .map_err(|error| self.step_voting_failure(error, None, &StepLedger::default()))?;
        let Some(step) = plan.next_steps.first().cloned() else {
            return Ok(self.no_work(None, plan));
        };
        self.advance_step_under(step, host, step_control, progress)
            .await
    }

    /// Runs one planned step by one bounded pass.
    ///
    /// The step is resolved against a fresh plan under the lock to the
    /// obligation it executes; a step another pass already completed returns
    /// `NoWork`. A step that plan still lists but that resolves to no
    /// obligation is an `InvariantViolation` rather than `NoWork`, so a caller
    /// re-selecting from a refreshed plan cannot loop on it forever. A step
    /// whose bundle still has a delegation step ahead of it in the plan fails
    /// with `InvalidInput` naming that prerequisite, before any lock-scoped
    /// work or network I/O; run the prerequisite first or use the driver.
    /// `round_lock::bundle_scope` decides the lock: `Delegate` and
    /// `AdvanceDelegation` lock their bundle, and every other step locks the
    /// round. A `ConfirmShare` whose share no
    /// helper has accepted yet (the plan's `blocking_share_work`) runs the
    /// share's delivery from its durable plan instead of polling for a
    /// confirmation no helper can give. The operation epoch is captured on
    /// entry: cancellation or an epoch change is observed at every boundary
    /// where the step decides to continue, and either ends the step as
    /// `Cancelled`.
    pub async fn advance_step(
        &self,
        step: NextStep,
        host: &RoundHostContext,
        control: &ChainSubmissionControl,
        progress: &dyn RoundStepProgressReporter,
    ) -> Result<RoundStepOutcome, RoundStepFailure> {
        self.advance_step_under(step, host, StepControl::capture(control), progress)
            .await
    }

    /// Runs one step as part of a longer run that captured `entry_epoch`.
    ///
    /// [`Self::advance_step`] captures the epoch when the step begins, which
    /// is the right answer for a host calling it directly. A driver decides to
    /// dispatch earlier than that — before planning, building the host
    /// context, and reading stored signing material — and an epoch switch
    /// across that gap must interrupt the step rather than be adopted by it.
    /// The step then stops at its first boundary, before any proving, durable
    /// write or broadcast.
    pub(crate) async fn advance_step_in_epoch(
        &self,
        step: NextStep,
        host: &RoundHostContext,
        control: &ChainSubmissionControl,
        entry_epoch: u64,
        progress: &dyn RoundStepProgressReporter,
    ) -> Result<RoundStepOutcome, RoundStepFailure> {
        self.advance_step_under(
            step,
            host,
            StepControl::in_epoch(control, entry_epoch),
            progress,
        )
        .await
    }

    /// Runs one step under a control captured by the public entry point.
    async fn advance_step_under(
        &self,
        step: NextStep,
        host: &RoundHostContext,
        control: StepControl<'_>,
        progress: &dyn RoundStepProgressReporter,
    ) -> Result<RoundStepOutcome, RoundStepFailure> {
        let scope = StepScope::capture(self, step, host, control)?;
        let ledger = StepLedger::default();
        // The driver schedules from this same function, so what it believes
        // can run concurrently is what actually takes separate locks.
        let lock_scope = round_lock::bundle_scope(&scope.step);
        let Some(guard) = round_lock::acquire(
            self.database.sidecar_id(),
            scope.wallet_id.clone(),
            &scope.round_id,
            lock_scope,
            scope.chain(),
            scope.entry_epoch(),
        )
        .await
        .map_err(|message| {
            self.step_failure(
                RoundStepFailureKind::InvariantViolation,
                Some(&scope.step),
                None,
                &ledger,
                message,
            )
        })?
        else {
            return self.step_cancelled(&scope, ledger);
        };
        // Proving threads share this lock so it survives a dropped future for
        // as long as a detached prover keeps working on the round.
        let lock: round_lock::HeldRoundLock = Arc::new(guard);

        // The one authoritative read under the lock: the step is resolved to
        // the obligation this plan lists for it, and that obligation is the
        // only thing the step executes.
        let classified = self.classified_plan(&scope)?;
        let Some(obligation) = resolve_step(&classified.obligations.obligations, &scope.step)
        else {
            // A step this plan no longer lists is benign: another pass, or a
            // background tracking pass, finished it. A step the plan still
            // lists but that resolves to no obligation is not: both facts come
            // from this one read, so they cannot disagree unless projection and
            // classification have. Answering `NoWork` there invites a caller
            // that re-selects from a refreshed plan to loop forever, which is
            // why the loop in `wallet-example` and every host driver would
            // otherwise need a guard of its own.
            //
            // `Obligation::Retire` and `Obligation::Blocked` cannot reach this
            // branch: they are never projected as steps, so no `NextStep` in
            // the plan resolves to them.
            if classified.plan.next_steps.contains(&scope.step) {
                return Err(self.step_failure(
                    RoundStepFailureKind::InvariantViolation,
                    Some(&scope.step),
                    None,
                    &ledger,
                    format!(
                        "{:?} resolved to no obligation in the plan that still lists it",
                        scope.step
                    ),
                ));
            }
            return Ok(self.no_work(Some(scope.step), classified.plan));
        };
        if let Some(prerequisite) = blocking_prerequisite(&classified.plan.next_steps, &scope.step)
        {
            return Err(self.step_failure(
                RoundStepFailureKind::InvalidInput,
                Some(&scope.step),
                None,
                &ledger,
                format!(
                    "{:?} requires {prerequisite:?} to complete first; run that step or drive the round",
                    scope.step
                ),
            ));
        }
        if scope.interrupted() {
            return self.step_cancelled(&scope, ledger);
        }
        progress.report(RoundStepProgress::Selected(scope.step.clone()));
        // The callback runs host code that may cancel or switch epochs; do
        // not start proving, prompt a signer, or touch durable state for a
        // step the host ended at this boundary.
        if scope.interrupted() {
            return self.step_cancelled(&scope, ledger);
        }

        match obligation.clone() {
            Obligation::Delegate { bundle_index } => {
                self.run_delegate(&scope, bundle_index, &lock, progress)
                    .await
            }
            Obligation::AdvanceDelegation {
                bundle_index,
                imported: false,
                ..
            } => {
                self.run_advance_delegation(&scope, bundle_index, &lock, progress)
                    .await
            }
            Obligation::AdvanceDelegation {
                bundle_index,
                imported: true,
                ..
            } => {
                let request = AdvanceImportedDelegation {
                    vote_round_id: scope.round_id_bytes,
                    bundle_index,
                };
                let outcome = self
                    .chain_client
                    .advance_until_terminal_in_epoch(
                        ChainAdvanceRequest::ImportedDelegation(request),
                        &persisted_policy(host),
                        scope.chain(),
                        scope.entry_epoch(),
                    )
                    .await
                    .map_err(|failure| {
                        self.step_chain_failure(failure, Some(&scope.step), &ledger)
                    })?;
                self.chain_step_outcome(&scope, outcome, ledger, progress)
            }
            Obligation::Cast {
                bundle_index,
                drafts,
                ..
            } => {
                self.run_cast_vote(&scope, bundle_index, &drafts, &lock, progress)
                    .await
            }
            Obligation::ReconcileChain {
                unit,
                ordered_proposal_ids,
                undispatched,
                ..
            } => {
                self.run_reconcile_chain(
                    &scope,
                    unit,
                    &ordered_proposal_ids,
                    undispatched,
                    progress,
                )
                .await
            }
            Obligation::Deliver {
                bundle_index,
                proposal_id,
                ..
            } => {
                self.run_deliver(&scope, bundle_index, proposal_id, progress)
                    .await
            }
            Obligation::Confirm {
                bundle_index,
                proposal_id,
                share_index,
                accepted,
                outcome_unknown,
                ..
            } => {
                // A share row no helper has reached (every POST failed
                // definitely, or a reservation was cleared before dispatch)
                // cannot be confirmed by polling: no helper holds it. Deliver
                // it from its durable plan. A share some helper may hold
                // (an ambiguous attempt, or one still in flight) is polled:
                // redelivery excludes those helpers, so only tracking can
                // classify them and make progress. Delivery also waits while
                // the vote's own chain work is pending.
                let chain_pending =
                    classified
                        .obligations
                        .obligations
                        .iter()
                        .any(|candidate| match candidate {
                            &Obligation::ReconcileChain {
                                bundle_index: pending_bundle,
                                ref ordered_proposal_ids,
                                ..
                            } => {
                                pending_bundle == bundle_index
                                    && ordered_proposal_ids.contains(&proposal_id)
                            }
                            _ => false,
                        });
                let never_reached = !accepted && !outcome_unknown;
                if never_reached && !chain_pending {
                    return self
                        .run_deliver(&scope, bundle_index, proposal_id, progress)
                        .await;
                }
                self.run_confirm_share(
                    &scope,
                    ShareKey {
                        bundle_index,
                        proposal_id,
                        share_index,
                    },
                    progress,
                )
                .await
            }
            // Never projected as a step, so never resolved from one.
            Obligation::Retire { .. } | Obligation::Blocked { .. } => {
                Ok(self.no_work(Some(scope.step), classified.plan))
            }
        }
    }

    /// The plan and its obligations for the scope's round, from one
    /// snapshot.
    fn classified_plan(&self, scope: &StepScope<'_>) -> Result<ClassifiedPlan, RoundStepFailure> {
        let ledger = StepLedger::default();
        self.ensure_stored_round_network(&scope.round_id, "the binding")
            .and_then(|()| {
                plan_round_classified(&self.database, &scope.round_id, &scope.proposal_ids())
            })
            .map_err(|error| self.step_voting_failure(error, Some(&scope.step), &ledger))
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
