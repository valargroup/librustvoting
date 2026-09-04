//! `VotingErrorView`: structured payloads survive the wire round trip and an
//! unknown category degrades to `Other` instead of failing the parse.

use crate::{wire::VotingErrorKindView, VotingError};

#[test]
fn voting_error_view_carries_structured_payloads() {
    use crate::types::DelegationSetupField;

    let eligibility = VotingError::InsufficientEligibility {
        required_weight_zatoshi: 12_500_000,
        selected_weight_zatoshi: 3,
        snapshot_height: None,
        required_notes: 5,
        selected_notes: 2,
    }
    .with_snapshot_height(42)
    .to_view();
    assert_eq!(
        eligibility.kind,
        VotingErrorKindView::InsufficientEligibility
    );
    assert!(!eligibility.retryable);
    assert_eq!(eligibility.snapshot_height, Some(42));
    assert_eq!(eligibility.required_weight_zatoshi, Some(12_500_000));
    assert_eq!(eligibility.selected_weight_zatoshi, Some(3));
    assert_eq!(eligibility.required_notes, Some(5));
    assert_eq!(eligibility.selected_notes, Some(2));
    assert!(eligibility.message.contains("at snapshot height 42"));

    let busy = VotingError::DbBusy {
        message: "locked".to_string(),
    }
    .to_view();
    assert_eq!(busy.kind, VotingErrorKindView::DbBusy);
    assert!(busy.retryable);

    let pir = VotingError::PirUnavailable {
        endpoint: Some("https://pir".to_string()),
        http_status: Some(503),
        retryable: true,
        message: "m".to_string(),
    }
    .to_view();
    assert_eq!(pir.kind, VotingErrorKindView::PirUnavailable);
    assert_eq!(pir.http_status, Some(503));
    assert_eq!(pir.endpoint.as_deref(), Some("https://pir"));
    assert!(pir.retryable);

    let setup = VotingError::SetupAlreadyPersisted {
        round_id: "r".to_string(),
        bundle_index: 3,
        field: DelegationSetupField::Tx1Effects,
    }
    .to_view();
    assert_eq!(setup.kind, VotingErrorKindView::SetupAlreadyPersisted);
    assert_eq!(setup.bundle_index, Some(3));

    let conflict = VotingError::KeystoneSignatureConflict { bundle_index: 9 }.to_view();
    assert_eq!(conflict.bundle_index, Some(9));

    let json = serde_json::to_string(&pir).unwrap();
    assert!(json.contains("\"kind\":\"pir_unavailable\""), "{json}");
    let round_trip: crate::wire::VotingErrorView = serde_json::from_str(&json).unwrap();
    assert_eq!(round_trip, pir);
}

#[test]
fn an_unknown_error_kind_deserializes_as_other() {
    let known = VotingError::Busy {
        message: "busy".to_string(),
    }
    .to_view();
    let json = serde_json::to_string(&known).unwrap().replace(
        "\"kind\":\"busy\"",
        "\"kind\":\"kind_added_after_this_host_shipped\"",
    );

    let view: crate::wire::VotingErrorView =
        serde_json::from_str(&json).expect("a category this host does not know must still parse");

    assert_eq!(view.kind, VotingErrorKindView::Other);
    assert_eq!(view.message, known.message);
}

#[test]
fn an_unknown_kind_with_a_new_structured_field_still_parses() {
    let known = VotingError::Busy {
        message: "busy".to_string(),
    }
    .to_view();
    let json = serde_json::to_string(&known).unwrap().replace(
        "\"kind\":\"busy\"",
        "\"kind\":\"quota_exhausted\",\"quota_remaining\":0",
    );

    let view: crate::wire::VotingErrorView = serde_json::from_str(&json)
        .expect("a field added for a newer category must not reject the payload");

    assert_eq!(view.kind, VotingErrorKindView::Other);
    assert_eq!(view.message, known.message);
}
