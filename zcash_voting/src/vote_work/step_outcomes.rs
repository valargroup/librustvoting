//! Step outcome construction and failure projection shared by every step.

use crate::{
    session::{NextStep, RoundPlan},
    ChainAdvanceOutcome, ChainSubmissionFailure, ChainSubmissionFailureKind, ChainSubmissionResult,
    ChainTransport, VotingError, VotingErrorKind,
};

use super::{
    execution::bounded_message, RoundExecutor, RoundStepDisposition, RoundStepFailure,
    RoundStepFailureKind, RoundStepOutcome, RoundStepProgress, RoundStepProgressReporter,
    VoteShareDeliveryReport,
};

impl<T: ChainTransport> RoundExecutor<T> {
    pub(super) fn chain_step_outcome(
        &self,
        step: NextStep,
        outcome: ChainAdvanceOutcome,
        delegation: Option<crate::delegate::SignedDelegationBundle>,
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
        self.outcome(step, disposition, Some(result), Vec::new(), delegation)
    }

    pub(super) async fn blocking<R: Send + 'static>(
        &self,
        step: &NextStep,
        label: &str,
        work: impl FnOnce() -> Result<R, VotingError> + Send + 'static,
    ) -> Result<R, RoundStepFailure> {
        tokio::task::spawn_blocking(work)
            .await
            .map_err(|error| {
                self.step_failure(
                    RoundStepFailureKind::InvariantViolation,
                    Some(step.clone()),
                    None,
                    None,
                    format!("{label} task failed: {error}"),
                )
            })?
            .map_err(|error| self.step_voting_failure(error, Some(step.clone())))
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
        step: NextStep,
        disposition: RoundStepDisposition,
        chain_outcome: Option<ChainSubmissionResult>,
        share_deliveries: Vec<VoteShareDeliveryReport>,
        delegation: Option<crate::delegate::SignedDelegationBundle>,
    ) -> Result<RoundStepOutcome, RoundStepFailure> {
        let plan = self.plan().map_err(|error| {
            self.step_voting_failure_after_chain(error, Some(step.clone()), chain_outcome.clone())
        })?;
        Ok(RoundStepOutcome {
            step: Some(step),
            disposition,
            chain_outcome,
            share_deliveries,
            delegation,
            plan,
        })
    }

    pub(super) fn step_cancelled(
        &self,
        step: Option<NextStep>,
        chain_outcome: Option<ChainSubmissionResult>,
        share_deliveries: Vec<VoteShareDeliveryReport>,
        delegation: Option<crate::delegate::SignedDelegationBundle>,
    ) -> Result<RoundStepOutcome, RoundStepFailure> {
        let plan = self.plan().map_err(|error| {
            self.step_voting_failure_after_chain(error, step.clone(), chain_outcome.clone())
        })?;
        Ok(RoundStepOutcome {
            step,
            disposition: RoundStepDisposition::Cancelled,
            chain_outcome,
            share_deliveries,
            delegation,
            plan,
        })
    }

    pub(super) fn step_voting_failure(
        &self,
        error: VotingError,
        step: Option<NextStep>,
    ) -> RoundStepFailure {
        self.step_voting_failure_after_chain(error, step, None)
    }

    /// [`Self::step_voting_failure`] for an error raised after the chain
    /// already produced `chain_outcome`, which stays on the failure so a
    /// durable confirmation is not lost behind a later delivery error.
    pub(super) fn step_voting_failure_after_chain(
        &self,
        error: VotingError,
        step: Option<NextStep>,
        chain_outcome: Option<ChainSubmissionResult>,
    ) -> RoundStepFailure {
        let kind = match error.kind() {
            VotingErrorKind::InvalidInput
            | VotingErrorKind::InsufficientEligibility
            | VotingErrorKind::NoSpendableNotes
            | VotingErrorKind::SetupAlreadyPersisted => RoundStepFailureKind::InvalidInput,
            VotingErrorKind::Busy | VotingErrorKind::DbBusy => RoundStepFailureKind::Busy,
            VotingErrorKind::Storage => RoundStepFailureKind::Storage,
            VotingErrorKind::PirUnavailable => RoundStepFailureKind::Transport,
            VotingErrorKind::ProofFailed => RoundStepFailureKind::ProofFailed,
            VotingErrorKind::KeystoneSignatureConflict => RoundStepFailureKind::Signing,
            VotingErrorKind::Internal => RoundStepFailureKind::InvariantViolation,
        };
        self.step_failure(kind, step, None, chain_outcome, error.to_string())
    }

    pub(super) fn step_chain_failure(
        &self,
        error: ChainSubmissionFailure,
        step: Option<NextStep>,
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
        self.step_failure(kind, step, error.strongest_state(), None, error.message())
    }

    pub(super) fn step_failure(
        &self,
        kind: RoundStepFailureKind,
        step: Option<NextStep>,
        strongest_chain_state: Option<crate::ChainSubmissionFailureState>,
        chain_outcome: Option<ChainSubmissionResult>,
        message: impl AsRef<str>,
    ) -> RoundStepFailure {
        RoundStepFailure {
            kind,
            step,
            strongest_chain_state,
            chain_outcome,
            message: bounded_message(message.as_ref()),
            plan: self.plan().ok().map(Box::new),
        }
    }
}
