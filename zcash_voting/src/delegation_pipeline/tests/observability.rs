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
