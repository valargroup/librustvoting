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
    assert_eq!(
        events.unconfirmed_at_entry(),
        vec![Some(0)],
        "the verdict rests on what the round owed at entry, which the pass reports",
    );
}

#[tokio::test(start_paused = true)]
async fn a_round_that_owed_a_share_is_never_reported_as_nothing_to_track() {
    // `NothingToTrack` and `AllConfirmed` are different answers for a host, so
    // the run must not infer the first from a pass that merely did nothing.
    // A pass can confirm and resubmit nothing and still have had a share to
    // walk — one another task confirmed underneath it, for instance — and the
    // round did owe something at entry.
    let db = db_with_pending_share(60);
    let host = ScriptedHost::fixed(Some(VOTE_END));
    let control = ChainSubmissionControl::new(1);

    let (report, events) = drive(
        &db,
        &host,
        &control,
        ShareTrackingDrivePolicy {
            max_passes: Some(1),
            ..ShareTrackingDrivePolicy::default()
        },
    )
    .await;

    assert_eq!(
        events.unconfirmed_at_entry(),
        vec![Some(1)],
        "the pass reports the share it set out to track",
    );
    assert!(
        !matches!(report.quiescence, ShareTrackingQuiescence::NothingToTrack),
        "the round owed a share, whatever the pass managed to do about it",
    );
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
        report.passes, 1,
        "the epoch changes as the second pass's inputs are read, and the run \
         stops there rather than dispatching a pass it already knows is stale",
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
async fn a_pass_that_fails_while_the_run_is_draining_reports_cancellation() {
    // Cancellation outranks a failure verdict. A pass that failed because the
    // host was draining the run says nothing about the round's health, and
    // `Failing` would send a host looking for a fault it caused. The failure
    // is still recorded.
    //
    // The cancellation lands inside the reporter, which the driver calls
    // synchronously between the failure and its decision about it — the one
    // point that reaches this branch rather than an earlier boundary check.
    // `max_consecutive_failures: 1` means the failure alone would otherwise
    // end the run as `Failing`, so the verdict here is the interruption
    // winning.
    let db = db_with_pending_share(60);
    let control = ChainSubmissionControl::new(1);
    let host = ScriptedHost::scripted(vec![ShareTrackingHostContext {
        configured_helper_urls: Vec::new(),
        now_seconds: NOW,
        vote_end_time_seconds: Some(VOTE_END),
    }]);
    let client = client();
    let events = CancellingOnFailureReporter::new(&control);

    let report = ShareTrackingDriver::new(&db, &client, ROUND_ID)
        .with_policy(ShareTrackingDrivePolicy {
            max_consecutive_failures: 1,
            ..ShareTrackingDrivePolicy::default()
        })
        .run(&host, &control, &events)
        .await;

    assert_eq!(report.quiescence, ShareTrackingQuiescence::Cancelled);
    assert_eq!(report.passes, 1);
    assert_eq!(
        report.failures.len(),
        1,
        "the failure is reported even though it is not why the run stopped",
    );
}

#[tokio::test(start_paused = true)]
async fn an_interruption_during_the_host_callback_outranks_the_round_verdict() {
    // `host_context()` is synchronous and arbitrary, so a host can cancel or
    // switch epoch while it runs. The vote-end and pass-budget checks read the
    // context it returned, so without a recheck the run would hand back a
    // verdict about the round for a run the host had already dropped.
    let db = db_with_pending_share(60);
    let control = ChainSubmissionControl::new(1);
    let host = EpochBumpingHost::with_a_fleet_that_fails_the_first_pass(&control);

    let (report, events) = drive_with(
        &db,
        &host,
        &control,
        // A zero budget: the very next check after the callback would
        // otherwise report `PassBudgetExhausted`.
        ShareTrackingDrivePolicy {
            max_passes: Some(0),
            ..ShareTrackingDrivePolicy::default()
        },
    )
    .await;

    assert_eq!(report.quiescence, ShareTrackingQuiescence::Cancelled);
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

#[tokio::test(start_paused = true)]
async fn a_wallet_switched_under_a_run_neither_redirects_it_nor_outlives_it() {
    // Two properties, and the host here switches the wallet at the one moment
    // that tests both: after the run's boundary check, while its next pass is
    // being set up.
    //
    // The pass acts under the wallet the run was *admitted* for, not whatever
    // the sidecar is scoped to by the time it starts — otherwise a switch
    // landing in this window would have the run drive another wallet's rows
    // under this run's admission, beside that wallet's own admitted run. Each
    // pass finding the share proves it: scoped to the switched-in wallet it
    // would have found none.
    //
    // And the run does not carry on past the switch: the next boundary check
    // ends it, because the host is driving something else now.
    let db = db_with_pending_share(60);
    let control = ChainSubmissionControl::new(1);
    let host = WalletSwitchingHost::after_first_pass(&db);

    let (report, events) =
        drive_with(&db, &host, &control, ShareTrackingDrivePolicy::default()).await;

    assert_eq!(
        events.unconfirmed_at_entry(),
        vec![Some(1), Some(1)],
        "every pass walked the admitted wallet's share",
    );
    assert_eq!(report.quiescence, ShareTrackingQuiescence::Cancelled);
    assert_eq!(
        report.passes, 2,
        "the pass already under way completed; the run stopped at the next boundary",
    );
}

#[tokio::test(start_paused = true)]
async fn a_pass_that_failed_before_looking_reports_no_entry_count_at_all() {
    // An empty fleet is rejected before storage is read, so the pass never
    // learns what the round owed. Reporting zero there would tell a host the
    // round owed nothing while it still held pending shares.
    let db = db_with_pending_share(60);
    let control = ChainSubmissionControl::new(1);

    let (_, events) = drive(
        &db,
        &ScriptedHost::scripted(vec![ShareTrackingHostContext {
            configured_helper_urls: Vec::new(),
            now_seconds: NOW,
            vote_end_time_seconds: Some(VOTE_END),
        }]),
        &control,
        ShareTrackingDrivePolicy {
            max_consecutive_failures: 1,
            ..ShareTrackingDrivePolicy::default()
        },
    )
    .await;

    assert_eq!(
        events.failed_pass_entry_counts(),
        vec![None],
        "absent, not zero: the pass failed before it could look",
    );
}
