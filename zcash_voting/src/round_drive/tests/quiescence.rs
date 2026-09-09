//! Why a run stops: nothing owed, an open ballot, cancellation, and the
//! dispatch budget.

use super::fixtures::*;

#[tokio::test]
async fn a_round_the_voter_has_not_decided_stops_for_the_ballot() {
    // The plan is empty because nothing can be planned yet, not because the
    // round is finished. Reporting `NoWorkLeft` here would tell a host the
    // round was done before the voter had chosen anything.
    let executor = executor();
    let control = ChainSubmissionControl::new(1);
    let (report, events) = drive(&executor, &control).await;

    let RoundQuiescence::NeedsBallot { open_proposals, .. } = report.quiescence else {
        panic!(
            "an undecided round stops for the ballot: {:?}",
            report.quiescence
        );
    };
    assert_eq!(open_proposals, vec![1, 2]);
    assert!(report.failures.is_empty());
    assert_eq!(report.tally, RoundWorkTally::default());
    let events = events.events.lock().unwrap();
    assert!(
        events
            .iter()
            .all(|event| matches!(event, RoundDriveEvent::PlanRefreshed { .. })),
        "nothing is dispatched for an empty plan: {events:?}"
    );
}

#[tokio::test]
async fn a_fully_skipped_ballot_stops_with_no_work_left() {
    // Every proposal has a terminal decision and none of them is a choice, so
    // the round genuinely owes nothing.
    let executor = executor();
    executor
        .set_ballot_intents(&[
            BallotIntent {
                proposal_id: 1,
                decision: Decision::Skipped,
            },
            BallotIntent {
                proposal_id: 2,
                decision: Decision::Skipped,
            },
        ])
        .unwrap();
    let control = ChainSubmissionControl::new(1);
    let (report, _) = drive(&executor, &control).await;

    assert!(
        matches!(report.quiescence, RoundQuiescence::NoWorkLeft),
        "{:?}",
        report.quiescence
    );
    assert_eq!(report.tally.total_proposals, 0);
}

#[tokio::test]
async fn a_choice_without_bundles_stops_for_bundle_setup() {
    let executor = executor_over(database_without_bundles());
    executor
        .set_ballot_intents(&[BallotIntent {
            proposal_id: 1,
            decision: Decision::Choice(0),
        }])
        .unwrap();
    let control = ChainSubmissionControl::new(1);
    let (report, events) = drive(&executor, &control).await;

    assert!(matches!(
        report.quiescence,
        RoundQuiescence::NeedsBundleSetup
    ));
    let plan = report.plan.expect("the remedy-bearing plan is retained");
    assert!(plan.needs_bundle_setup);
    assert!(plan.next_steps.is_empty());
    assert!(!events
        .events
        .lock()
        .unwrap()
        .iter()
        .any(|event| matches!(event, RoundDriveEvent::StepSelected { .. })));
}

