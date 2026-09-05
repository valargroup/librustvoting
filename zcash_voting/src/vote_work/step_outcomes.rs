//! Step outcome construction and failure projection shared by every step.
//! Every constructor takes the step's ledger, so what the step already
//! accomplished rides on whatever it reports.

use crate::{
    session::{NextStep, RoundPlan},
    ChainAdvanceOutcome, ChainSubmissionFailure, ChainSubmissionFailureKind, ChainTransport,
    VotingError, VotingErrorKind,
};

use super::{
    step_ledger::StepLedger, step_scope::bounded_message, step_scope::StepScope, RoundExecutor,
    RoundStepDisposition, RoundStepFailure, RoundStepFailureKind, RoundStepOutcome,
    RoundStepProgress, RoundStepProgressReporter,
};

impl<T: ChainTransport> RoundExecutor<T> {
    /// The outcome of a step whose only work was one chain episode.
    pub(super) fn chain_step_outcome(
        &self,
        scope: &StepScope<'_>,
        outcome: ChainAdvanceOutcome,
        mut ledger: StepLedger,
        progress: &dyn RoundStepProgressReporter,
    ) -> Result<RoundStepOutcome, RoundStepFailure> {
        let disposition = match &outcome {
            ChainAdvanceOutcome::Confirmed(_) => RoundStepDisposition::Advanced,
            ChainAdvanceOutcome::StillPending(_) => RoundStepDisposition::Pending,
            ChainAdvanceOutcome::Cancelled => RoundStepDisposition::Cancelled,
            ChainAdvanceOutcome::SubmittedWithoutHash(_) | ChainAdvanceOutcome::Rejected(_) => {
                RoundStepDisposition::ChainTerminal
            }
        };
        let result = outcome.into_result();
        progress.report(RoundStepProgress::ChainOutcome(result.clone()));
        ledger.record_chain_outcome(result);
        self.outcome(scope, disposition, ledger)
    }

    pub(super) async fn blocking<R: Send + 'static>(
        &self,
        scope: &StepScope<'_>,
        label: &str,
        work: impl FnOnce() -> Result<R, VotingError> + Send + 'static,
    ) -> Result<R, RoundStepFailure> {
        let ledger = StepLedger::default();
        tokio::task::spawn_blocking(work)
            .await
            .map_err(|error| {
                self.step_failure(
                    RoundStepFailureKind::InvariantViolation,
                    Some(&scope.step),
                    None,
                    &ledger,
                    format!("{label} task failed: {error}"),
                )
            })?
            .map_err(|error| self.step_voting_failure(error, Some(&scope.step), &ledger))
    }

    pub(super) fn no_work(&self, step: Option<NextStep>, plan: RoundPlan) -> RoundStepOutcome {
        RoundStepOutcome {
            step,
            disposition: RoundStepDisposition::NoWork,
            chain_outcome: None,
            share_deliveries: Vec::new(),
            delegation: None,
            plan,
        }
    }

    pub(super) fn outcome(
        &self,
        scope: &StepScope<'_>,
        disposition: RoundStepDisposition,
        ledger: StepLedger,
    ) -> Result<RoundStepOutcome, RoundStepFailure> {
        let plan = self
            .plan()
            .map_err(|error| self.step_voting_failure(error, Some(&scope.step), &ledger))?;
        Ok(RoundStepOutcome {
            step: Some(scope.step.clone()),
            disposition,
            chain_outcome: ledger.chain_outcome,
            share_deliveries: ledger.share_deliveries,
            delegation: ledger.delegation,
            plan,
        })
    }

    pub(super) fn step_cancelled(
        &self,
        scope: &StepScope<'_>,
        ledger: StepLedger,
    ) -> Result<RoundStepOutcome, RoundStepFailure> {
        self.outcome(scope, RoundStepDisposition::Cancelled, ledger)
    }

    pub(super) fn step_voting_failure(
        &self,
        error: VotingError,
        step: Option<&NextStep>,
        ledger: &StepLedger,
    ) -> RoundStepFailure {
        let kind = failure_kind_for(&error);
        self.step_failure(kind, step, None, ledger, error.to_string())
    }

    pub(super) fn step_chain_failure(
        &self,
        error: ChainSubmissionFailure,
        step: Option<&NextStep>,
        ledger: &StepLedger,
    ) -> RoundStepFailure {
        let kind = match error.kind() {
            ChainSubmissionFailureKind::InvalidInput => RoundStepFailureKind::InvalidInput,
            ChainSubmissionFailureKind::InvariantViolation => {
                RoundStepFailureKind::InvariantViolation
            }
            ChainSubmissionFailureKind::Storage => RoundStepFailureKind::Storage,
            ChainSubmissionFailureKind::Transport => RoundStepFailureKind::Transport,
            ChainSubmissionFailureKind::Protocol => RoundStepFailureKind::Protocol,
        };
        self.step_failure(kind, step, error.strongest_state(), ledger, error.message())
    }

    /// A failure carrying the strongest truthful durable state, everything
    /// the step already accomplished, and a refreshed plan.
    pub(super) fn step_failure(
        &self,
        kind: RoundStepFailureKind,
        step: Option<&NextStep>,
        strongest_chain_state: Option<crate::ChainSubmissionFailureState>,
        ledger: &StepLedger,
        message: impl AsRef<str>,
    ) -> RoundStepFailure {
        RoundStepFailure {
            kind,
            step: step.cloned(),
            strongest_chain_state,
            chain_outcome: ledger.chain_outcome.clone(),
            message: bounded_message(message.as_ref()),
            plan: self.plan().ok().map(Box::new),
            share_deliveries: ledger.share_deliveries.clone(),
        }
    }
}

/// The step failure kind a [`VotingError`] presents as.
///
/// Shared with the round driver, which classifies a planning error the same
/// way a step would so a host reads one taxonomy.
pub(crate) fn failure_kind_for(error: &VotingError) -> RoundStepFailureKind {
    match error.kind() {
        VotingErrorKind::InvalidInput | VotingErrorKind::SetupAlreadyPersisted => {
            RoundStepFailureKind::InvalidInput
        }
        VotingErrorKind::InsufficientEligibility => RoundStepFailureKind::InsufficientEligibility,
        VotingErrorKind::NoSpendableNotes => RoundStepFailureKind::NoSpendableNotes,
        VotingErrorKind::Busy | VotingErrorKind::DbBusy => RoundStepFailureKind::Busy,
        VotingErrorKind::Storage => RoundStepFailureKind::Storage,
        VotingErrorKind::PirUnavailable => RoundStepFailureKind::Transport,
        VotingErrorKind::ProofFailed => RoundStepFailureKind::ProofFailed,
        VotingErrorKind::KeystoneSignatureConflict => RoundStepFailureKind::Signing,
        VotingErrorKind::Internal => RoundStepFailureKind::InvariantViolation,
        VotingErrorKind::DelegationTargetMismatch => RoundStepFailureKind::DelegationTargetMismatch,
        // A refusal to clear recovery state is a host asking for something the
        // round no longer permits, not a step that failed on its own terms.
        VotingErrorKind::DelegationAlreadyBroadcast => RoundStepFailureKind::InvalidInput,
    }
}
