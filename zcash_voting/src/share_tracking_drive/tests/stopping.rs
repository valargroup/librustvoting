//! Every reason a tracking run ends, and what it leaves behind.

use super::fixtures::*;

#[tokio::test(start_paused = true)]
async fn a_round_with_nothing_pending_is_not_tracked() {
    let db = empty_db();
    let host = ScriptedHost::fixed(Some(VOTE_END));
    let control = ChainSubmissionControl::new(1);

    let (report, events) = drive(&db, &host, &control, ShareTrackingDrivePolicy::default()).await;

    assert_eq!(report.quiescence, ShareTrackingQuiescence::NothingToTrack);
    assert_eq!(
        report.passes, 1,
        "one pass establishes there is nothing to track"
    );
    assert!(events.delays().is_empty(), "nothing to wait for");
}

#[tokio::test(start_paused = true)]
async fn a_cancelled_run_stops_before_it_polls_anything() {
    let db = db_with_pending_share(60);
    let host = ScriptedHost::fixed(Some(VOTE_END));
    let control = ChainSubmissionControl::new(1);
    control.cancel();

    let (report, events) = drive(&db, &host, &control, ShareTrackingDrivePolicy::default()).await;

    assert_eq!(report.quiescence, ShareTrackingQuiescence::Cancelled);
    assert_eq!(report.passes, 0);
    assert_eq!(events.passes_started(), 0);
}

#[tokio::test(start_paused = true)]
async fn moving_to_another_operation_epoch_stops_the_run() {
    // The host switched account or round mid-run. An epoch change is the same
    // stop signal as cancellation, so neither needs its own handling in a
    // host. The epoch is captured at entry, so changing it before the run
    // would simply be the epoch the run belongs to.
    let db = db_with_pending_share(60);
    let control = ChainSubmissionControl::new(1);
    let host = EpochBumpingHost::after_first_pass(&control);

    let (report, _) = drive_with(&db, &host, &control, ShareTrackingDrivePolicy::default()).await;

    assert_eq!(report.quiescence, ShareTrackingQuiescence::Cancelled);
    assert_eq!(
        report.passes, 2,
        "the epoch changes as the second pass is read in, and that pass stops          from inside through its own cancel callback rather than running out",
    );
}

#[tokio::test(start_paused = true)]
async fn a_round_past_its_vote_end_is_not_polled() {
    // Recovery closes at vote end, so a pass there could only re-poll shares
    // it cannot act on.
    let db = db_with_pending_share(60);
    let host = ScriptedHost::fixed(Some(NOW - 1));
    let control = ChainSubmissionControl::new(1);

    let (report, events) = drive(&db, &host, &control, ShareTrackingDrivePolicy::default()).await;

    assert_eq!(report.quiescence, ShareTrackingQuiescence::VoteEndReached);
    assert_eq!(report.passes, 0);
    assert_eq!(events.passes_started(), 0);
}

#[tokio::test(start_paused = true)]
async fn vote_end_reached_between_passes_stops_the_run() {
    // The boundary is re-read every pass, so a run that starts inside the
    // window still stops the moment it closes.
    let db = db_with_pending_share(60);
    let host = ScriptedHost::scripted(vec![
        ShareTrackingHostContext {
            configured_helper_urls: fleet(),
            now_seconds: NOW,
            vote_end_time_seconds: Some(VOTE_END),
        },
        ShareTrackingHostContext {
            configured_helper_urls: fleet(),
            now_seconds: NOW,
            vote_end_time_seconds: Some(NOW - 1),
        },
    ]);
    let control = ChainSubmissionControl::new(1);

    let (report, _) = drive(&db, &host, &control, ShareTrackingDrivePolicy::default()).await;

    assert_eq!(report.quiescence, ShareTrackingQuiescence::VoteEndReached);
    assert_eq!(report.passes, 1, "the first pass ran inside the window");
}

