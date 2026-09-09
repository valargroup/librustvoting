use super::fixtures::{pipeline_with_round, ROUND_ID};
use crate::{ObservabilityOptions, ObservationOutcome};

#[test]
fn eligibility_reports_preserve_wallet_failures_and_round_identity() {
    let pipeline = pipeline_with_round();
    let plain = pipeline.eligibility().unwrap_err().to_string();
    for options in [None, Some(ObservabilityOptions::default())] {
        let report = pipeline.eligibility_with_report(options);
        assert_eq!(report.result.unwrap_err().to_string(), plain);
        assert_eq!(report.observability.is_some(), options.is_some());
        if let Some(diagnostics) = report.observability {
            assert_eq!(diagnostics.round_id.as_deref(), Some(ROUND_ID));
            assert_eq!(diagnostics.outcome, ObservationOutcome::Failed);
            assert_eq!(
                diagnostics.records[0].stage.as_ref(),
                "delegation::eligibility"
            );
        }
    }
}

#[test]
fn keystone_reports_preserve_preparation_failures_and_bundle_identity() {
    let pipeline = pipeline_with_round();
    let plain = pipeline.keystone_request(0).unwrap_err().to_string();
    for options in [None, Some(ObservabilityOptions::default())] {
        let report = pipeline.keystone_request_with_report(0, options);
        assert_eq!(report.result.unwrap_err().to_string(), plain);
        assert_eq!(report.observability.is_some(), options.is_some());
        if let Some(diagnostics) = report.observability {
            assert_eq!(diagnostics.round_id.as_deref(), Some(ROUND_ID));
            assert_eq!(diagnostics.outcome, ObservationOutcome::Failed);
            assert_eq!(diagnostics.records[0].attribution.bundle_index, Some(0));
            assert_eq!(
                diagnostics.records[0].stage.as_ref(),
                "delegation::keystone_request"
            );
        }
    }
}

#[test]
fn proof_preparation_reports_selection_before_a_preparation_failure() {
    use crate::{
        delegate::DelegationProgress, types::DelegationProgressBridge, HyperTransport, PirFleet,
    };
    use std::sync::{Arc, Mutex};

    let pipeline = pipeline_with_round();
    let pir = PirFleet::new(
        &["https://pir.invalid".to_string()],
        crate::config::PirLayout {
            pir_depth: pir_types::COMPILED_PIR_LAYOUT.pir_depth as u32,
            tier0_layers: pir_types::COMPILED_PIR_LAYOUT.tier0_layers as u32,
            tier1_layers: pir_types::COMPILED_PIR_LAYOUT.tier1_layers as u32,
            poly_len: pir_types::DEFAULT_YPIR_POLY_LEN as u32,
        },
        Arc::new(HyperTransport::new()),
    )
    .unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&events);
    let progress = DelegationProgressBridge::new(move |event| recorded.lock().unwrap().push(event));
    // This fixture cannot prepare a bundle. The host must still see that
    // selection began, without a fictitious PCZT/proof completion event.
    assert!(pipeline.ensure_proof(0, &pir, &progress).is_err());
    assert_eq!(
        *events.lock().unwrap(),
        vec![DelegationProgress::SelectingNotes]
    );
}
