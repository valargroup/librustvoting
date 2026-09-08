use super::fixtures::*;
use crate::{ObservabilityOptions, ObservationOutcome, ObservationScope};

#[tokio::test]
async fn cancelled_round_returns_enabled_diagnostics_with_round_identity() {
    let executor = executor();
    let control = ChainSubmissionControl::new(1);
    control.cancel();
    let report = RoundDriver::new(&executor)
        .run_with_report(
            &FixedHost,
            &control,
            &RecordingReporter::default(),
            Some(crate::ObservabilityOptions::default()),
        )
        .await;
    assert!(matches!(
        report.result.quiescence,
        RoundQuiescence::Cancelled
    ));
    let diagnostics = report.observability.unwrap();
    assert_eq!(diagnostics.outcome, ObservationOutcome::Cancelled);
    assert_eq!(diagnostics.round_id.as_deref(), Some(ROUND_ID));
}

#[test]
fn hashless_terminal_keeps_uncertainty_at_chain_step_and_round_boundaries() {
    let diagnostic = crate::ChainSubmissionDiagnostic::from_redacted_message(
        crate::ChainSubmissionDiagnosticKind::NullifierAlreadySpent,
        "previous dispatch landed",
    );
    for (chain, expected) in [
        (
            crate::ChainSubmissionResult::SubmittedWithoutHash(diagnostic.clone()),
            ObservationOutcome::PossiblyDispatched,
        ),
        (
            crate::ChainSubmissionResult::Rejected(diagnostic),
            ObservationOutcome::Rejected,
        ),
    ] {
        let step = crate::session::NextStep::AdvanceImportedDelegation { bundle_index: 0 };
        let step_result = Ok(crate::RoundStepOutcome {
            step: Some(step.clone()),
            disposition: crate::RoundStepDisposition::ChainTerminal,
            chain_outcome: Some(chain.clone()),
            share_deliveries: vec![],
            delegation: None,
            plan: executor().plan().unwrap(),
        });
        let round_result = crate::RoundRunReport {
            quiescence: RoundQuiescence::ChainTerminal {
                step,
                outcome: chain.clone(),
            },
            plan: None,
            tally: Default::default(),
            failures: vec![],
            skipped_bundles: vec![],
            chain_outcomes: vec![],
            share_deliveries: vec![],
            delegations: vec![],
        };
        assert_eq!(
            crate::observability::chain_result_outcome(&Ok(chain)),
            expected
        );
        assert_eq!(
            crate::observability::step_result_outcome(&step_result),
            expected
        );
        let owner = ObservationScope::new(Some(ObservabilityOptions::default())).invocation();
        let stage = owner.stage("round.run");
        let outcome = crate::observability::round_run_outcome(&round_result);
        stage.finish(outcome, None);
        let report = owner.complete("round_run", outcome, round_result);
        assert_eq!(report.observability.unwrap().outcome, expected);
    }
}
