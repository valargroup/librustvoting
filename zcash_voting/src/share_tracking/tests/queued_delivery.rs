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
