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
