//! Every observation names the step it came from.

use super::fixtures::*;

#[tokio::test(start_paused = true)]
async fn every_step_observation_names_its_step() {
    // A bare `RoundStepProgress` has no subject: `ChainOutcome` and
    // `TreeSynced` say nothing about which bundle produced them. A host
    // reading one stream while bundles interleave would misattribute them, so
    // the driver wraps each one with the step it came from.
    let database = database_with_imported_delegation();
    let chain = Arc::new(ScriptedChain::default());
    chain.queue_confirmed();
    let executor = executor_over_chain(Arc::clone(&database), Arc::clone(&chain));
    let control = ChainSubmissionControl::new(1);
    let events = RecordingReporter::default();
    let _ = RoundDriver::new(&executor)
        .run(&SinglePassHost, &control, &events)
        .await;

    let expected = NextStep::AdvanceImportedDelegation { bundle_index: 0 };
    let events = events.events.lock().unwrap();
    let progress: Vec<&RoundDriveEvent> = events
        .iter()
        .filter(|event| matches!(event, RoundDriveEvent::StepProgress { .. }))
        .collect();
    assert!(
        !progress.is_empty(),
        "the step reported progress at all: {events:?}"
    );
    for event in progress {
        let RoundDriveEvent::StepProgress { step, .. } = event else {
            unreachable!()
        };
        assert_eq!(step, &expected, "{event:?}");
    }
    assert!(
        events.iter().any(|event| matches!(
            event,
            RoundDriveEvent::StepProgress {
                progress: crate::RoundStepProgress::ChainOutcome(_),
                ..
            }
        )),
        "the subjectless chain outcome is among them: {events:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_run_reports_its_plan_before_it_dispatches_anything() {
    // A host renders from `PlanRefreshed`, so a dispatch it never saw a plan
    // for would leave the first step unattributable in the UI.
    let database = database_with_imported_delegation();
    let chain = Arc::new(ScriptedChain::default());
    chain.queue_confirmed();
    let executor = executor_over_chain(Arc::clone(&database), Arc::clone(&chain));
    let control = ChainSubmissionControl::new(1);
    let events = RecordingReporter::default();
    let _ = RoundDriver::new(&executor)
        .run(&SinglePassHost, &control, &events)
        .await;

    let events = events.events.lock().unwrap();
    let first_plan = events
        .iter()
        .position(|event| matches!(event, RoundDriveEvent::PlanRefreshed { .. }));
    let first_dispatch = events
        .iter()
        .position(|event| matches!(event, RoundDriveEvent::StepSelected { .. }));
    assert_eq!(first_plan, Some(0));
    assert!(first_dispatch > first_plan, "{events:?}");
}
