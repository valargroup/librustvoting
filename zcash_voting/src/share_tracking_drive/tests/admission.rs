//! One run at a time per round.

use super::fixtures::*;

/// A driver whose share is not due for a day, so a started run settles into a
/// wait and stays there for the rest of the test.
fn waiting_policy() -> ShareTrackingDrivePolicy {
    ShareTrackingDrivePolicy {
        timing: crate::share_policy::ShareTimingPolicy {
            future_check_max_delay_seconds: 60 * 60 * 24,
            ..ShareTrackingDrivePolicy::default().timing
        },
        ..ShareTrackingDrivePolicy::default()
    }
}

#[tokio::test(start_paused = true)]
async fn a_second_run_for_the_same_round_is_turned_away_without_polling() {
    // Two lifecycle callbacks can start a run for one round. The pass's
    // per-share locks keep them off the same share, but not off the same
    // round: interleaved runs re-poll shares the other has just answered, and
    // a pass is meant to plan from the complete previous pass.
    let db = db_with_pending_share(60);
    let host = ScriptedHost::fixed(Some(VOTE_END));
    let control = ChainSubmissionControl::new(1);
    let client = client();
    let holder_events = RecordingReporter::default();
    let holder = ShareTrackingDriver::new(&db, &client, ROUND_ID).with_policy(waiting_policy());

    let holding_run = holder.run(&host, &control, &holder_events);
    tokio::pin!(holding_run);
    assert!(
        tokio::time::timeout(Duration::from_millis(1), &mut holding_run)
            .await
            .is_err(),
        "the first run should be waiting for its share to come due"
    );

    let second_host = ScriptedHost::fixed(Some(VOTE_END));
    let second_events = RecordingReporter::default();
    let second = ShareTrackingDriver::new(&db, &client, ROUND_ID).with_policy(waiting_policy());
    let report = second.run(&second_host, &control, &second_events).await;

    assert_eq!(report.quiescence, ShareTrackingQuiescence::AlreadyDriving);
    assert_eq!(report.passes, 0);
    assert_eq!(second_events.passes_started(), 0);
    assert_eq!(
        *second_host.reads.lock().unwrap(),
        0,
        "a run that is turned away reads nothing and touches no share",
    );

    control.cancel();
    let holder_report = holding_run.await;
    assert_eq!(
        holder_report.quiescence,
        ShareTrackingQuiescence::Cancelled,
        "the run that held the round is unaffected",
    );
}

#[tokio::test(start_paused = true)]
async fn a_finished_run_releases_its_round_for_the_next_one() {
    // Admission is held for the length of a run, not for the life of the
    // process. A host that starts a run per lifecycle event would otherwise be
    // locked out of a round after the first one ended.
    let db = db_with_pending_share(60);
    let host = ScriptedHost::fixed(Some(VOTE_END));
    let control = ChainSubmissionControl::new(1);
    let policy = ShareTrackingDrivePolicy {
        max_passes: Some(1),
        ..ShareTrackingDrivePolicy::default()
    };

    let (first, _) = drive(&db, &host, &control, policy.clone()).await;
    let (second, _) = drive(&db, &host, &control, policy).await;

    for report in [&first, &second] {
        assert!(
            matches!(
                report.quiescence,
                ShareTrackingQuiescence::PassBudgetExhausted { .. }
            ),
            "each run reaches its own budget rather than the other's admission",
        );
        assert_eq!(report.passes, 1);
    }
}

#[tokio::test(start_paused = true)]
async fn another_round_in_the_same_wallet_runs_alongside() {
    // The guard is per round, not per wallet: a wallet voting in two rounds
    // must be able to drive both at once.
    let db = db_with_pending_share(60);
    seed_round_with_pending_share(&db, OTHER_ROUND_ID, 60);
    let control = ChainSubmissionControl::new(1);
    let client = client();
    let host = ScriptedHost::fixed(Some(VOTE_END));
    let held_events = RecordingReporter::default();
    let holder = ShareTrackingDriver::new(&db, &client, ROUND_ID).with_policy(waiting_policy());

    let holding_run = holder.run(&host, &control, &held_events);
    tokio::pin!(holding_run);
    assert!(
        tokio::time::timeout(Duration::from_millis(1), &mut holding_run)
            .await
            .is_err(),
        "the first run should be waiting for its share to come due"
    );

    let other_host = ScriptedHost::fixed(Some(VOTE_END));
    let other_events = RecordingReporter::default();
    let other = ShareTrackingDriver::new(&db, &client, OTHER_ROUND_ID).with_policy(
        ShareTrackingDrivePolicy {
            max_passes: Some(1),
            ..ShareTrackingDrivePolicy::default()
        },
    );
    let report = other.run(&other_host, &control, &other_events).await;

    assert!(
        matches!(
            report.quiescence,
            ShareTrackingQuiescence::PassBudgetExhausted { .. }
        ),
        "the other round's run polls rather than being turned away, got {:?}",
        report.quiescence,
    );
    assert_eq!(report.passes, 1);

    control.cancel();
    holding_run.await;
}

