//! Host-facing projection of a PIR snapshot-height probe.
//!
//! The projection must be total: every status a probe can report reaches the
//! view, and every field of a diagnostic survives it. A host chooses an
//! endpoint from these, so a status that silently collapsed into another would
//! make it pick a PIR server that cannot answer for the round's snapshot.

use crate::pir_snapshot::{
    classify_pir_snapshot_height, PirSnapshotEndpointDiagnostic, PirSnapshotEndpointStatus,
};
use crate::wire::{PirSnapshotEndpointDiagnosticView, PirSnapshotEndpointStatusView};

/// Every status the probe classifier and the transport paths can produce.
const EVERY_STATUS: [PirSnapshotEndpointStatus; 7] = [
    PirSnapshotEndpointStatus::Matched,
    PirSnapshotEndpointStatus::Behind,
    PirSnapshotEndpointStatus::Ahead,
    PirSnapshotEndpointStatus::MissingHeight,
    PirSnapshotEndpointStatus::MalformedJson,
    PirSnapshotEndpointStatus::NonSuccessStatus,
    PirSnapshotEndpointStatus::TimeoutOrNetworkError,
];

#[test]
fn every_status_maps_to_a_distinct_view() {
    let views: Vec<PirSnapshotEndpointStatusView> =
        EVERY_STATUS.into_iter().map(Into::into).collect();

    // Distinctness is the point: a host picks an endpoint from these, so two
    // statuses collapsing into one would let it choose a server that cannot
    // answer for the round's snapshot.
    for (index, view) in views.iter().enumerate() {
        for other in &views[index + 1..] {
            assert_ne!(view, other, "two statuses share a view");
        }
    }
    assert_eq!(views[0], PirSnapshotEndpointStatusView::Matched);
    assert_eq!(
        views[6],
        PirSnapshotEndpointStatusView::TimeoutOrNetworkError
    );
}

#[test]
fn a_diagnostic_reaches_the_view_whole() {
    let diagnostic = PirSnapshotEndpointDiagnostic {
        endpoint: "https://pir-1.example".to_string(),
        status: PirSnapshotEndpointStatus::NonSuccessStatus,
        reported_height: Some(2_100_000),
        http_status_code: Some(503),
        message: Some("service unavailable".to_string()),
    };

    let view = PirSnapshotEndpointDiagnosticView::from(diagnostic);

    assert_eq!(view.endpoint, "https://pir-1.example");
    assert_eq!(view.status, PirSnapshotEndpointStatusView::NonSuccessStatus);
    assert_eq!(view.reported_height, Some(2_100_000));
    assert_eq!(view.http_status_code, Some(503));
    assert_eq!(view.message.as_deref(), Some("service unavailable"));
}

#[test]
fn an_endpoint_that_never_answered_carries_no_height_or_code() {
    let view = PirSnapshotEndpointDiagnosticView::from(PirSnapshotEndpointDiagnostic {
        endpoint: "https://pir-2.example".to_string(),
        status: PirSnapshotEndpointStatus::TimeoutOrNetworkError,
        reported_height: None,
        http_status_code: None,
        message: None,
    });

    assert_eq!(view.reported_height, None);
    assert_eq!(view.http_status_code, None);
    assert_eq!(view.message, None);
}

#[test]
fn a_behind_endpoint_keeps_the_height_it_reported() {
    // The height matters beside the status: a host showing why an endpoint was
    // rejected needs how far behind it is, not only that it was.
    let view = PirSnapshotEndpointDiagnosticView::from(classify_pir_snapshot_height(
        "https://pir-3.example",
        2_100_000,
        Some(2_099_000),
    ));

    assert_eq!(view.status, PirSnapshotEndpointStatusView::Behind);
    assert_eq!(view.reported_height, Some(2_099_000));
}

#[test]
fn the_status_serializes_as_the_snake_case_name_the_host_reads() {
    // Pinned because the host contract is the wire name, not the variant.
    let json = serde_json::to_string(&PirSnapshotEndpointStatusView::MissingHeight).unwrap();
    assert_eq!(json, r#""missing_height""#);

    let parsed: PirSnapshotEndpointStatusView =
        serde_json::from_str(r#""timeout_or_network_error""#).unwrap();
    assert_eq!(parsed, PirSnapshotEndpointStatusView::TimeoutOrNetworkError);
}

#[test]
fn a_diagnostic_view_round_trips_through_json() {
    let view = PirSnapshotEndpointDiagnosticView::from(classify_pir_snapshot_height(
        "https://pir-4.example",
        2_100_000,
        Some(2_100_000),
    ));

    let json = serde_json::to_string(&view).unwrap();
    let parsed: PirSnapshotEndpointDiagnosticView = serde_json::from_str(&json).unwrap();

    assert_eq!(parsed, view);
    assert_eq!(parsed.status, PirSnapshotEndpointStatusView::Matched);
}