#[tokio::test(start_paused = true)]
async fn a_share_that_never_becomes_pollable_stops_at_the_pass_budget() {
    // The budget is the safety net for the one case that would otherwise poll
    // for the rest of the round.
    let db = db_with_pending_share(60);
    let host = ScriptedHost::fixed(Some(VOTE_END));
    let control = ChainSubmissionControl::new(1);

    let (report, events) = drive(
        &db,
        &host,
        &control,
        ShareTrackingDrivePolicy {
            max_passes: Some(3),
            ..ShareTrackingDrivePolicy::default()
        },
    )
    .await;

    assert!(matches!(
        report.quiescence,
        ShareTrackingQuiescence::PassBudgetExhausted { .. }
    ));
    assert_eq!(report.passes, 3);
    assert_eq!(
        events.passes_started(),
        3,
        "the budget bounds passes, not waits"
    );
}

#[tokio::test(start_paused = true)]
async fn the_default_run_is_bounded_by_vote_end_rather_than_a_pass_count() {
    // A share that keeps looking pollable produces a pass per poll interval,
    // so a pass budget is a duration in disguise — and a generous-looking one
    // expires hours into a voting window that can last days, stopping
    // confirmation and recovery with nothing to restart it. The default policy
    // sets none, and vote end is what ends the run.
    let db = db_with_pending_share(60);
    let host = ScriptedHost::scripted(vec![
        ShareTrackingHostContext {
            configured_helper_urls: fleet(),
            now_seconds: NOW,
            vote_end_time_seconds: Some(VOTE_END),
        },
        ShareTrackingHostContext {
            configured_helper_urls: fleet(),
            now_seconds: NOW,
            vote_end_time_seconds: Some(VOTE_END),
        },
        ShareTrackingHostContext {
            configured_helper_urls: fleet(),
            now_seconds: NOW,
            vote_end_time_seconds: Some(NOW - 1),
        },
    ]);
    let control = ChainSubmissionControl::new(1);

    let (report, _) = drive(&db, &host, &control, ShareTrackingDrivePolicy::default()).await;

    assert_eq!(report.quiescence, ShareTrackingQuiescence::VoteEndReached);
    assert_eq!(
        report.passes, 2,
        "the run kept polling until the window closed, not until a budget did",
    );
}

#[tokio::test(start_paused = true)]
async fn a_zero_budget_stops_without_polling() {
    let db = db_with_pending_share(60);
    let host = ScriptedHost::fixed(Some(VOTE_END));
    let control = ChainSubmissionControl::new(1);

    let (report, events) = drive(
        &db,
        &host,
        &control,
        ShareTrackingDrivePolicy {
            max_passes: Some(0),
            ..ShareTrackingDrivePolicy::default()
        },
    )
    .await;

    assert!(matches!(
        report.quiescence,
        ShareTrackingQuiescence::PassBudgetExhausted { .. }
    ));
    assert_eq!(report.passes, 0);
    assert_eq!(events.passes_started(), 0);
}

#[tokio::test(start_paused = true)]
async fn repeated_pass_failures_stop_the_run_and_are_reported() {
    // An empty fleet is rejected before storage or network, so every pass
    // fails the same way. The run backs off and hands the host the messages
    // rather than retrying silently for the rest of the round.
    let db = db_with_pending_share(60);
    let host = ScriptedHost::fixed(Some(VOTE_END));
    let control = ChainSubmissionControl::new(1);

    let (report, events) = drive(
        &db,
        &ScriptedHost::scripted(vec![ShareTrackingHostContext {
            configured_helper_urls: Vec::new(),
            now_seconds: NOW,
            vote_end_time_seconds: Some(VOTE_END),
        }]),
        &control,
        ShareTrackingDrivePolicy {
            max_consecutive_failures: 3,
            ..ShareTrackingDrivePolicy::default()
        },
    )
    .await;
    drop(host);

    let ShareTrackingQuiescence::Failing { messages } = report.quiescence else {
        panic!("expected a failing run, got {:?}", report.quiescence);
    };
    assert_eq!(messages.len(), 3);
    assert_eq!(report.passes, 3);
    assert_eq!(report.failures.len(), 3);
    assert_eq!(
        events.delays(),
        vec![
            ShareTrackingDrivePolicy::default().failure_retry,
            ShareTrackingDrivePolicy::default().failure_retry,
        ],
        "a failed pass computes no delay, so the policy supplies one",
    );
}