#[tokio::test(start_paused = true)]
async fn a_replacement_takes_the_round_over_from_a_cancelled_run() {
    // The dangerous overlap is not two live runs, it is a run replacing a
    // cancelled one. A host that cancels a run and starts its replacement can
    // arrive between the cancel and the holder's return; turning that
    // replacement away would leave the round with no run at all, and nothing
    // restarts it until the next lifecycle event.
    let db = db_with_pending_share(60);
    let host = ScriptedHost::fixed(Some(VOTE_END));
    let holder_control = ChainSubmissionControl::new(1);
    let client = client();
    let holder_events = RecordingReporter::default();
    let holder = ShareTrackingDriver::new(&db, &client, ROUND_ID).with_policy(waiting_policy());

    let holding_run = holder.run(&host, &holder_control, &holder_events);
    tokio::pin!(holding_run);
    assert!(
        tokio::time::timeout(Duration::from_millis(1), &mut holding_run)
            .await
            .is_err(),
        "the first run should be waiting for its share to come due"
    );

    // Cancel the holder but do not poll it: the replacement starts while the
    // holder still holds the round, which is exactly the race.
    holder_control.cancel();

    let replacement_control = ChainSubmissionControl::new(1);
    let replacement_host = ScriptedHost::fixed(Some(VOTE_END));
    let replacement_events = RecordingReporter::default();
    let replacement =
        ShareTrackingDriver::new(&db, &client, ROUND_ID).with_policy(ShareTrackingDrivePolicy {
            max_passes: Some(1),
            ..ShareTrackingDrivePolicy::default()
        });

    let replacement_run =
        replacement.run(&replacement_host, &replacement_control, &replacement_events);
    // The holder is not polled at all between its cancellation and this
    // point, so the replacement's takeover cannot depend on the holder having
    // unwound promptly — only on its state being readable.
    tokio::pin!(replacement_run);
    // Polled first, while the holder still holds the round: the replacement
    // must wait for the departure rather than conclude a run is active.
    assert!(
        tokio::time::timeout(Duration::from_millis(1), &mut replacement_run)
            .await
            .is_err(),
        "the replacement should be waiting for the departing holder to release"
    );

    let (holder_report, replacement_report) = tokio::join!(holding_run, replacement_run);

    assert_eq!(holder_report.quiescence, ShareTrackingQuiescence::Cancelled);
    assert!(
        matches!(
            replacement_report.quiescence,
            ShareTrackingQuiescence::PassBudgetExhausted { .. }
        ),
        "the replacement drives the round rather than being turned away, got {:?}",
        replacement_report.quiescence,
    );
    assert_eq!(replacement_report.passes, 1);
}

#[tokio::test(start_paused = true)]
async fn a_caller_cancelled_inside_the_wait_reports_cancellation() {
    // A caller waiting out a departing holder still observes its own stop
    // signal, and must not report the wait as another run's activity. The
    // wait for a departing holder is unbounded by design, so this is the only
    // thing that ends it.
    let db = db_with_pending_share(60);
    let host = ScriptedHost::fixed(Some(VOTE_END));
    let holder_control = ChainSubmissionControl::new(1);
    let client = client();
    let holder_events = RecordingReporter::default();
    let holder = ShareTrackingDriver::new(&db, &client, ROUND_ID).with_policy(waiting_policy());

    let holding_run = holder.run(&host, &holder_control, &holder_events);
    tokio::pin!(holding_run);
    assert!(
        tokio::time::timeout(Duration::from_millis(1), &mut holding_run)
            .await
            .is_err(),
        "the first run should be waiting for its share to come due"
    );

    // Put the holder into its departing state, so the waiter genuinely waits
    // rather than being refused, and do not poll it — the waiter must be
    // sitting inside the wait when its own cancellation lands.
    holder_control.cancel();

    let waiter_control = ChainSubmissionControl::new(1);
    let waiter_host = ScriptedHost::fixed(Some(VOTE_END));
    let waiter_events = RecordingReporter::default();
    let waiter = ShareTrackingDriver::new(&db, &client, ROUND_ID).with_policy(waiting_policy());
    let waiting_run = waiter.run(&waiter_host, &waiter_control, &waiter_events);
    tokio::pin!(waiting_run);
    assert!(
        tokio::time::timeout(Duration::from_millis(1), &mut waiting_run)
            .await
            .is_err(),
        "the waiter should be inside the wait for the departing holder"
    );

    waiter_control.cancel();
    let report = tokio::time::timeout(Duration::from_secs(1), waiting_run)
        .await
        .expect("a cancelled waiter stops rather than waiting the release out");

    assert_eq!(report.quiescence, ShareTrackingQuiescence::Cancelled);
    assert_eq!(report.passes, 0);
    assert_eq!(
        *waiter_host.reads.lock().unwrap(),
        0,
        "a waiter that never got the round read nothing",
    );

    holding_run.await;
}

