//! `ChainDiagnosticKindView`: every lifecycle diagnostic category has a
//! stable wire name that survives the round trip.

use crate::{wire::ChainDiagnosticKindView, ChainSubmissionDiagnosticKind};

#[test]
fn every_diagnostic_kind_round_trips_under_its_stable_name() {
    for (kind, name) in [
        (
            ChainSubmissionDiagnosticKind::AmbiguousDispatch,
            "ambiguous_dispatch",
        ),
        (
            ChainSubmissionDiagnosticKind::EndpointUnsupported,
            "endpoint_unsupported",
        ),
        (
            ChainSubmissionDiagnosticKind::InvalidProtocolResponse,
            "invalid_protocol_response",
        ),
    ] {
        assert_eq!(kind.as_str(), name);
        let view: ChainDiagnosticKindView = kind.into();
        let json = serde_json::to_string(&view).unwrap();
        assert_eq!(json, format!("\"{name}\""));
        let round_trip: ChainDiagnosticKindView = serde_json::from_str(&json).unwrap();
        assert_eq!(round_trip, view);
    }
}
