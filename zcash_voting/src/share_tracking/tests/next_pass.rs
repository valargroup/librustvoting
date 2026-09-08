//! What one pass tells the next: the shares still unconfirmed, and how long to
//! wait before asking again.

use super::*;

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

/// Builds two pending shares where only the first generation has corrupt
/// recovery material and therefore reaches an unrecoverable observation.
fn db_with_unrecoverable_first_share(configured: &[String]) -> VotingDb {
    let db = db_with_delivery(&configured[..1], &[], 2);
    let second_submission = ShareSubmissionReport {
        accepted_urls: configured[..1].to_vec(),
        ambiguous_urls: Vec::new(),
        target_count: 1,
    };
    share::record_delivery(
        &db,
        &share::ShareDeliveryRecordParams {
            round_id: ROUND_ID,
            bundle_index: 0,
            proposal_id: 1,
            share_index: 1,
            submission: &second_submission,
            submit_at: SUBMIT_AT,
        },
    )
    .unwrap();

    let mut corrupt_recovery = recovery_bundle_fixture();
    corrupt_recovery.share_blinds[0] = field_bytes(9);
    db.conn()
        .execute(
            "UPDATE votes SET commitment_bundle_json = :json
             WHERE round_id = :round_id AND wallet_id = :wallet_id
               AND bundle_index = 0 AND proposal_id = 1",
            rusqlite::named_params! {
                ":json": serialize_recovery(&corrupt_recovery).unwrap(),
                ":round_id": ROUND_ID,
                ":wallet_id": WALLET_ID,
            },
        )
        .unwrap();
    db
}

fn queue_pending_statuses(transport: &MockTransport, configured: &[String], share_ids: &[String]) {
    for share_id in share_ids {
        for server_url in configured {
            transport.queue_get(
                &format!("{server_url}/shielded-vote/v1/share-status/{ROUND_ID}/{share_id}"),
                json_status("pending"),
            );
        }
    }
}

// ---- The round-boundary cap ------------------------------------------

/// The cap under the default policy: a ten-second resubmission cutoff, and a
/// ten-second status budget reserved for the walk that precedes recovery.
fn capped(delay_seconds: u64, now_seconds: u64, vote_end_time_seconds: Option<u64>) -> u64 {
    cap_delay_at_next_round_boundary(
        delay_seconds,
        now_seconds,
        crate::share_policy::RoundWindow::new(vote_end_time_seconds, ShareTimingPolicy::default()),
    )
}

#[test]
fn a_delay_landing_before_every_boundary_is_left_alone() {
    // Recovery stays open until 1_089, which the 15 does not reach.
    assert_eq!(capped(15, 1_000, Some(1_100)), 15);
}

#[test]
fn a_delay_stepping_over_the_last_usable_start_is_shortened_to_it() {
    // Recovery closes ten seconds before the vote end, so the last permitted
    // second is 1_039. A pass must *begin* early enough to still reach its
    // recovery phase, and it walks helper status first with a ten-second
    // budget, so the last usable start is 1_029 rather than 1_039. Waking at
    // 1_039 would spend the window on the walk and suppress every POST.
    assert_eq!(capped(60, 1_000, Some(1_050)), 29);
}

#[test]
fn a_round_too_close_to_fit_a_status_walk_wakes_for_the_vote_end() {
    // The window is open right now — 1_000 is before the 1_009 cutoff — but no
    // *future* start leaves room for the walk that precedes recovery. There is
    // no recovery wake worth scheduling, so the vote end is the boundary and
    // only the pass already running can still resubmit.
    assert_eq!(capped(15, 1_000, Some(1_020)), 15);
    assert!(
        crate::share_policy::RoundWindow::new(Some(1_020), ShareTimingPolicy::default())
            .can_resubmit_at(1_000)
    );
}

#[test]
fn a_pass_on_the_last_open_second_waits_for_the_vote_end_not_for_itself() {
    // 1_000 *is* the last second a resubmission is permitted, so there is no
    // later open second to wake for and the vote end is the only boundary
    // left. Treating the current second as the boundary would cap the delay to
    // zero and spend a pass re-waking on the second it is already inside.
    assert_eq!(capped(15, 1_000, Some(1_011)), 11);
    assert!(
        crate::share_policy::RoundWindow::new(Some(1_011), ShareTimingPolicy::default())
            .can_resubmit_at(1_000)
    );
    assert!(
        !crate::share_policy::RoundWindow::new(Some(1_011), ShareTimingPolicy::default())
            .can_resubmit_at(1_001)
    );
}

#[test]
fn a_round_past_its_recovery_cutoff_still_wakes_by_the_vote_end() {
    // No open recovery second is left, so the vote end is the only boundary
    // still ahead and confirmation is all a final pass could do.
    assert_eq!(capped(15, 1_000, Some(1_005)), 5);
}

