//! The wait between two episodes of a still-tracking submission.

use super::fixtures::*;

use crate::round_drive::run_loop::{repoll_delay, sleep_until_interrupted};

#[tokio::test(start_paused = true)]
async fn a_deadline_expiring_after_selection_wakes_the_idle_driver() {
    let selected_at = tokio::time::Instant::now();
    let deadline = selected_at + Duration::from_secs(2);
    let deadlines = [(
        NextStep::AdvanceImportedDelegation { bundle_index: 0 },
        Some(deadline),
    )];
    // Selection excludes this obligation while its deadline is still future.
    // The task can then be descheduled through the deadline before waiting.
    assert!(deadline > selected_at);
    tokio::time::advance(Duration::from_secs(3)).await;
    let delay = repoll_delay(&deadlines, &Default::default(), true);
    let control = ChainSubmissionControl::new(1);
    assert_eq!(
        delay,
        Duration::ZERO,
        "overdue work must wake an idle driver"
    );
    assert!(tokio::time::timeout(
        Duration::from_millis(1),
        sleep_until_interrupted(delay, &control, 1),
    )
    .await
    .expect("replanning must not need host cancellation"));
}

#[tokio::test(start_paused = true)]
async fn repoll_wakes_at_the_earliest_deadline_including_exact_expiry() {
    let now = tokio::time::Instant::now();
    let deadlines = [
        (
            NextStep::AdvanceImportedDelegation { bundle_index: 0 },
            Some(now + Duration::from_secs(5)),
        ),
        (
            NextStep::AdvanceImportedDelegation { bundle_index: 1 },
            Some(now + Duration::from_secs(2)),
        ),
    ];
    assert_eq!(
        repoll_delay(&deadlines, &Default::default(), true),
        Duration::from_secs(2)
    );
    tokio::time::advance(Duration::from_secs(2)).await;
    assert_eq!(
        repoll_delay(&deadlines, &Default::default(), true),
        Duration::ZERO
    );
}

#[tokio::test(start_paused = true)]
async fn unavailable_repolls_wait_for_completion_or_interruption() {
    let now = tokio::time::Instant::now();
    let deadlines = [(
        NextStep::AdvanceImportedDelegation { bundle_index: 0 },
        Some(now),
    )];
    // Full capacity or an exhausted dispatch budget must not spin on due work.
    assert_eq!(
        repoll_delay(&deadlines, &Default::default(), false),
        Duration::MAX
    );
    assert_eq!(repoll_delay(&deadlines, &[0].into(), true), Duration::MAX);
    assert_eq!(repoll_delay(&[], &Default::default(), true), Duration::MAX);
    let unbounded = [(
        NextStep::AdvanceImportedDelegation { bundle_index: 0 },
        None,
    )];
    assert_eq!(
        repoll_delay(&unbounded, &Default::default(), true),
        Duration::MAX
    );
    let control = ChainSubmissionControl::new(1);
    control.cancel();
    assert!(
        !sleep_until_interrupted(
            repoll_delay(&deadlines, &Default::default(), false),
            &control,
            1
        )
        .await
    );
}

#[tokio::test(start_paused = true)]
async fn the_repoll_wait_runs_to_completion_when_nothing_interrupts() {
    let control = ChainSubmissionControl::new(1);
    assert!(sleep_until_interrupted(Duration::from_secs(2), &control, 1).await);
}

#[tokio::test(start_paused = true)]
async fn a_cancelled_host_does_not_pay_the_rest_of_the_repoll_wait() {
    // A host that closes the session mid-wait must not be held for the
    // remainder of it: the wait is woken by the control, not slept through.
    let control = ChainSubmissionControl::new(1);
    control.cancel();
    assert!(!sleep_until_interrupted(Duration::from_secs(3600), &control, 1).await);
}

#[tokio::test(start_paused = true)]
async fn a_cancelled_wait_ends_without_advancing_the_clock() {
    // The mechanism, not just the outcome. A wait that re-read the control on
    // a tick would need the clock to reach that tick before it noticed, and
    // under paused time the runtime supplies exactly that by auto-advancing
    // whenever every task is idle. Being woken by the control instead costs no
    // clock movement at all, which is what makes an hours-long wait free.
    let control = ChainSubmissionControl::new(1);
    let waiting = tokio::spawn({
        let control = control.clone();
        async move { sleep_until_interrupted(Duration::from_secs(86_400), &control, 1).await }
    });
    // Let it register its interest and settle into the wait. Deliberately not
    // a multiple of any poll tick: landing on one would leave a polling
    // implementation already runnable at `at_cancel` and let it pass too.
    tokio::time::sleep(Duration::from_millis(3)).await;
    assert!(!waiting.is_finished());

    let at_cancel = tokio::time::Instant::now();
    control.cancel();

    assert!(!waiting.await.unwrap(), "the wait reports the interruption");
    assert_eq!(
        tokio::time::Instant::now(),
        at_cancel,
        "the wait is woken by the control, not by the clock reaching a poll tick",
    );
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
        .advance_step_in_epoch(
            step.clone(),
            &SinglePassHost.host_context(),
            &control,
            control.operation_epoch(),
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

#[tokio::test(start_paused = true)]
async fn an_exhausted_budget_does_not_pay_a_wait_that_leads_nowhere() {
    // The final allowed dispatch returned `Pending`, so the run wanted to
    // re-poll. A re-poll is a pause before another dispatch, and there is no
    // dispatch left to make: sleeping first would hold the run open for the
    // host's whole interval before the next pass could report the exhaustion.
    let database = database_with_imported_delegation();
    let chain = Arc::new(ScriptedChain::default());
    chain.queue_not_found();
    let executor = executor_over_chain(database, chain);
    let control = ChainSubmissionControl::new(1);

    let started = tokio::time::Instant::now();
    let report = RoundDriver::new(&executor)
        .with_policy(RoundDrivePolicy {
            max_dispatches: 1,
            pending_repoll: Duration::from_secs(3600),
            ..RoundDrivePolicy::default()
        })
        .run(&SinglePassHost, &control, &RecordingReporter::default())
        .await;
    let elapsed = started.elapsed();

    assert!(matches!(
        report.quiescence,
        RoundQuiescence::PassBudgetExhausted { .. }
    ));
    assert!(
        elapsed < Duration::from_secs(3600),
        "the run waited out an interval it could never use: {elapsed:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn an_unbounded_repoll_waits_instead_of_overflowing() {
    // `pending_repoll` is host-configured and unbounded. An absolute deadline
    // that far out is not representable, and panicking on the addition would
    // let a policy value bring down the host process; the wait stays
    // cancellable instead.
    let control = ChainSubmissionControl::new(1);
    let waiting = tokio::spawn({
        let control = control.clone();
        async move { sleep_until_interrupted(Duration::MAX, &control, 1).await }
    });

    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        !waiting.is_finished(),
        "an unbounded wait does not end early"
    );
    control.cancel();

    assert!(
        !waiting.await.unwrap(),
        "the wait reports that it was interrupted"
    );
}
