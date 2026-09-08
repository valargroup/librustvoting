//! Where the cadence comes from, and what the driver reads per pass.

use super::fixtures::*;

#[tokio::test(start_paused = true)]
async fn the_wait_between_passes_is_the_one_the_pass_computed() {
    // The driver invents no cadence of its own between successful passes. A
    // share 60s from its status check is polled on the timing policy's
    // schedule, capped by `future_check_max_delay_seconds`.
    let db = db_with_pending_share(60);
    let host = ScriptedHost::fixed(Some(VOTE_END));
    let control = ChainSubmissionControl::new(1);
    let timing = ShareTrackingDrivePolicy::default().timing;

    let (_, events) = drive(
        &db,
        &host,
        &control,
        ShareTrackingDrivePolicy {
            max_passes: 2,
            ..ShareTrackingDrivePolicy::default()
        },
    )
    .await;

    let delays = events.delays();
    assert_eq!(delays.len(), 1, "one wait between two passes");
    assert_eq!(
        delays[0],
        Duration::from_secs(timing.future_check_max_delay_seconds),
        "a check further out than the cap waits the cap, not the whole gap",
    );
}

#[tokio::test(start_paused = true)]
async fn a_pass_close_to_its_check_waits_only_until_then() {
    // Under the cap the delay is the real distance to the status check, so a
    // share becomes pollable as soon as it is due rather than a fixed tick
    // later.
    let timing = ShareTrackingDrivePolicy::default().timing;
    let ahead = timing.future_check_max_delay_seconds / 2;
    let db = db_with_pending_share(ahead);
    let host = ScriptedHost::fixed(Some(VOTE_END));
    let control = ChainSubmissionControl::new(1);

    let (_, events) = drive(
        &db,
        &host,
        &control,
        ShareTrackingDrivePolicy {
            max_passes: 2,
            ..ShareTrackingDrivePolicy::default()
        },
    )
    .await;

    assert_eq!(
        events.delays(),
        vec![Duration::from_secs(
            ahead + timing.status_check_grace_seconds
        )],
    );
}

#[tokio::test(start_paused = true)]
async fn the_host_context_is_read_once_per_pass() {
    // A run can span hours, so a refreshed fleet or clock must reach the next
    // pass. Freezing one context for the whole run would pin both.
    let db = db_with_pending_share(60);
    let host = ScriptedHost::fixed(Some(VOTE_END));
    let control = ChainSubmissionControl::new(1);

    let (report, _) = drive(
        &db,
        &host,
        &control,
        ShareTrackingDrivePolicy {
            max_passes: 3,
            ..ShareTrackingDrivePolicy::default()
        },
    )
    .await;

    assert_eq!(report.passes, 3);
    assert_eq!(
        *host.reads.lock().unwrap(),
        3,
        "one read per pass, and none for a pass the budget will not allow",
    );
}

#[tokio::test(start_paused = true)]
async fn a_long_wait_is_cancelled_at_once_rather_than_slept_out() {
    // A share that is not due for hours leaves the driver in one long wait, and
    // a destructive drain has to stop it at once rather than at the end of it.
    //
    // This pins the semantics. The mechanism — that the wait costs one timer
    // however long it lasts, rather than re-reading the control on a tick — is
    // a property of `sleep_until_interrupted` itself, pinned by
    // `a_cancelled_wait_ends_without_advancing_the_clock` in
    // `round_drive/tests/repoll.rs`.
    let db = db_with_pending_share(60);
    let host = ScriptedHost::fixed(Some(VOTE_END));
    let control = ChainSubmissionControl::new(1);
    let client = client();
    let events = RecordingReporter::default();
    let driver =
        ShareTrackingDriver::new(&db, &client, ROUND_ID).with_policy(ShareTrackingDrivePolicy {
            // Far longer than the test could ever wait out.
            timing: crate::share_policy::ShareTimingPolicy {
                future_check_max_delay_seconds: 60 * 60 * 24,
                ..ShareTrackingDrivePolicy::default().timing
            },
            ..ShareTrackingDrivePolicy::default()
        });

    let run = driver.run(&host, &control, &events);
    tokio::pin!(run);

    // Let the first pass finish and the driver settle into its wait.
    assert!(
        tokio::time::timeout(Duration::from_millis(1), &mut run)
            .await
            .is_err(),
        "the run should still be waiting for the share to come due"
    );

    control.cancel();
    let report = tokio::time::timeout(Duration::from_secs(1), run)
        .await
        .expect("a cancelled wait ends without waiting out its delay");

    assert_eq!(report.quiescence, ShareTrackingQuiescence::Cancelled);
    assert_eq!(report.passes, 1);
}

#[test]
fn the_default_policy_is_the_cadence_the_host_was_driving_by_hand() {
    let policy = ShareTrackingDrivePolicy::default();
    assert_eq!(
        policy.failure_retry,
        Duration::from_secs(15),
        "the retry delay the Dart host used before the SDK owned the loop",
    );
    assert_eq!(
        policy.max_consecutive_failures, 240,
        "about an hour of retries at the failure delay, so a helper outage \
         does not abandon a round's shares",
    );
    assert_eq!(policy.max_passes, 1024);
}