#[tokio::test]
async fn a_partly_decided_ballot_still_runs_the_delegation_prerequisite() {
    // An incomplete ballot may prepare its delegation proof. The host's
    // driver runs once, then the round waits for the remaining decision;
    // neither signing nor a chain reservation is needed at this boundary.
    let executor = executor();
    executor
        .set_ballot_intents(&[BallotIntent {
            proposal_id: 1,
            decision: Decision::Choice(0),
        }])
        .unwrap();
    let plan = executor.plan().unwrap();
    assert_eq!(plan.open_proposals, vec![2]);
    assert_eq!(
        plan.next_steps,
        vec![NextStep::Delegate { bundle_index: 0 }]
    );

    let control = ChainSubmissionControl::new(1);
    let report = RoundDriver::new(&executor)
        .run(
            &SigningHost {
                database: executor.database(),
            },
            &control,
            &RecordingReporter::default(),
        )
        .await;
    assert!(
        matches!(report.quiescence, RoundQuiescence::NeedsBallot { .. }),
        "{:?}",
        report.quiescence
    );
    assert!(report.failures.is_empty());
    assert_eq!(
        executor.database().delegation_phase(ROUND_ID, 0).unwrap(),
        crate::phases::DelegationPhase::Proved
    );
    assert_eq!(
        executor
            .database()
            .conn()
            .query_row("SELECT count(*) FROM chain_submissions", [], |row| row
                .get::<_, u32>(0))
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn a_cancelled_control_stops_before_the_first_plan() {
    let executor = executor();
    let control = ChainSubmissionControl::new(1);
    control.cancel();
    let (report, events) = drive(&executor, &control).await;

    assert!(matches!(report.quiescence, RoundQuiescence::Cancelled));
    assert!(report.plan.is_none(), "no plan was read");
    assert!(events.events.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_delegation_step_without_signing_material_stops_naming_its_bundles() {
    // Both proposals decided, so the bundle's cast is due and its delegation
    // becomes the plan's first step. The fixture host carries no
    // `DelegationStepInputs`, which is what a Keystone wallet looks like
    // before its signatures are scanned.
    let executor = executor();
    executor
        .set_ballot_intents(&[
            BallotIntent {
                proposal_id: 1,
                decision: Decision::Choice(0),
            },
            BallotIntent {
                proposal_id: 2,
                decision: Decision::Choice(1),
            },
        ])
        .unwrap();
    let control = ChainSubmissionControl::new(1);
    let (report, events) = drive(&executor, &control).await;

    let RoundQuiescence::NeedsDelegationSignatures { bundles } = report.quiescence else {
        panic!(
            "a delegation with no signer stops for the host: {:?}",
            report.quiescence
        );
    };
    assert_eq!(bundles, vec![0]);
    let events = events.events.lock().unwrap();
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, RoundDriveEvent::StepSelected { .. })),
        "nothing is dispatched before the signature exists: {events:?}"
    );
}

#[tokio::test]
async fn the_dispatch_budget_stops_a_plan_that_never_shrinks() {
    let executor = executor();
    executor
        .set_ballot_intents(&[
            BallotIntent {
                proposal_id: 1,
                decision: Decision::Choice(0),
            },
            BallotIntent {
                proposal_id: 2,
                decision: Decision::Choice(1),
            },
        ])
        .unwrap();
    // A zero budget stops before the first dispatch, which is the only way to
    // observe the guard without a plan that genuinely cannot make progress.
    let policy = RoundDrivePolicy {
        max_dispatches: 0,
        ..RoundDrivePolicy::default()
    };
    let control = ChainSubmissionControl::new(1);
    let events = RecordingReporter::default();
    let report = RoundDriver::new(&executor)
        .with_policy(policy)
        .run(&FixedHost, &control, &events)
        .await;

    let RoundQuiescence::PassBudgetExhausted { remaining } = report.quiescence else {
        panic!(
            "zero budget stops with current work: {:?}",
            report.quiescence
        );
    };
    let plan = report.plan.as_ref().expect("zero budget still plans");
    assert_eq!(remaining, plan.next_steps);
    assert_eq!(report.tally.total_proposals, 2);
    let events = events.events.lock().unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, RoundDriveEvent::PlanRefreshed { .. }))
            .count(),
        1
    );
    assert!(!events
        .iter()
        .any(|event| matches!(event, RoundDriveEvent::StepSelected { .. })));
}

#[test]
fn the_default_policy_is_the_cadence_hosts_were_driving_by_hand() {
    // These are the constants a host loop used before the driver existed.
    // Changing one changes every host's pacing, so it is pinned here.
    let policy = RoundDrivePolicy::default();
    assert_eq!(policy.pending_repoll, Duration::from_secs(2));
    assert_eq!(policy.max_bundle_concurrency.get(), 5);
    assert_eq!(policy.failure_isolation, FailureIsolation::SkipBundle);
}

