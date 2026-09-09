use super::{fixtures::*, *};

#[tokio::test(start_paused = true)]
async fn invalid_proposal_plan_does_not_send_that_proposal_or_block_later_ones() {
    let fixture = Fixture::new(3);
    fixture
        .db
        .conn()
        .execute("DELETE FROM helper_share_plans WHERE proposal_id = 2", [])
        .unwrap();
    let transport = ScriptedTransport::new(|_| ReplyPlan::default());
    let mut reports = fixture.deliver(transport.clone(), &uncancelled).await;
    assert!(reports.remove(1).unwrap_err().partial.is_none());
    assert_complete(reports, 2);
    assert_eq!(transport.count(), SHARE_COUNT * 2);
    assert!(transport
        .started
        .lock()
        .unwrap()
        .iter()
        .all(|wire| wire.proposal_id != 2));
}

#[tokio::test(start_paused = true)]
async fn reservation_failure_keeps_siblings_and_later_proposals() {
    let fixture = Fixture::new(3);
    fixture.db.conn().execute_batch(
        "CREATE TRIGGER fail_queue_reservation BEFORE UPDATE OF attempting_urls ON share_delegations
         WHEN NEW.proposal_id = 1 AND NEW.share_index = 0 AND NEW.attempting_urls != '[]'
         BEGIN SELECT RAISE(FAIL, 'queue reservation failed'); END;"
    ).unwrap();
    let transport = ScriptedTransport::new(|_| ReplyPlan::default());
    let mut reports = fixture.deliver(transport.clone(), &uncancelled).await;
    let failure = reports.remove(0).unwrap_err();
    assert!(failure
        .error
        .to_string()
        .contains("queue reservation failed"));
    let partial = failure.partial.unwrap();
    assert_eq!(partial.pending_share_indices, vec![0]);
    assert_eq!(partial.deliveries.len(), SHARE_COUNT - 1);
    assert_complete(reports, 2);
    assert_eq!(transport.count(), SHARE_COUNT * 3 - 1);
    assert!(transport
        .started
        .lock()
        .unwrap()
        .iter()
        .all(|wire| (wire.proposal_id, wire.share_index) != (1, 0)));
}

#[tokio::test(start_paused = true)]
async fn outcome_write_failure_keeps_interrupted_marker_and_other_proposals() {
    let fixture = Fixture::new(3);
    fixture
        .db
        .conn()
        .execute_batch(
            "CREATE TRIGGER fail_queue_outcome BEFORE UPDATE OF sent_to_urls ON share_delegations
         WHEN NEW.proposal_id = 1 AND NEW.share_index = 0 AND NEW.sent_to_urls != '[]'
         BEGIN SELECT RAISE(FAIL, 'share zero failed'); END;
         CREATE TRIGGER fail_queue_outcome_one BEFORE UPDATE OF sent_to_urls ON share_delegations
         WHEN NEW.proposal_id = 1 AND NEW.share_index = 1 AND NEW.sent_to_urls != '[]'
         BEGIN SELECT RAISE(FAIL, 'share one failed'); END;",
        )
        .unwrap();
    let transport = ScriptedTransport::new(|wire| ReplyPlan {
        delay: if wire.share_index == 0 {
            Duration::from_secs(1)
        } else {
            Duration::ZERO
        },
        ..Default::default()
    });
    let mut events = Vec::new();
    let results = crate::vote::submit_confirmed_vote_shares(
        &fixture.votes,
        &fixture.db,
        &HelperClient::new(transport.clone(), HelperHealth::default()),
        ShareDeliverySubmissionParams {
            configured_server_urls: &fixture.configured,
            now_seconds: SUBMIT_AT,
        },
        &uncancelled,
        &mut |vote, report| events.push((vote.proposal_id(), report.clone())),
    )
    .await;
    assert_eq!(events.len(), 3);
    assert_eq!(
        results
            .iter()
            .map(|result| result.vote.proposal_id())
            .collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    let mut reports = results
        .into_iter()
        .map(|result| result.delivery)
        .collect::<Vec<_>>();
    let failure = reports.remove(0).unwrap_err();
    assert!(failure.error.to_string().contains("share zero failed"));
    let failed = failure.partial.unwrap();
    assert_eq!(failed.pending_share_indices, vec![0, 1]);
    assert_eq!(failed.deliveries.len(), SHARE_COUNT - 2);
    assert_complete(reports, 2);
    let durable = share::list(&fixture.db, ROUND_ID).unwrap();
    for interrupted in durable
        .iter()
        .filter(|share| share.proposal_id == 1 && share.share_index < 2)
    {
        assert_eq!(interrupted.attempting_urls, fixture.configured);
        assert!(interrupted.sent_to_urls.is_empty());
    }
    fixture
        .db
        .conn()
        .execute_batch("DROP TRIGGER fail_queue_outcome; DROP TRIGGER fail_queue_outcome_one")
        .unwrap();
    let resumed = ScriptedTransport::new(|_| ReplyPlan::default());
    let reports = fixture.deliver(resumed.clone(), &uncancelled).await;
    assert_eq!(
        resumed.count(),
        0,
        "initial delivery must not replay interrupted POSTs"
    );
    assert_eq!(
        reports[0].as_ref().unwrap().deliveries[0]
            .submission
            .ambiguous_urls,
        fixture.configured
    );
}

#[tokio::test(start_paused = true)]
async fn reports_complete_out_of_order_but_return_in_proposal_and_share_order() {
    let fixture = Fixture::new(3);
    let transport = ScriptedTransport::new(|wire| ReplyPlan {
        delay: if wire.proposal_id == 1 && wire.share_index == 0 {
            Duration::from_secs(5)
        } else {
            Duration::ZERO
        },
        status: if wire.proposal_id == 2 {
            503
        } else if wire.proposal_id == 3 {
            400
        } else {
            200
        },
        ..Default::default()
    });
    let mut events = Vec::new();
    let results = crate::vote::submit_confirmed_vote_shares(
        &fixture.votes,
        &fixture.db,
        &HelperClient::new(transport, HelperHealth::default()),
        ShareDeliverySubmissionParams {
            configured_server_urls: &fixture.configured,
            now_seconds: SUBMIT_AT,
        },
        &uncancelled,
        &mut |vote, _| events.push(vote.proposal_id()),
    )
    .await;
    assert_eq!(events, vec![2, 3, 1]);
    let states = results
        .into_iter()
        .map(|result| {
            let report = result.delivery.unwrap();
            assert_eq!(
                report
                    .deliveries
                    .iter()
                    .map(|share| share.share_index)
                    .collect::<Vec<_>>(),
                (0..SHARE_COUNT as u32).collect::<Vec<_>>()
            );
            crate::share_tracking::delivery_progress(&report)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        states,
        vec![
            crate::share_tracking::DeliveryProgress::Complete,
            crate::share_tracking::DeliveryProgress::AwaitingAmbiguousHelpers,
            crate::share_tracking::DeliveryProgress::Incomplete
        ]
    );
}
