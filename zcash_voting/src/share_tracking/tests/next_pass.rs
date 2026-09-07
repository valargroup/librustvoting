//! What one pass tells the next: the shares still unconfirmed, and how long to
//! wait before asking again.

use super::*;

/// `params`, but for a round whose vote end is a caller-chosen boundary.
fn params_ending_at<'a>(
    configured: &'a [String],
    now_seconds: u64,
    vote_end_time_seconds: Option<u64>,
    random_bytes: &'a (dyn Fn(usize) -> Vec<u8> + Send + Sync),
) -> ShareTrackingParams<'a> {
    ShareTrackingParams {
        round_id: ROUND_ID,
        configured_server_urls: configured,
        now_seconds,
        vote_end_time_seconds,
        policy: ShareTimingPolicy::default(),
        random_bytes,
    }
}

/// Answers every configured helper's status poll for the round's only share.
fn all_helpers_answer(configured: &[String], share_id: &str, status: &str) -> Arc<MockTransport> {
    let transport = Arc::new(MockTransport::default());
    for index in 1..=configured.len() {
        transport.queue_get(
            &format!(
                "{}/shielded-vote/v1/share-status/{ROUND_ID}/{share_id}",
                helper(index)
            ),
            json_status(status),
        );
    }
    transport
}

// ---- The vote-end cap ------------------------------------------------

#[test]
fn a_delay_landing_before_the_vote_end_is_left_alone() {
    assert_eq!(cap_delay_at_vote_end(15, 1_000, Some(1_100)), 15);
}

#[test]
fn a_delay_stepping_over_the_vote_end_is_shortened_to_it() {
    // Recovery closes at the vote end, so waiting the full 15 would skip the
    // last pass that could still resubmit or confirm.
    assert_eq!(cap_delay_at_vote_end(15, 1_000, Some(1_005)), 5);
}

#[test]
fn a_round_at_or_past_its_vote_end_yields_no_wait() {
    // Not a floor of `min_tracking_delay_seconds`: whether to run a final pass
    // at all is the caller's decision, not something to encode as a wait.
    assert_eq!(cap_delay_at_vote_end(15, 1_000, Some(1_000)), 0);
    assert_eq!(cap_delay_at_vote_end(15, 1_000, Some(900)), 0);
}

#[test]
fn a_round_without_a_vote_end_keeps_its_delay() {
    assert_eq!(cap_delay_at_vote_end(15, 1_000, None), 15);
}

// ---- What a whole pass reports ---------------------------------------

#[tokio::test(start_paused = true)]
async fn a_pass_that_confirms_nothing_reports_the_share_still_unconfirmed() {
    let configured = helpers(5);
    let db = db_with_share(&configured);
    let share_id = share_id_of(&db);
    let client = client_with(all_helpers_answer(&configured, &share_id, "pending"));
    let random = zero_bytes;

    let report = track_pending_shares(
        &db,
        &params(&configured, ready_not_overdue(), &random),
        &client,
        &never_cancel(),
    )
    .await
    .unwrap();

    assert!(report.confirmed.is_empty());
    assert_eq!(report.remaining_unconfirmed, 1);
    // Nothing is beyond repair, so the two counts differ: the share is merely
    // waiting, which is the state a next delay alone cannot distinguish.
    assert!(report.unrecoverable.is_empty());
    assert_eq!(
        report.next_delay_seconds,
        Some(ShareTimingPolicy::default().ready_poll_interval_seconds)
    );
}

#[tokio::test(start_paused = true)]
async fn a_confirmed_share_leaves_nothing_unconfirmed_and_no_next_delay() {
    let configured = helpers(5);
    let db = db_with_share(&configured);
    let share_id = share_id_of(&db);
    let client = client_with(all_helpers_answer(&configured, &share_id, "confirmed"));
    let random = zero_bytes;

    let report = track_pending_shares(
        &db,
        &params(&configured, ready_not_overdue(), &random),
        &client,
        &never_cancel(),
    )
    .await
    .unwrap();

    assert_eq!(report.confirmed.len(), 1);
    assert_eq!(report.remaining_unconfirmed, 0);
    assert_eq!(report.next_delay_seconds, None);
}

#[tokio::test(start_paused = true)]
async fn the_next_delay_a_pass_reports_never_steps_over_the_vote_end() {
    let configured = helpers(5);
    let db = db_with_share(&configured);
    let share_id = share_id_of(&db);
    let now = ready_not_overdue();
    // Inside `resubmit_cutoff_seconds` of the end, so the resubmission window
    // is shut and this pass only polls. The uncapped delay would be the ready
    // poll interval, which is longer than the round has left.
    let vote_end = now + 5;
    assert!(vote_end < now + ShareTimingPolicy::default().ready_poll_interval_seconds);
    let client = client_with(all_helpers_answer(&configured, &share_id, "pending"));
    let random = zero_bytes;

    let report = track_pending_shares(
        &db,
        &params_ending_at(&configured, now, Some(vote_end), &random),
        &client,
        &never_cancel(),
    )
    .await
    .unwrap();

    assert_eq!(report.remaining_unconfirmed, 1);
    assert_eq!(report.next_delay_seconds, Some(5));
}

#[tokio::test(start_paused = true)]
async fn a_round_with_no_vote_end_keeps_the_delay_the_shares_asked_for() {
    let configured = helpers(5);
    let db = db_with_share(&configured);
    let share_id = share_id_of(&db);
    let client = client_with(all_helpers_answer(&configured, &share_id, "pending"));
    let random = zero_bytes;

    let report = track_pending_shares(
        &db,
        &params_ending_at(&configured, ready_not_overdue(), None, &random),
        &client,
        &never_cancel(),
    )
    .await
    .unwrap();

    assert_eq!(
        report.next_delay_seconds,
        Some(ShareTimingPolicy::default().ready_poll_interval_seconds),
        "no vote end is no boundary to respect",
    );
}
