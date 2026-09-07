//! A failed obligation isolates its bundle, or ends the run, per policy.

use super::fixtures::*;

/// Two bundles, both with a due cast, and a chain endpoint that cannot be
/// reached. Each bundle's delegation proves and signs, then fails at dispatch.
async fn run_two_failing_bundles(isolation: FailureIsolation) -> (RoundRunReport, Vec<String>) {
    let database = database_with_bundles(2);
    let executor = executor_over_unreachable_chain(Arc::clone(&database));
    decide_ballot(&executor);
    let plan = executor.plan().unwrap();
    assert_eq!(
        plan.next_steps[..2],
        [
            NextStep::Delegate { bundle_index: 0 },
            NextStep::Delegate { bundle_index: 1 },
        ],
        "both bundles owe a delegation before anything they hold can be cast"
    );

    let control = ChainSubmissionControl::new(1);
    let events = RecordingReporter::default();
    let report = RoundDriver::new(&executor)
        .with_policy(RoundDrivePolicy {
            failure_isolation: isolation,
            ..RoundDrivePolicy::default()
        })
        .run(
            &SigningHost {
                database: Arc::clone(&database),
            },
            &control,
            &events,
        )
        .await;
    let selected = events
        .events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|event| match event {
            RoundDriveEvent::StepSelected { step } => Some(format!("{step:?}")),
            _ => None,
        })
        .collect();
    (report, selected)
}

#[tokio::test]
async fn a_failed_bundle_is_skipped_and_the_rest_of_the_round_runs() {
    let (report, selected) = run_two_failing_bundles(FailureIsolation::SkipBundle).await;

    assert_eq!(
        selected.len(),
        2,
        "the second bundle is still dispatched after the first fails: {selected:?}"
    );
    assert_eq!(report.failures.len(), 2, "{:?}", report.failures);
    assert_eq!(
        report.failures[0].bundle_index,
        Some(0),
        "a failure names the bundle it isolated"
    );
    assert_eq!(report.failures[1].bundle_index, Some(1));
    assert_eq!(report.skipped_bundles, vec![0, 1]);
    assert!(
        matches!(report.quiescence, RoundQuiescence::Failures),
        "{:?}",
        report.quiescence
    );
}

#[tokio::test]
async fn stop_round_ends_at_the_first_failure() {
    let (report, selected) = run_two_failing_bundles(FailureIsolation::StopRound).await;

    assert_eq!(
        selected.len(),
        1,
        "nothing runs after the first failure: {selected:?}"
    );
    assert_eq!(report.failures.len(), 1);
    assert!(
        report.skipped_bundles.is_empty(),
        "stopping the round isolates nothing"
    );
    // The record still names the bundle the failure came from. That field is
    // attribution, not isolation: a host reading it as "suppressed for the
    // rest of the run" would be wrong here, which is why `skipped_bundles`
    // above is the authoritative list.
    assert!(
        report.failures[0].bundle_index.is_some(),
        "a bundle-attributable failure names its bundle under either policy"
    );
    assert!(matches!(report.quiescence, RoundQuiescence::Failures));
}

#[tokio::test]
async fn a_skipped_bundle_is_reported_as_it_happens() {
    let database = database_with_bundles(2);
    let executor = executor_over_unreachable_chain(Arc::clone(&database));
    decide_ballot(&executor);
    let control = ChainSubmissionControl::new(1);
    let events = RecordingReporter::default();
    let _ = RoundDriver::new(&executor)
        .run(
            &SigningHost {
                database: Arc::clone(&database),
            },
            &control,
            &events,
        )
        .await;

    let skipped: Vec<u32> = events
        .events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|event| match event {
            RoundDriveEvent::BundleSkipped { bundle_index, .. } => Some(*bundle_index),
            _ => None,
        })
        .collect();
    assert_eq!(skipped, vec![0, 1]);
}