#[tokio::test(start_paused = true)]
async fn two_sidecars_holding_the_same_round_do_not_block_each_other() {
    // Two independently opened databases can carry the same wallet id and
    // round id while holding separate share rows. A run over one cannot touch
    // the other's rows, so they are not each other's concurrency.
    let shared_wallet = unique_wallet_id();
    let first = empty_db();
    first.set_wallet_id(&shared_wallet);
    seed_round_with_pending_share(&first, ROUND_ID, 60);
    let second = empty_db();
    second.set_wallet_id(&shared_wallet);
    seed_round_with_pending_share(&second, ROUND_ID, 60);

    let control = ChainSubmissionControl::new(1);
    let client = client();
    let host = ScriptedHost::fixed(Some(VOTE_END));
    let holder_events = RecordingReporter::default();
    let holder = ShareTrackingDriver::new(&first, &client, ROUND_ID).with_policy(waiting_policy());

    let holding_run = holder.run(&host, &control, &holder_events);
    tokio::pin!(holding_run);
    assert!(
        tokio::time::timeout(Duration::from_millis(1), &mut holding_run)
            .await
            .is_err(),
        "the first run should be waiting for its share to come due"
    );

    let other_host = ScriptedHost::fixed(Some(VOTE_END));
    let other_events = RecordingReporter::default();
    let other = ShareTrackingDriver::new(&second, &client, ROUND_ID).with_policy(
        ShareTrackingDrivePolicy {
            max_passes: Some(1),
            ..ShareTrackingDrivePolicy::default()
        },
    );
    let report = other.run(&other_host, &control, &other_events).await;

    assert!(
        matches!(
            report.quiescence,
            ShareTrackingQuiescence::PassBudgetExhausted { .. }
        ),
        "the other sidecar's run polls its own rows, got {:?}",
        report.quiescence,
    );

    control.cancel();
    holding_run.await;
}

#[tokio::test(start_paused = true)]
async fn a_live_holder_turns_a_caller_away_without_making_it_wait() {
    // Liveness is read from the holder's own control, not inferred from how
    // long it takes to release, so a caller meeting a live run is answered at
    // once rather than after a handoff window.
    let db = db_with_pending_share(60);
    let host = ScriptedHost::fixed(Some(VOTE_END));
    let control = ChainSubmissionControl::new(1);
    let client = client();
    let holder_events = RecordingReporter::default();
    let holder = ShareTrackingDriver::new(&db, &client, ROUND_ID).with_policy(waiting_policy());

    let holding_run = holder.run(&host, &control, &holder_events);
    tokio::pin!(holding_run);
    assert!(
        tokio::time::timeout(Duration::from_millis(1), &mut holding_run)
            .await
            .is_err(),
        "the first run should be waiting for its share to come due"
    );

    let second_host = ScriptedHost::fixed(Some(VOTE_END));
    let second_events = RecordingReporter::default();
    let second = ShareTrackingDriver::new(&db, &client, ROUND_ID).with_policy(waiting_policy());
    let report = tokio::time::timeout(
        Duration::from_millis(1),
        second.run(&second_host, &control, &second_events),
    )
    .await
    .expect("a live holder is an immediate answer, not a wait");

    assert_eq!(report.quiescence, ShareTrackingQuiescence::AlreadyDriving);

    control.cancel();
    holding_run.await;
}

#[tokio::test(start_paused = true)]
async fn a_holder_left_behind_by_an_epoch_change_hands_the_round_over() {
    // A host that switches account moves the epoch rather than cancelling, and
    // the run it left behind is departing just as surely. Its replacement runs
    // under the new epoch and must not be turned away.
    let db = db_with_pending_share(60);
    let host = ScriptedHost::fixed(Some(VOTE_END));
    let control = ChainSubmissionControl::new(1);
    let client = client();
    let holder_events = RecordingReporter::default();
    let holder = ShareTrackingDriver::new(&db, &client, ROUND_ID).with_policy(waiting_policy());

    let holding_run = holder.run(&host, &control, &holder_events);
    tokio::pin!(holding_run);
    assert!(
        tokio::time::timeout(Duration::from_millis(1), &mut holding_run)
            .await
            .is_err(),
        "the first run should be waiting for its share to come due"
    );

    control.set_operation_epoch(2);

    let replacement_host = ScriptedHost::fixed(Some(VOTE_END));
    let replacement_events = RecordingReporter::default();
    let replacement =
        ShareTrackingDriver::new(&db, &client, ROUND_ID).with_policy(ShareTrackingDrivePolicy {
            max_passes: Some(1),
            ..ShareTrackingDrivePolicy::default()
        });
    let replacement_run = replacement.run(&replacement_host, &control, &replacement_events);
    tokio::pin!(replacement_run);
    assert!(
        tokio::time::timeout(Duration::from_millis(1), &mut replacement_run)
            .await
            .is_err(),
        "the replacement waits for the run the epoch change left behind"
    );

    let (holder_report, replacement_report) = tokio::join!(holding_run, replacement_run);

    assert_eq!(holder_report.quiescence, ShareTrackingQuiescence::Cancelled);
    assert!(
        matches!(
            replacement_report.quiescence,
            ShareTrackingQuiescence::PassBudgetExhausted { .. }
        ),
        "the replacement drives the round, got {:?}",
        replacement_report.quiescence,
    );
}
