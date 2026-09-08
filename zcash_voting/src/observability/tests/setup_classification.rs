//! `delegation::setup` must report reuse as reuse, and everything else as-is.
//!
//! The projection exists because `setup` is idempotent: a re-attempted round
//! re-runs it per bundle and gets `SetupAlreadyPersisted` back for work that
//! already succeeded. These tests pin which fields are tolerated, because the
//! tolerated set is what `DelegationPipeline::ensure_setup` actually catches —
//! widening it here would hide a genuine error behind a benign outcome.

use crate::observability::delegation_setup_outcome;
use crate::types::DelegationSetupField;
use crate::{ObservationOutcome, VotingError};

fn already_persisted(field: DelegationSetupField) -> Result<(), VotingError> {
    Err(VotingError::SetupAlreadyPersisted {
        round_id: "round".to_string(),
        bundle_index: 0,
        field,
    })
}

#[test]
fn success_is_succeeded() {
    assert_eq!(
        delegation_setup_outcome(&Ok::<_, VotingError>(())),
        ObservationOutcome::Succeeded
    );
}

/// The three fields `ensure_setup` catches and continues from.
#[test]
fn tolerated_already_persisted_fields_are_reused() {
    for field in [
        DelegationSetupField::PcztSighash,
        DelegationSetupField::Tx1Effects,
        DelegationSetupField::DelegationPczt,
    ] {
        assert_eq!(
            delegation_setup_outcome(&already_persisted(field)),
            ObservationOutcome::Reused,
            "{field:?} is caught by ensure_setup and must not report a failure"
        );
    }
}

/// The load-bearing half: a setup persisted for different notes is propagated
/// by `ensure_setup`, so relabelling it would hide a real error.
#[test]
fn padded_note_secrets_mismatch_still_fails() {
    assert_eq!(
        delegation_setup_outcome(&already_persisted(DelegationSetupField::PaddedNoteSecrets)),
        ObservationOutcome::Failed,
        "PaddedNoteSecrets is propagated by ensure_setup and must stay a failure"
    );
}

/// Pins the classification of every currently defined setup field.
#[test]
fn every_setup_field_variant_is_covered() {
    let classified = [
        DelegationSetupField::PaddedNoteSecrets,
        DelegationSetupField::PcztSighash,
        DelegationSetupField::Tx1Effects,
        DelegationSetupField::DelegationPczt,
    ]
    .map(|field| delegation_setup_outcome(&already_persisted(field)));
    assert_eq!(
        classified,
        [
            ObservationOutcome::Failed,
            ObservationOutcome::Reused,
            ObservationOutcome::Reused,
            ObservationOutcome::Reused,
        ]
    );
}

/// Reuse is keyed on the error, not on "any error from setup".
#[test]
fn unrelated_errors_still_fail() {
    let errors = [
        VotingError::InvalidInput {
            message: "branch id mismatch".to_string(),
        },
        VotingError::Storage {
            message: "db gone".to_string(),
        },
        VotingError::Internal {
            message: "boom".to_string(),
        },
    ];
    for error in errors {
        assert_eq!(
            delegation_setup_outcome(&Err::<(), _>(error)),
            ObservationOutcome::Failed
        );
    }
}

/// The error category is reported independently of the outcome, so a reused
/// setup is still identifiable in a report.
#[test]
fn reused_setup_still_reports_its_error_kind() {
    let error = already_persisted(DelegationSetupField::PcztSighash).unwrap_err();
    assert_eq!(
        crate::observability::voting_error_kind(&error),
        "SetupAlreadyPersisted"
    );
}
