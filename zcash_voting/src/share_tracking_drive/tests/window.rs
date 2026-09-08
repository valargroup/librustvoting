//! Waits that would outlast the round they belong to.
//!
//! A pass computes its delay from share rows, which say when a share is next
//! worth polling and nothing about when the round closes. The driver knows the
//! boundary, so it is the driver that keeps a wait inside it.

use super::fixtures::*;

/// Contexts for a run that starts `seconds_left` before vote end and finds the
/// boundary passed on its next pass.
fn closing_window(seconds_left: u64, fleet: Vec<String>) -> ScriptedHost {
    ScriptedHost::scripted(vec![
        ShareTrackingHostContext {
            configured_helper_urls: fleet.clone(),
            now_seconds: NOW,
            vote_end_time_seconds: Some(NOW + seconds_left),
        },
        ShareTrackingHostContext {
            configured_helper_urls: fleet,
            now_seconds: NOW,
            vote_end_time_seconds: Some(NOW - 1),
        },
    ])
}

#[tokio::test(start_paused = true)]
async fn a_pass_delay_is_shortened_to_what_is_left_of_the_window() {
    // Uncapped this share's next check is `future_check_max_delay_seconds`
    // away, well past a window with five seconds left. Sleeping through the
    // boundary would leave the run holding a round it can no longer act on
    // until a whole poll interval had elapsed.
    let db = db_with_pending_share(60);
    let host = closing_window(5, fleet());
    let control = ChainSubmissionControl::new(1);

    let (report, events) =
        drive_with(&db, &host, &control, ShareTrackingDrivePolicy::default()).await;

    assert_eq!(
        events.delays(),
        vec![Duration::from_secs(5)],
        "the wait ends at vote end, not at the share's next check",
    );
    assert_eq!(report.quiescence, ShareTrackingQuiescence::VoteEndReached);
}

#[tokio::test(start_paused = true)]
async fn a_failure_retry_is_shortened_to_what_is_left_of_the_window() {
    // The failure path has the same boundary as the successful one. A
    // transient fault a few seconds before vote end must not spend the last
    // usable pass of the round on the policy's retry delay.
    let db = db_with_pending_share(60);
    let host = closing_window(5, Vec::new());
    let control = ChainSubmissionControl::new(1);

    let (report, events) =
        drive_with(&db, &host, &control, ShareTrackingDrivePolicy::default()).await;

    assert!(
        ShareTrackingDrivePolicy::default().failure_retry > Duration::from_secs(5),
        "the retry the policy asks for must be the longer one for this to mean anything",
    );
    assert_eq!(events.delays(), vec![Duration::from_secs(5)]);
    assert_eq!(report.quiescence, ShareTrackingQuiescence::VoteEndReached);
    assert_eq!(
        report.failures.len(),
        1,
        "the failed pass is still recorded"
    );
}

#[tokio::test(start_paused = true)]
async fn a_round_with_no_vote_end_keeps_the_delay_it_was_given() {
    // Absent timing is not a boundary of zero. A round the host reports no end
    // for is paced by its share rows alone.
    let db = db_with_pending_share(60);
    let host = ScriptedHost::fixed(None);
    let control = ChainSubmissionControl::new(1);
    let timing = ShareTrackingDrivePolicy::default().timing;

    let (_, events) = drive(
        &db,
        &host,
        &control,
        ShareTrackingDrivePolicy {
            max_passes: Some(2),
            ..ShareTrackingDrivePolicy::default()
        },
    )
    .await;

    assert_eq!(
        events.delays(),
        vec![Duration::from_secs(timing.future_check_max_delay_seconds)],
    );
}

#[tokio::test(start_paused = true)]
async fn the_pass_that_produced_a_delay_is_charged_to_the_window() {
    // `now_seconds` is read before the pass, so a pass that takes real time
    // has already spent part of the window by the time the cap is computed.
    // Capping against the pre-pass clock would let a slow pass plus a capped
    // wait land past vote end, holding the round — and its admission — beyond
    // the boundary.
    let db = db_with_pending_share(60);
    let host = closing_window(20, fleet());
    let control = ChainSubmissionControl::new(1);

    let (report, events) =
        drive_charging_each_pass(&db, &host, &control, ShareTrackingDrivePolicy::default(), 8)
            .await;

    assert_eq!(
        events.delays(),
        vec![Duration::from_secs(12)],
        "twenty seconds of window less the eight the pass spent",
    );
    assert_eq!(report.quiescence, ShareTrackingQuiescence::VoteEndReached);
}

#[tokio::test(start_paused = true)]
async fn a_pass_that_outlasts_the_window_leaves_no_wait_at_all() {
    let db = db_with_pending_share(60);
    let host = closing_window(5, fleet());
    let control = ChainSubmissionControl::new(1);

    let (_, events) = drive_charging_each_pass(
        &db,
        &host,
        &control,
        ShareTrackingDrivePolicy::default(),
        30,
    )
    .await;

    assert_eq!(
        events.delays(),
        vec![Duration::ZERO],
        "the window closed during the pass, so the run goes straight back to \
         the boundary check rather than sleeping past it",
    );
}
