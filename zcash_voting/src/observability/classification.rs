//! Projections of authoritative domain outcomes, without copying error messages.
use super::ObservationOutcome;
use crate::{
    ChainAdvanceOutcome, ChainSubmissionFailure, ChainSubmissionFailureKind, ChainSubmissionResult,
};

pub(crate) fn chain_error_kind(error: &ChainSubmissionFailure) -> &'static str {
    match error.kind() {
        ChainSubmissionFailureKind::InvalidInput => "InvalidInput",
        ChainSubmissionFailureKind::InvariantViolation => "InvariantViolation",
        ChainSubmissionFailureKind::Storage => "Storage",
        ChainSubmissionFailureKind::Transport => "Transport",
        ChainSubmissionFailureKind::Protocol => "Protocol",
    }
}

pub(crate) fn chain_result_outcome(
    result: &Result<ChainSubmissionResult, ChainSubmissionFailure>,
) -> ObservationOutcome {
    match result {
        Err(_) => ObservationOutcome::Failed,
        Ok(ChainSubmissionResult::Confirmed(_)) => ObservationOutcome::Succeeded,
        Ok(ChainSubmissionResult::Pending(_)) => ObservationOutcome::Pending,
        Ok(ChainSubmissionResult::Rejected(_)) => ObservationOutcome::Rejected,
        Ok(ChainSubmissionResult::SubmittedWithoutHash(_)) => {
            ObservationOutcome::PossiblyDispatched
        }
        Ok(ChainSubmissionResult::Cancelled) => ObservationOutcome::Cancelled,
    }
}

pub(crate) fn chain_episode_outcome(
    result: &Result<ChainAdvanceOutcome, ChainSubmissionFailure>,
) -> ObservationOutcome {
    match result {
        Err(_) => ObservationOutcome::Failed,
        Ok(ChainAdvanceOutcome::Confirmed(_)) => ObservationOutcome::Succeeded,
        Ok(ChainAdvanceOutcome::StillPending(_)) => ObservationOutcome::Pending,
        Ok(ChainAdvanceOutcome::Rejected(_)) => ObservationOutcome::Rejected,
        Ok(ChainAdvanceOutcome::SubmittedWithoutHash(_)) => ObservationOutcome::PossiblyDispatched,
        Ok(ChainAdvanceOutcome::Cancelled) => ObservationOutcome::Cancelled,
    }
}

pub(crate) fn step_attribution(step: &crate::session::NextStep) -> super::ObservationAttribution {
    use crate::session::NextStep;
    let (bundle, proposal, share) = match *step {
        NextStep::Delegate { bundle_index }
        | NextStep::AdvanceDelegation { bundle_index }
        | NextStep::AdvanceImportedDelegation { bundle_index } => (bundle_index, None, None),
        NextStep::CastVote {
            bundle_index,
            proposal_id,
            ..
        }
        | NextStep::AdvanceVote {
            bundle_index,
            proposal_id,
        }
        | NextStep::AdvanceVoteBatch {
            bundle_index,
            proposal_id,
        } => (bundle_index, Some(proposal_id), None),
        NextStep::SubmitShares {
            bundle_index,
            proposal_id,
            share_index,
        }
        | NextStep::ConfirmShare {
            bundle_index,
            proposal_id,
            share_index,
        } => (bundle_index, Some(proposal_id), Some(share_index)),
    };
    super::ObservationAttribution {
        bundle_index: Some(bundle),
        proposal_id: proposal,
        share_index: share,
    }
}

pub(crate) fn step_result_outcome(
    result: &Result<crate::RoundStepOutcome, crate::RoundStepFailure>,
) -> ObservationOutcome {
    use crate::RoundStepDisposition;
    match result {
        Err(_) => ObservationOutcome::Failed,
        Ok(result) => match result.disposition {
            RoundStepDisposition::NoWork => ObservationOutcome::NoWork,
            RoundStepDisposition::Advanced => ObservationOutcome::Succeeded,
            RoundStepDisposition::Pending => ObservationOutcome::Pending,
            RoundStepDisposition::Cancelled => ObservationOutcome::Cancelled,
            RoundStepDisposition::ChainTerminal => match &result.chain_outcome {
                Some(ChainSubmissionResult::SubmittedWithoutHash(_)) => {
                    ObservationOutcome::PossiblyDispatched
                }
                _ => ObservationOutcome::Rejected,
            },
        },
    }
}

