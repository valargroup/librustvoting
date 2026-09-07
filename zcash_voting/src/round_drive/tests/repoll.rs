//! The wait between two episodes of a still-tracking submission.

use super::fixtures::*;

use crate::round_drive::run_loop::sleep_until_interrupted;

#[tokio::test(start_paused = true)]
async fn the_repoll_wait_runs_to_completion_when_nothing_interrupts() {
    let control = ChainSubmissionControl::new(1);
    assert!(sleep_until_interrupted(Duration::from_secs(2), &control, 1).await);
}

#[tokio::test(start_paused = true)]
async fn a_cancelled_host_does_not_pay_the_rest_of_the_repoll_wait() {
    // A host that closes the session mid-wait must not be held for the
    // remainder of it: the wait is polled, not slept through.
    let control = ChainSubmissionControl::new(1);
    control.cancel();
    assert!(!sleep_until_interrupted(Duration::from_secs(3600), &control, 1).await);
}

#[tokio::test(start_paused = true)]
async fn a_new_operation_epoch_ends_the_repoll_wait() {
    let control = ChainSubmissionControl::new(1);
    control.set_operation_epoch(2);
    assert!(!sleep_until_interrupted(Duration::from_secs(3600), &control, 1).await);
}

#[tokio::test(start_paused = true)]
async fn a_tracking_submission_is_polled_again_after_the_repoll_wait() {
    // The first poll finds nothing, so the episode ends `Pending` while the
    // submission is still tracking. The driver waits and polls the same step
    // again rather than treating "not confirmed yet" as the end of the run.
    let database = database_with_imported_delegation();
    let chain = Arc::new(ScriptedChain::default());
    chain.queue_not_found();
    chain.queue_confirmed();
    let executor = executor_over_chain(Arc::clone(&database), Arc::clone(&chain));
    assert_eq!(
        executor.plan().unwrap().next_steps,
        vec![NextStep::AdvanceImportedDelegation { bundle_index: 0 }]
    );

    let control = ChainSubmissionControl::new(1);
    let events = RecordingReporter::default();
    let report = RoundDriver::new(&executor)
        .run(&SinglePassHost, &control, &events)
        .await;

    assert_eq!(
        *chain.gets.lock().unwrap(),
        2,
        "one poll per episode, and the driver ran a second episode"
    );
    let events = events.events.lock().unwrap();
    let waits: Vec<&RoundDriveEvent> = events
        .iter()
        .filter(|event| matches!(event, RoundDriveEvent::AwaitingRepoll { .. }))
        .collect();
    assert_eq!(waits.len(), 1, "{events:?}");
    let RoundDriveEvent::AwaitingRepoll { step, delay } = waits[0] else {
        unreachable!()
    };
    assert_eq!(
        step,
        &NextStep::AdvanceImportedDelegation { bundle_index: 0 }
    );
    assert_eq!(*delay, RoundDrivePolicy::default().pending_repoll);
    // The delegation confirmed, so nothing chain-side is owed. Nothing was
    // voted on, so the run ends at the ballot.
    assert!(
        matches!(report.quiescence, RoundQuiescence::NeedsBallot { .. }),
        "{:?}",
        report.quiescence
    );
    assert!(report.failures.is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_final_allowed_dispatch_refreshes_before_deciding_quiescence() {
    let database = database_with_imported_delegation();
    let chain = Arc::new(ScriptedChain::default());
    chain.queue_confirmed();
    let executor = executor_over_chain(Arc::clone(&database), Arc::clone(&chain));
    let control = ChainSubmissionControl::new(1);
    let events = RecordingReporter::default();
    let report = RoundDriver::new(&executor)
        .with_policy(RoundDrivePolicy {
            max_dispatches: 1,
            ..RoundDrivePolicy::default()
        })
        .run(&SinglePassHost, &control, &events)
        .await;

    assert!(matches!(
        report.quiescence,
        RoundQuiescence::NeedsBallot { .. }
    ));
    assert!(
        report.plan.as_ref().unwrap().next_steps.is_empty(),
        "the report carries the plan after the successful final dispatch"
    );
    assert_eq!(
        events
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| matches!(event, RoundDriveEvent::PlanRefreshed { .. }))
            .count(),
        2
    );
}

#[tokio::test(start_paused = true)]
async fn confirmed_chain_work_pending_on_helpers_is_replanned_not_stalled() {
    let database = database_with_imported_delegation();
    let chain = Arc::new(ScriptedChain::default());
    chain.queue_confirmed();
    let executor = executor_over_chain(database, chain);
    let control = ChainSubmissionControl::new(1);
    let step = NextStep::AdvanceImportedDelegation { bundle_index: 0 };
    let mut outcome = executor
        .advance_step(
            step.clone(),
            &SinglePassHost.host_context(),
            &control,
            &crate::NoopRoundStepProgressReporter {},
        )
        .await
        .unwrap();
    assert!(matches!(
        outcome.chain_outcome,
        Some(crate::ChainSubmissionResult::Confirmed(_))
    ));

    // Vote completion uses this combination after chain confirmation when
    // helper delivery has only ambiguous attempts left to track.
    outcome.disposition = crate::RoundStepDisposition::Pending;
    let delay = Duration::from_secs(2);
    let mut run = crate::round_drive::run_ledger::Run::default();
    let quiescence = run.record_outcome(&step, outcome, delay, &RecordingReporter::default());

    assert!(
        quiescence.is_none(),
        "confirmed chain work is not stalled recovery"
    );
    assert_eq!(run.repoll, vec![(step, delay)]);
}

#[tokio::test(start_paused = true)]
async fn a_rejected_submission_stops_the_run_carrying_its_diagnostic() {
    // A terminal chain result plans no retry and carries no vote diagnostic of
    // its own, so the outcome the run reports is the only place the rejection
    // survives. One dispatch per run, so the queue is consumed exactly.
    let database = database_with_imported_delegation();
    let chain = Arc::new(ScriptedChain::default());
    let executor = executor_over_chain(Arc::clone(&database), Arc::clone(&chain));
    let one_dispatch = RoundDrivePolicy {
        max_dispatches: 1,
        ..RoundDrivePolicy::default()
    };
    let control = ChainSubmissionControl::new(1);

    // First run: the poll finds nothing, which creates the tracking row.
    chain.queue_not_found();
    let report = RoundDriver::new(&executor)
        .with_policy(one_dispatch.clone())
        .run(&SinglePassHost, &control, &RecordingReporter::default())
        .await;
    assert!(matches!(
        report.quiescence,
        RoundQuiescence::PassBudgetExhausted { .. }
    ));

    // Recovery is where a failed import becomes terminal rather than a
    // hashless retry, so put the row there before the next poll answers.
    database
        .conn()
        .execute(
            "UPDATE chain_submissions
             SET state = 'recovering', diagnostic_kind = 'tracking_window_expired',
                 diagnostic = 'tracking expired'",
            [],
        )
        .unwrap();

    chain.queue_rejected();
    let report = RoundDriver::new(&executor)
        .with_policy(one_dispatch)
        .run(&SinglePassHost, &control, &RecordingReporter::default())
        .await;

    let RoundQuiescence::ChainTerminal { step, outcome } = report.quiescence else {
        panic!("a rejection stops the run: {:?}", report.quiescence);
    };
    assert_eq!(
        step,
        NextStep::AdvanceImportedDelegation { bundle_index: 0 }
    );
    assert!(
        matches!(outcome, crate::ChainSubmissionResult::Rejected(_)),
        "{outcome:?}"
    );
    assert_eq!(
        report.chain_outcomes.len(),
        1,
        "the terminal outcome is also kept in the report"
    );
    // The wave persisted the rejection and then ended the run. A report whose
    // plan was read before the wave would still list the step that produced
    // the rejection and show the bundle as non-terminal, describing the round
    // as it was rather than as this run left it.
    let stopped_plan = report.plan.as_ref().expect("a run always reports a plan");
    assert!(
        stopped_plan.next_steps.is_empty(),
        "the rejected submission plans no retry: {:?}",
        stopped_plan.next_steps
    );
    assert!(stopped_plan.blocking_recovery);
    assert!(stopped_plan.delegation_statuses[0].terminal);

    let restarted = RoundDriver::new(&executor)
        .with_policy(RoundDrivePolicy {
            max_dispatches: 1,
            ..RoundDrivePolicy::default()
        })
        .run(&SinglePassHost, &control, &RecordingReporter::default())
        .await;
    assert!(matches!(
        restarted.quiescence,
        RoundQuiescence::PersistedChainTerminal
    ));
    assert!(restarted.plan.as_ref().unwrap().blocking_recovery);
    assert!(restarted.plan.as_ref().unwrap().next_steps.is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_persisted_hashless_submission_requires_manual_handling() {
    let database = database_with_imported_delegation();
    let chain = Arc::new(ScriptedChain::default());
    chain.queue_not_found();
    let executor = executor_over_chain(Arc::clone(&database), chain);
    let control = ChainSubmissionControl::new(1);
    let policy = RoundDrivePolicy {
        max_dispatches: 1,
        ..RoundDrivePolicy::default()
    };
    let _ = RoundDriver::new(&executor)
        .with_policy(policy.clone())
        .run(&SinglePassHost, &control, &RecordingReporter::default())
        .await;
    database
        .conn()
        .execute(
            "UPDATE chain_submissions
             SET state = 'submitted_without_hash',
                 candidate_transaction_hash = NULL,
                 confirmed_transaction_hash = NULL,
                 final_van_position = NULL,
                 vote_commitment_positions = NULL,
                 diagnostic_kind = 'ambiguous_attempts_exhausted',
                 diagnostic = 'dispatch outcome remains unknown'",
            [],
        )
        .unwrap();

    let report = RoundDriver::new(&executor)
        .with_policy(policy)
        .run(&SinglePassHost, &control, &RecordingReporter::default())
        .await;
    assert!(matches!(
        report.quiescence,
        RoundQuiescence::PersistedChainTerminal
    ));
    let status = &report.plan.as_ref().unwrap().delegation_statuses[0];
    assert!(status.terminal);
    assert!(status.submission_diagnostic.is_some());
}

#[tokio::test(start_paused = true)]
async fn a_submission_that_never_confirms_stops_at_the_dispatch_budget() {
    // The wait between episodes is the one place a run could poll forever.
    // The budget counts every dispatch, re-polls included, so a chain that
    // never answers ends the run instead of holding it open.
    let database = database_with_imported_delegation();
    let chain = Arc::new(ScriptedChain::default());
    for _ in 0..3 {
        chain.queue_not_found();
    }
    let executor = executor_over_chain(Arc::clone(&database), Arc::clone(&chain));
    let control = ChainSubmissionControl::new(1);
    let events = RecordingReporter::default();
    let report = RoundDriver::new(&executor)
        .with_policy(RoundDrivePolicy {
            max_dispatches: 3,
            ..RoundDrivePolicy::default()
        })
        .run(&SinglePassHost, &control, &events)
        .await;

    assert_eq!(*chain.gets.lock().unwrap(), 3, "one poll per dispatch");
    let RoundQuiescence::PassBudgetExhausted { remaining } = report.quiescence else {
        panic!("a stuck submission stops: {:?}", report.quiescence);
    };
    assert_eq!(
        remaining,
        vec![NextStep::AdvanceImportedDelegation { bundle_index: 0 }],
        "the run names the work it left behind"
    );
    assert!(
        report.failures.is_empty(),
        "nothing failed; it did not finish"
    );
    assert_eq!(
        report.plan.as_ref().unwrap().next_steps,
        remaining,
        "budget exhaustion and the report use the same fresh plan"
    );
    let events = events.events.lock().unwrap();
    let final_plan = events.iter().rev().find_map(|event| match event {
        RoundDriveEvent::PlanRefreshed { plan, tally } => Some((plan, tally)),
        _ => None,
    });
    let (final_plan, final_tally) = final_plan.expect("a final refreshed plan");
    assert_eq!(final_plan.as_ref(), report.plan.as_ref().unwrap());
    assert_eq!(*final_tally, report.tally);
}

#[tokio::test(start_paused = true)]
async fn a_submission_stuck_in_recovery_stops_instead_of_being_polled_forever() {
    // Recovery has already escalated to the exact tree, so another poll is
    // not what resolves this. Re-polling it for the rest of the round would
    // hide a submission the host can retry later; the run stops and names it.
    let database = database_with_imported_delegation();
    let chain = Arc::new(ScriptedChain::default());
    let executor = executor_over_chain(Arc::clone(&database), Arc::clone(&chain));
    let one_dispatch = RoundDrivePolicy {
        max_dispatches: 1,
        ..RoundDrivePolicy::default()
    };
    let control = ChainSubmissionControl::new(1);

    chain.queue_not_found();
    let _ = RoundDriver::new(&executor)
        .with_policy(one_dispatch.clone())
        .run(&SinglePassHost, &control, &RecordingReporter::default())
        .await;
    database
        .conn()
        .execute(
            "UPDATE chain_submissions
             SET state = 'recovering', diagnostic_kind = 'tracking_window_expired',
                 diagnostic = 'tracking expired'",
            [],
        )
        .unwrap();

    chain.queue_not_found();
    let report = RoundDriver::new(&executor)
        .with_policy(one_dispatch)
        .run(&SinglePassHost, &control, &RecordingReporter::default())
        .await;

    let RoundQuiescence::ChainRecoveryStalled { step, outcome } = report.quiescence else {
        panic!("a stalled recovery stops the run: {:?}", report.quiescence);
    };
    assert_eq!(
        step,
        NextStep::AdvanceImportedDelegation { bundle_index: 0 }
    );
    assert!(
        matches!(
            outcome,
            crate::ChainSubmissionResult::Pending(crate::ChainSubmissionPending::Recovering { .. })
        ),
        "{outcome:?}"
    );
}
