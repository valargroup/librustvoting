//! Typed error kinds: SQLite classification and the retryability of each kind.

use super::super::{DelegationSetupField, VotingError, VotingErrorKind};

#[test]
fn sqlite_busy_and_locked_map_to_db_busy() {
    for code in [
        rusqlite::ErrorCode::DatabaseBusy,
        rusqlite::ErrorCode::DatabaseLocked,
    ] {
        let error = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code,
                extended_code: 5,
            },
            Some("database is locked".to_string()),
        );
        let mapped = VotingError::from(error);
        assert_eq!(mapped.kind(), VotingErrorKind::DbBusy);
        assert!(mapped.retryable());
    }
    let other = VotingError::from(rusqlite::Error::InvalidQuery);
    assert_eq!(other.kind(), VotingErrorKind::Storage);
    assert!(!other.retryable());
}

#[test]
fn eligibility_error_text_matches_legacy_wording_with_optional_height() {
    let error = VotingError::InsufficientEligibility {
        required_weight_zatoshi: 12_500_000,
        selected_weight_zatoshi: 30,
        snapshot_height: None,
        bundle_note_slots: 5,
        selected_notes: 2,
    };
    assert_eq!(
        error.to_string(),
        "minimum voting eligibility requires at least one eligible voting bundle with 12500000 zatoshi voting weight; selected 2 distinct notes across eligible bundles with 30 zatoshi eligible bundle weight"
    );
    let with_height = error.with_snapshot_height(42);
    assert!(with_height.to_string().ends_with(" at snapshot height 42"));
    assert_eq!(with_height.kind(), VotingErrorKind::InsufficientEligibility);
    assert!(!with_height.retryable());

    let unchanged = VotingError::NoSpendableNotes { snapshot_height: 7 }.with_snapshot_height(9);
    assert_eq!(
        unchanged.to_string(),
        "no spendable voting notes at snapshot height 7"
    );

    let setup = VotingError::SetupAlreadyPersisted {
        round_id: "ab".to_string(),
        bundle_index: 3,
        field: DelegationSetupField::PcztSighash,
    };
    assert_eq!(
        setup.to_string(),
        "refusing to overwrite pczt_sighash for round=ab, bundle=3"
    );
}

/// The mismatch a host can actually see names the way out of it.
///
/// Proof reuse rebuilds this silently, so the one path that surfaces it is
/// Keystone signing, which refuses on purpose: the device signed the exact
/// PCZT the stored setup describes. A hardware voter who is only told the
/// setup is wrong has nowhere to go, so the message carries the recovery.
#[test]
fn a_target_mismatch_names_the_recovery_that_rebuilds_the_bundle() {
    let error = VotingError::DelegationTargetMismatch {
        bundle_index: 2,
        van_matches: false,
        commitment_matches: false,
    };

    assert_eq!(error.kind(), VotingErrorKind::DelegationTargetMismatch);
    assert_eq!(
        error.to_string(),
        "bundle 2's stored delegation target does not reproduce from this voting hotkey \
         (van_matches=false, commitment_matches=false); if the bundle was never broadcast, \
         re-run delegation preparation to rebuild it"
    );
    // Retrying the same call with the same key never succeeds.
    assert!(!error.retryable());
}

/// A refusal to clear state that may be on chain says what it kept and why,
/// and offers no recovery, because there is none to offer.
#[test]
fn an_already_broadcast_refusal_reports_the_evidence_it_found() {
    let error = VotingError::DelegationAlreadyBroadcast {
        bundle_index: 1,
        evidence: "a delegation transaction hash".to_string(),
    };

    assert_eq!(error.kind(), VotingErrorKind::DelegationAlreadyBroadcast);
    assert!(
        error
            .to_string()
            .contains("may already be on chain (a delegation transaction hash)"),
        "{error}"
    );
    assert!(
        error.to_string().contains("recovery state was kept"),
        "{error}"
    );
    assert!(!error.retryable());
}