pub(crate) fn round_run_outcome(result: &crate::RoundRunReport) -> ObservationOutcome {
    use crate::RoundQuiescence;
    match &result.quiescence {
        RoundQuiescence::Cancelled => ObservationOutcome::Cancelled,
        RoundQuiescence::Failures => ObservationOutcome::Failed,
        RoundQuiescence::NoWorkLeft => {
            if result.failures.is_empty() {
                ObservationOutcome::Succeeded
            } else {
                ObservationOutcome::Failed
            }
        }
        RoundQuiescence::ChainTerminal {
            outcome: ChainSubmissionResult::SubmittedWithoutHash(_),
            ..
        } => ObservationOutcome::PossiblyDispatched,
        RoundQuiescence::ChainTerminal { .. } => ObservationOutcome::Rejected,
        // The persisted plan can also contain a managed submission with no
        // projected next step; it does not establish a chain rejection.
        RoundQuiescence::PersistedChainTerminal => ObservationOutcome::Pending,
        _ => ObservationOutcome::Pending,
    }
}

pub(crate) fn helper_error_kind(error: &crate::HelperError) -> &'static str {
    use crate::HelperError;
    match error {
        HelperError::InvalidRequest { .. } => "InvalidInput",
        HelperError::Transport(_) => "Transport",
        HelperError::Status { .. } => "HttpStatus",
        HelperError::Decode { .. } => "Protocol",
        HelperError::AmbiguousSubmissionResponse { .. } => "PossiblyDispatched",
        HelperError::DeadlineExceeded => "DeadlineExceeded",
        HelperError::Cancelled => "Cancelled",
    }
}

/// Classifies an SDK error without recording its potentially sensitive message.
pub(crate) fn voting_error_kind(error: &crate::VotingError) -> &'static str {
    match error.kind() {
        crate::VotingErrorKind::InvalidInput => "InvalidInput",
        crate::VotingErrorKind::KeystoneSignatureConflict => "KeystoneSignatureConflict",
        crate::VotingErrorKind::ProofFailed => "ProofFailed",
        crate::VotingErrorKind::Busy => "Busy",
        crate::VotingErrorKind::Storage => "Storage",
        crate::VotingErrorKind::Internal => "Internal",
        crate::VotingErrorKind::InsufficientEligibility => "InsufficientEligibility",
        crate::VotingErrorKind::NoSpendableNotes => "NoSpendableNotes",
        crate::VotingErrorKind::SetupAlreadyPersisted => "SetupAlreadyPersisted",
        crate::VotingErrorKind::DbBusy => "DbBusy",
        crate::VotingErrorKind::PirUnavailable => "PirUnavailable",
        crate::VotingErrorKind::DelegationTargetMismatch => "DelegationTargetMismatch",
        crate::VotingErrorKind::DelegationAlreadyBroadcast => "DelegationAlreadyBroadcast",
    }
}

/// Projects delegation setup, where "already persisted" is not a failure.
///
/// `setup` is idempotent by construction: re-running it for a bundle whose
/// PCZT sighash or tx1 effects are already stored returns
/// [`crate::VotingError::SetupAlreadyPersisted`], and
/// `DelegationPipeline::ensure_setup` catches exactly those two fields and
/// continues by validating the persisted proof. Recording that branch as
/// `Failed` reports a failure for work the round completed normally, and every
/// re-attempt of a round produces one per bundle — noise that trains a reader
/// to ignore `failed`, which is how a real failure gets missed.
///
/// [`ObservationOutcome::Reused`] is the same projection the SDK already
/// applies to a reused proof and to a duplicate share submission.
///
/// The field match is deliberately narrow.
/// [`crate::types::DelegationSetupField::PaddedNoteSecrets`] is **not**
/// tolerated by `ensure_setup`: it means the stored setup belongs to different
/// notes, the caller propagates it, and it must keep reporting `Failed`.
pub(crate) fn delegation_setup_outcome<T>(
    result: &Result<T, crate::VotingError>,
) -> ObservationOutcome {
    match result {
        Ok(_) => ObservationOutcome::Succeeded,
        Err(crate::VotingError::SetupAlreadyPersisted { field, .. })
            if matches!(
                field,
                crate::types::DelegationSetupField::PcztSighash
                    | crate::types::DelegationSetupField::Tx1Effects
            ) =>
        {
            ObservationOutcome::Reused
        }
        Err(_) => ObservationOutcome::Failed,
    }
}

/// Projects proof completion identically at stage and invocation boundaries.
pub(crate) fn delegation_proof_outcome(
    result: &Result<crate::delegate::DelegationProofStatus, crate::VotingError>,
) -> ObservationOutcome {
    match result {
        Ok(crate::delegate::DelegationProofStatus::Reused) => ObservationOutcome::Reused,
        Ok(crate::delegate::DelegationProofStatus::Generated) => ObservationOutcome::Succeeded,
        Err(_) => ObservationOutcome::Failed,
    }
}
