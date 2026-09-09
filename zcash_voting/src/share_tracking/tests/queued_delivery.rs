use super::*;

/// Holds every POST slot until the entire wave is journaled, mutates the
/// durable state without cancelling the caller, then lets admission resume.
async fn assert_queued_delivery_is_stale(db: &VotingDb, change_journal: impl FnOnce(&VotingDb)) {
    let capacity_deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    let mut held_permits = Vec::new();
    for _ in 0..crate::share_policy::SHARE_HELPER_MAX_CONCURRENT_POSTS {
        held_permits.push(
            super::super::post_capacity::acquire(capacity_deadline, &never_cancel())
                .await
                .unwrap(),
        );
    }
    let configured = helpers(2);
    let transport = Arc::new(MockTransport::default());
    for helper in &configured {
        transport.queue_post(
            &format!("{helper}/shielded-vote/v1/shares"),
            json_status("queued"),
        );
    }
    let client = client_with(transport.clone());
    let request = InitialShareSubmissionParams {
        target_count: 2,
        ..initial_submission(&configured)
    };
    let change_while_queued = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(only_share(db).attempting_urls, configured);
        assert!(transport.calls().is_empty());
        change_journal(db);
        drop(held_permits);
    };
    let cancel = never_cancel();
    let (delivery, ()) = tokio::join!(
        submit_share_to_helpers(db, &client, &request, &cancel),
        change_while_queued,
    );
    assert!(
        transport.calls().is_empty(),
        "stale queued shares must not be sent"
    );
    let error = delivery.unwrap_err();
    assert!(matches!(error, VotingError::InvalidInput { .. }));
    assert!(error
        .to_string()
        .contains("committed share changed while helper delivery was in flight"));
}