#[tokio::test]
async fn a_share_a_helper_already_holds_is_left_to_background_tracking() {
    // The share is submitted and accepted, so only polling can finish it and
    // the host's own timer owns that. A foreground run that polled it here
    // would hold the vote flow open for work that does not block it.
    let helpers = vec!["http://helper.invalid".to_string()];
    let database = crate::share_tracking::tests::db_with_share(&helpers);
    // The round's vote owes two shares; the fixture delivers the first. Record
    // the second as accepted too, so nothing is left that a helper has not
    // already taken.
    crate::share::record_delivery(
        &database,
        &crate::share::ShareDeliveryRecordParams {
            round_id: ROUND_ID,
            bundle_index: 0,
            proposal_id: 1,
            share_index: 1,
            submission: &crate::share_tracking::ShareSubmissionReport {
                accepted_urls: helpers.clone(),
                ambiguous_urls: Vec::new(),
                target_count: helpers.len(),
            },
            submit_at: 1_700_000_000,
        },
    )
    .unwrap();
    database
        .conn()
        .execute(
            "UPDATE votes SET tx_hash = 'aa' WHERE round_id = :round_id
               AND wallet_id = :wallet_id AND bundle_index = 0 AND proposal_id = 1",
            rusqlite::named_params! {
                ":round_id": ROUND_ID,
                ":wallet_id": database.wallet_id(),
            },
        )
        .unwrap();
    let executor = executor_over_share_round(Arc::new(database));
    let plan = executor.plan().unwrap();
    assert!(!plan.blocking_share_work, "a helper accepted the share");
    assert_eq!(
        plan.next_steps,
        vec![
            NextStep::ConfirmShare {
                bundle_index: 0,
                proposal_id: 1,
                share_index: 0,
            },
            NextStep::ConfirmShare {
                bundle_index: 0,
                proposal_id: 1,
                share_index: 1,
            },
        ]
    );

    let control = ChainSubmissionControl::new(1);
    let (report, events) = drive(&executor, &control).await;

    let RoundQuiescence::BackgroundShareWorkOnly { shares } = report.quiescence else {
        panic!(
            "non-blocking share work is handed back: {:?}",
            report.quiescence
        );
    };
    assert_eq!(shares.len(), 2);
    assert_eq!(shares[0].bundle_index, 0);
    assert!(
        !events
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|event| matches!(event, RoundDriveEvent::StepSelected { .. })),
        "the run polls nothing itself"
    );
}

#[tokio::test]
async fn an_accepted_share_never_outranks_the_delivery_the_round_owes() {
    // Plan order puts the accepted share first, and only the foreground can
    // deliver the one no helper reached. Leaving the accepted share in the
    // dispatch stream let the run poll it, and a re-poll promotes the step it
    // named, so the undelivered share was never submitted at all.
    let helpers = vec!["http://helper.invalid".to_string()];
    let database = crate::share_tracking::tests::db_with_share(&helpers);
    // Share 1 was attempted and reached nobody: it owes delivery, not polling.
    crate::share::record_delivery(
        &database,
        &crate::share::ShareDeliveryRecordParams {
            round_id: ROUND_ID,
            bundle_index: 0,
            proposal_id: 1,
            share_index: 1,
            submission: &crate::share_tracking::ShareSubmissionReport {
                accepted_urls: Vec::new(),
                ambiguous_urls: Vec::new(),
                target_count: helpers.len(),
            },
            submit_at: 1_700_000_000,
        },
    )
    .unwrap();
    database
        .conn()
        .execute(
            "UPDATE votes SET tx_hash = 'aa' WHERE round_id = :round_id
               AND wallet_id = :wallet_id AND bundle_index = 0 AND proposal_id = 1",
            rusqlite::named_params! {
                ":round_id": ROUND_ID,
                ":wallet_id": database.wallet_id(),
            },
        )
        .unwrap();
    let executor = executor_over_share_round(Arc::new(database));
    let classified = executor.plan_classified().unwrap();

    let accepted = NextStep::ConfirmShare {
        bundle_index: 0,
        proposal_id: 1,
        share_index: 0,
    };
    let undelivered = NextStep::ConfirmShare {
        bundle_index: 0,
        proposal_id: 1,
        share_index: 1,
    };
    assert_eq!(
        classified.plan.next_steps,
        vec![accepted.clone(), undelivered.clone()],
        "the accepted share sorts ahead of the one that owes delivery"
    );
    assert!(
        classified.plan.blocking_share_work,
        "share 1 has reached no helper"
    );

    let control = ChainSubmissionControl::new(1);
    let (_report, events) = drive(&executor, &control).await;

    let selected: Vec<_> = events
        .events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|event| match event {
            RoundDriveEvent::StepSelected { step } => Some(step.clone()),
            _ => None,
        })
        .collect();
    assert!(
        selected.contains(&undelivered),
        "the run owes this delivery: {selected:?}"
    );
    assert!(
        !selected.contains(&accepted),
        "the accepted share is the timer's, not this run's: {selected:?}"
    );
}