#[test]
fn a_round_at_or_past_its_vote_end_yields_no_wait() {
    // Not a floor of `min_tracking_delay_seconds`: whether to run a final pass
    // at all is the caller's decision, not something to encode as a wait.
    assert_eq!(capped(15, 1_000, Some(1_000)), 0);
    assert_eq!(capped(15, 1_000, Some(900)), 0);
}

#[test]
fn a_round_without_a_vote_end_keeps_its_delay() {
    assert_eq!(capped(15, 1_000, None), 15);
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
async fn concurrent_confirmation_removes_an_unrecoverable_observation_from_the_final_snapshot() {
    let configured = helpers(2);
    let db = Arc::new(db_with_unrecoverable_first_share(&configured));
    let first_share_id = share_id_at(&db, 0);
    let second_share_id = share_id_at(&db, 1);
    let transport = Arc::new(MockTransport::default());
    queue_pending_statuses(
        &transport,
        &configured,
        &[first_share_id, second_share_id.clone()],
    );
    let confirming_db = Arc::clone(&db);
    transport.observe_gets(move |url| {
        if url.contains(&second_share_id) {
            share::confirm(&confirming_db, ROUND_ID, 0, 1, 0).unwrap();
        }
    });
    let client = client_with(transport);
    let random = zero_bytes;

    let report = track_pending_shares(
        &db,
        &params(&configured, ready_not_overdue(), &random),
        &client,
        &never_cancel(),
    )
    .await
    .unwrap();

    assert_eq!(report.remaining_unconfirmed, 1);
    assert!(report.unrecoverable.is_empty());
    assert!(report.terminal_unconfirmed.is_empty());
}

#[tokio::test(start_paused = true)]
async fn replacement_generation_does_not_inherit_an_unrecoverable_observation() {
    let configured = helpers(2);
    let db = Arc::new(db_with_unrecoverable_first_share(&configured));
    let first_share_id = share_id_at(&db, 0);
    let second_share_id = share_id_at(&db, 1);
    let transport = Arc::new(MockTransport::default());
    queue_pending_statuses(
        &transport,
        &configured,
        &[first_share_id, second_share_id.clone()],
    );
    let replacing_db = Arc::clone(&db);
    transport.observe_gets(move |url| {
        if url.contains(&second_share_id) {
            replacing_db
                .conn()
                .execute(
                    "UPDATE share_delegations SET nullifier = :nullifier
                     WHERE round_id = :round_id AND wallet_id = :wallet_id
                       AND bundle_index = 0 AND proposal_id = 1 AND share_index = 0",
                    rusqlite::named_params! {
                        ":nullifier": vec![0xEEu8; 32],
                        ":round_id": ROUND_ID,
                        ":wallet_id": WALLET_ID,
                    },
                )
                .unwrap();
        }
    });
    let client = client_with(transport);
    let random = zero_bytes;

    let report = track_pending_shares(
        &db,
        &params(&configured, ready_not_overdue(), &random),
        &client,
        &never_cancel(),
    )
    .await
    .unwrap();

    assert_eq!(report.remaining_unconfirmed, 2);
    assert!(report.unrecoverable.is_empty());
    assert!(report.terminal_unconfirmed.is_empty());
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
async fn the_next_delay_a_pass_reports_leaves_room_for_the_status_walk() {
    let configured = helpers(5);
    let db = db_with_share(&configured);
    let share_id = share_id_of(&db);
    let now = ready_not_overdue();
    // Thirty seconds left. Recovery shuts ten before the vote end, so the last
    // permitted second is `now + 19` — but a pass walks helper status before it
    // decides anything about recovery, with a ten-second budget, so the last
    // second it can *begin* and still get there is `now + 9`. Waking at
    // `now + 19` would spend the window on the walk and suppress every POST,
    // and the plain fifteen-second interval steps over both.
    let policy = ShareTimingPolicy::default();
    let vote_end = now + 30;
    assert_eq!(policy.resubmit_cutoff_seconds, 10);
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
    assert_eq!(
        report.next_delay_seconds,
        Some(9),
        "the pass must wake early enough to still reach its recovery phase",
    );
}

#[tokio::test(start_paused = true)]
async fn a_pass_too_close_to_fit_a_status_walk_wakes_for_the_vote_end() {
    let configured = helpers(5);
    let db = db_with_share(&configured);
    let share_id = share_id_of(&db);
    let now = ready_not_overdue();
    // Twenty seconds left. Recovery is open right now, but no future start
    // leaves room for the walk that precedes it, so there is no recovery wake
    // worth scheduling and the vote end is the only boundary left.
    let vote_end = now + 20;
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

    assert_eq!(report.next_delay_seconds, Some(15));
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
