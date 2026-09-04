//! Typed PIR failure classification: retryability follows phase and status,
//! and the typed value survives `anyhow` context wrapping.

use super::super::{PirHttpFailure, PirHttpFailurePhase as Phase};

#[test]
fn retryability_follows_phase_and_status() {
    let retryable = |phase, http_status| PirHttpFailure { phase, http_status }.retryable();
    assert!(retryable(Phase::Connect, None));
    assert!(retryable(Phase::Send, None));
    assert!(retryable(Phase::Body, Some(200)));
    assert!(retryable(Phase::Timeout, None));
    assert!(retryable(Phase::Status, Some(408)));
    assert!(retryable(Phase::Status, Some(429)));
    assert!(retryable(Phase::Status, Some(503)));
    assert!(!retryable(Phase::Status, Some(404)));
    assert!(!retryable(Phase::Status, Some(400)));
    assert!(!retryable(Phase::Build, None));
}

#[test]
fn typed_failure_survives_anyhow_context_and_maps_to_pir_unavailable() {
    let error = PirHttpFailure {
        phase: Phase::Status,
        http_status: Some(502),
    }
    .wrap("PIR HTTP status 502 body=".to_string())
    .context("fetch proof");
    let typed = PirHttpFailure::from_error_chain(&error).expect("typed failure in chain");
    assert_eq!(typed.http_status, Some(502));

    let mapped =
        crate::pir::map_pir_fetch_error(Some("https://pir"), "PIR parallel fetch failed", error);
    assert!(mapped.retryable());
    assert_eq!(mapped.kind(), crate::VotingErrorKind::PirUnavailable);
    let text = mapped.to_string();
    assert!(text.contains("PIR parallel fetch failed"), "{text}");
    assert!(text.contains("https://pir"), "{text}");

    let foreign = crate::pir::map_pir_fetch_error(
        None,
        "PIR parallel fetch failed",
        anyhow::anyhow!("opaque"),
    );
    assert!(!foreign.retryable());
}