#[tokio::test(start_paused = true)]
async fn queued_delivery_rejects_a_deleted_round_before_posting() {
    let db = db_with_recoverable_vote();
    assert_queued_delivery_is_stale(&db, |db| {
        db.clear_round_discarding_recovery(ROUND_ID).unwrap();
    })
    .await;
    assert!(share::list(&db, ROUND_ID).unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn queued_delivery_leaves_a_replacement_generation_untouched() {
    let db = db_with_recoverable_vote();
    assert_queued_delivery_is_stale(&db, |db| {
        db.conn()
            .execute(
                "UPDATE share_delegations SET nullifier = ?1
             WHERE round_id = ?2 AND wallet_id = ?3
               AND bundle_index = 0 AND proposal_id = 1 AND share_index = 0",
                rusqlite::params![vec![0xF2_u8; 32], ROUND_ID, WALLET_ID],
            )
            .unwrap();
    })
    .await;
    let replacement = only_share(&db);
    assert_eq!(replacement.nullifier, vec![0xF2; 32]);
    assert_eq!(replacement.attempting_urls, helpers(2));
    assert!(replacement.sent_to_urls.is_empty());
    assert!(replacement.ambiguous_urls.is_empty());
}

#[tokio::test(start_paused = true)]
async fn queued_delivery_requires_its_attempt_reservation() {
    let db = db_with_recoverable_vote();
    assert_queued_delivery_is_stale(&db, |db| {
        db.conn()
            .execute(
                "UPDATE share_delegations SET attempting_urls = '[]'
             WHERE round_id = ?1 AND wallet_id = ?2
               AND bundle_index = 0 AND proposal_id = 1 AND share_index = 0",
                rusqlite::params![ROUND_ID, WALLET_ID],
            )
            .unwrap();
    })
    .await;
    let delivery = only_share(&db);
    assert!(delivery.attempting_urls.is_empty());
    assert!(delivery.sent_to_urls.is_empty());
    assert!(delivery.ambiguous_urls.is_empty());
}

#[tokio::test(start_paused = true)]
async fn queued_delivery_does_not_validate_against_a_different_wallet() {
    let db = db_with_recoverable_vote();
    assert_queued_delivery_is_stale(&db, |db| {
        let original_nullifier = only_share(db).nullifier;
        db.clear_round_discarding_recovery(ROUND_ID).unwrap();
        db.set_wallet_id("queued-delivery-other-wallet");
        seed_recoverable_vote_for_wallet(db, "queued-delivery-other-wallet");
        share::record_delivery(
            db,
            &share::ShareDeliveryRecordParams {
                round_id: ROUND_ID,
                bundle_index: 0,
                proposal_id: 1,
                share_index: 0,
                submission: &ShareSubmissionReport {
                    target_count: 2,
                    ..Default::default()
                },
                submit_at: SUBMIT_AT,
            },
        )
        .unwrap();
        for helper in helpers(2) {
            assert!(share::begin_existing_delivery_attempt(
                db,
                &share::ShareDeliveryAttemptParams {
                    round_id: ROUND_ID,
                    bundle_index: 0,
                    proposal_id: 1,
                    share_index: 0,
                    server_url: &helper,
                    target_count: 2,
                    submit_at: SUBMIT_AT,
                },
                &helpers(2)
            )
            .unwrap());
        }
        assert_eq!(only_share(db).nullifier, original_nullifier);
    })
    .await;
    let other_wallet = only_share(&db);
    assert_eq!(other_wallet.attempting_urls, helpers(2));
    assert!(other_wallet.sent_to_urls.is_empty());
    assert!(other_wallet.ambiguous_urls.is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_pass_that_reached_a_helper_does_not_report_local_throttling() {
    // `local_capacity_exhausted` means "no helper was asked", which is why
    // completion may reschedule on it silently. This pins the half of that
    // contract a test can pin deterministically: a pass that reached helpers
    // reports their answer.
    //
    // The mixed wave — one helper refusing while another is starved of a POST
    // permit — is the case `helper_answered` exists for, and it is not covered
    // here: making one wave member hold the last permit past the fan-out
    // deadline while another answers is not reproducible with this harness,
    // because the admitted member's own POST timeout releases the permit in
    // time for its wave-mate. `a_share_this_process_never_got_to_send_waits_instead_of_failing`
    // covers the classifier's side of the same rule.
    let db = db_with_recoverable_vote();
    let configured = helpers(2);
    let transport = Arc::new(MockTransport::default());
    for helper_url in &configured {
        // A definite, non-ambiguous refusal: the helper answered, and said no.
        transport.queue_post(
            &format!("{helper_url}/shielded-vote/v1/shares"),
            http_status(400),
        );
    }
    let client = client_with(transport.clone());
    let request = InitialShareSubmissionParams {
        target_count: 2,
        ..initial_submission(&configured)
    };

    let report = submit_share_to_helpers(&db, &client, &request, &never_cancel())
        .await
        .unwrap();

    assert!(!transport.calls().is_empty(), "helpers were reached");
    assert!(report.accepted_urls.is_empty());
    assert!(report.ambiguous_urls.is_empty());
    assert!(
        !report.local_capacity_exhausted,
        "a pass that reached a helper reports the helper's answer, not the queue"
    );
}

#[tokio::test(start_paused = true)]
async fn a_stale_wave_member_does_not_discard_a_sibling_helper_acceptance() {
    // `join_all` runs every wave member to completion, so by the time outcomes
    // are applied each answer is one this process observed. Returning at the
    // first stale member left the rest of the wave in `attempting_urls`, so a
    // helper that answered `queued` was re-POSTed as an interrupted attempt on
    // the next pass and the acceptance was thrown away.
    let db = db_with_recoverable_vote();
    let configured = helpers(2);
    let capacity_deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    let mut held_permits = Vec::new();
    for _ in 0..crate::share_policy::SHARE_HELPER_MAX_CONCURRENT_POSTS {
        held_permits.push(
            super::super::post_capacity::acquire(capacity_deadline, &never_cancel())
                .await
                .unwrap(),
        );
    }
    let transport = Arc::new(MockTransport::default());
    for helper in &configured {
        transport.queue_post(
            &format!("{helper}/shielded-vote/v1/shares"),
            json_status("queued"),
        );
    }
    let client = client_with(transport.clone());
    let request = InitialShareSubmissionParams {
        target_count: 2,
        ..initial_submission(&configured)
    };
    // Drop only the first helper's reservation, leaving the second's intact.
    let surviving = configured[1].clone();
    let strand_first_helper = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(only_share(&db).attempting_urls, configured);
        db.conn()
            .execute(
                "UPDATE share_delegations SET attempting_urls = json_array(?3)
                  WHERE round_id = ?1 AND wallet_id = ?2
                    AND bundle_index = 0 AND proposal_id = 1 AND share_index = 0",
                rusqlite::params![ROUND_ID, WALLET_ID, surviving],
            )
            .unwrap();
        drop(held_permits);
    };
    let cancel = never_cancel();
    let (delivery, ()) = tokio::join!(
        submit_share_to_helpers(&db, &client, &request, &cancel),
        strand_first_helper,
    );

    // The stranded member still fails the call: its reservation is gone, and
    // that is exactly the state the caller must be told about.
    let error = delivery.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("committed share changed while helper delivery was in flight"),
        "{error}"
    );
    // But the helper that answered is durably accepted, not left mid-attempt.
    let stored = only_share(&db);
    assert_eq!(
        stored.sent_to_urls,
        vec![surviving.clone()],
        "an observed acceptance survives a stale wave-mate"
    );
    assert!(
        !stored.attempting_urls.contains(&surviving),
        "the accepted helper is no longer mid-attempt"
    );
}