#[tokio::test]
async fn an_outcome_unknown_share_never_outranks_the_delivery_the_round_owes() {
    // Share 0 may already be held by a helper, so only duplicate-safe
    // background tracking may reconcile or replenish it. If the foreground
    // polls it, the Pending result promotes it ahead of plan order forever and
    // share 1 never gets the delivery attempt it can safely make.
    let helpers = vec!["http://helper.invalid".to_string()];
    let database = crate::share_tracking::tests::db_with_share(&[]);
    database
        .conn()
        .execute(
            "UPDATE share_delegations
             SET ambiguous_urls = '[\"http://helper.invalid\"]'
             WHERE round_id = :round_id AND wallet_id = :wallet_id
               AND bundle_index = 0 AND proposal_id = 1 AND share_index = 0",
            rusqlite::named_params! {
                ":round_id": ROUND_ID,
                ":wallet_id": database.wallet_id(),
            },
        )
        .unwrap();
    crate::share::record_delivery(
        &database,
        &crate::share::ShareDeliveryRecordParams {
            round_id: ROUND_ID,
            bundle_index: 0,
            proposal_id: 1,
            share_index: 1,
            submission: &crate::share_tracking::ShareSubmissionReport {
                accepted_urls: Vec::new(),
                ambiguous_urls: Vec::new(),
                target_count: helpers.len(),
            },
            submit_at: 1_700_000_000,
        },
    )
    .unwrap();
    database
        .conn()
        .execute(
            "UPDATE votes SET tx_hash = 'aa' WHERE round_id = :round_id
               AND wallet_id = :wallet_id AND bundle_index = 0 AND proposal_id = 1",
            rusqlite::named_params! {
                ":round_id": ROUND_ID,
                ":wallet_id": database.wallet_id(),
            },
        )
        .unwrap();

    let helper = Arc::new(AcceptingHelper::default());
    let executor = executor_over_accepting_helper(Arc::new(database), Arc::clone(&helper));
    let outcome_unknown = NextStep::ConfirmShare {
        bundle_index: 0,
        proposal_id: 1,
        share_index: 0,
    };
    let undelivered = NextStep::ConfirmShare {
        bundle_index: 0,
        proposal_id: 1,
        share_index: 1,
    };
    assert_eq!(
        executor.plan().unwrap().next_steps,
        vec![outcome_unknown.clone(), undelivered.clone()],
        "the tracking-owned share sorts ahead of the deliverable one"
    );

    let control = ChainSubmissionControl::new(1);
    let (report, events) = drive(&executor, &control).await;
    let selected: Vec<_> = events
        .events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|event| match event {
            RoundDriveEvent::StepSelected { step } => Some(step.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        selected,
        vec![undelivered],
        "only the share that can make foreground progress is dispatched"
    );
    assert_eq!(*helper.posts.lock().unwrap(), 1);

    let RoundQuiescence::BackgroundShareWorkOnly { shares } = report.quiescence else {
        panic!(
            "the outcome-unknown share is handed to tracking: {:?}",
            report.quiescence
        );
    };
    assert!(
        shares.iter().any(|share| share.share_index == 0),
        "the handoff names the unresolved outcome-unknown share: {shares:?}"
    );
    let plan = report.plan.expect("the handoff retains its plan");
    assert!(!plan.blocking_recovery);
    assert!(!plan.blocking_share_work);
}
