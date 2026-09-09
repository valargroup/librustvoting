use super::{fixtures::*, *};
use std::sync::atomic::AtomicBool;
use tokio::sync::Semaphore;

#[tokio::test(start_paused = true)]
async fn mixed_fleet_admission_cancellation_releases_the_full_charge() {
    let small_fleet = Fixture::new(1);
    let large_fleet = Fixture::with_helpers(2, 20);
    let gate = Arc::new(Semaphore::new(0));
    let gated_transport = || {
        ScriptedTransport::new({
            let gate = gate.clone();
            move |_| ReplyPlan {
                gate: Some(gate.clone()),
                ..Default::default()
            }
        })
    };
    let small_transport = gated_transport();
    let large_transport = gated_transport();
    let cancelled = AtomicBool::new(false);
    let cancel = || cancelled.load(Ordering::SeqCst);
    let run_large = async {
        // Sixteen one-target shares charge 64 units. Only six ten-target
        // shares fit alongside them, despite spare physical POST slots.
        small_transport.wait_for(16).await;
        large_fleet.deliver(large_transport.clone(), &cancel).await
    };
    let interrupt = async {
        large_transport.wait_for(60).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(small_transport.count(), 16);
        assert_eq!(large_transport.count(), 60);
        assert_eq!(share::list(&large_fleet.db, ROUND_ID).unwrap().len(), 6);
        cancelled.store(true, Ordering::SeqCst);
        gate.add_permits(76);
    };
    let (small_reports, large_reports, ()) = tokio::join!(
        small_fleet.deliver(small_transport.clone(), &uncancelled),
        run_large,
        interrupt,
    );
    assert_complete(small_reports, 1);
    assert_eq!(large_transport.count(), 60);
    assert_eq!(large_transport.active.load(Ordering::SeqCst), 0);
    let large_reports = large_reports
        .into_iter()
        .map(Result::unwrap)
        .collect::<Vec<_>>();
    assert!(large_reports.iter().all(|report| report.cancelled));
    assert_eq!(
        large_reports
            .iter()
            .map(|report| report.deliveries.len())
            .sum::<usize>(),
        6
    );
    assert_eq!(
        large_reports
            .iter()
            .map(|report| report.pending_share_indices.len())
            .sum::<usize>(),
        26
    );
    assert!(share::list(&large_fleet.db, ROUND_ID)
        .unwrap()
        .iter()
        .all(|share| share.sent_to_urls.len() == 10
            && share.attempting_urls.is_empty()
            && share.ambiguous_urls.is_empty()));

    // Resumption uses the entire budget again and sends only the 26 unsent shares.
    let resumed = ScriptedTransport::new(|_| ReplyPlan {
        delay: Duration::from_secs(1),
        ..Default::default()
    });
    let reports = large_fleet.deliver(resumed.clone(), &uncancelled).await;
    assert_eq!(resumed.peak.load(Ordering::SeqCst), 120);
    assert_eq!(resumed.count(), 260);
    for report in reports {
        let report = report.unwrap();
        assert!(!report.cancelled);
        assert!(report.pending_share_indices.is_empty());
        assert_eq!(report.deliveries.len(), SHARE_COUNT);
        assert!(report
            .deliveries
            .iter()
            .all(|delivery| delivery.submission.accepted_urls.len() == 10
                && delivery.submission.ambiguous_urls.is_empty()));
    }
}

#[tokio::test(start_paused = true)]
async fn cancellation_before_admission_keeps_every_share_pending() {
    let fixture = Fixture::new(3);
    let transport = ScriptedTransport::new(|_| ReplyPlan::default());
    let reports = fixture.deliver(transport.clone(), &|| true).await;
    assert_eq!(transport.count(), 0);
    assert!(share::list(&fixture.db, ROUND_ID).unwrap().is_empty());
    for report in reports {
        let report = report.unwrap();
        assert!(report.cancelled);
        assert!(report.deliveries.is_empty());
        assert_eq!(
            report.pending_share_indices,
            (0..SHARE_COUNT as u32).collect::<Vec<_>>()
        );
    }
    assert_complete(fixture.deliver(transport, &uncancelled).await, 3);
}

#[tokio::test(start_paused = true)]
async fn cancellation_and_epoch_changes_drain_live_posts_and_release_capacity() {
    for change_epoch in [false, true] {
        let fixture = Fixture::new(3);
        let control = crate::ChainSubmissionControl::new(1);
        let entry_epoch = control.operation_epoch();
        let gate = Arc::new(Semaphore::new(0));
        let transport = ScriptedTransport::new({
            let gate = gate.clone();
            move |_| ReplyPlan {
                gate: Some(gate.clone()),
                ..Default::default()
            }
        });
        let cancel = || control.is_cancelled() || control.operation_epoch() != entry_epoch;
        let interrupt = async {
            transport.wait_for(32).await;
            if change_epoch {
                control.set_operation_epoch(entry_epoch + 1);
            } else {
                control.cancel();
            }
            gate.add_permits(32);
        };
        let (reports, ()) = tokio::join!(fixture.deliver(transport.clone(), &cancel), interrupt);
        assert_eq!(transport.count(), 32);
        assert_eq!(transport.active.load(Ordering::SeqCst), 0);
        let mut reports = reports.into_iter();
        for _ in 0..2 {
            let first = reports.next().unwrap().unwrap();
            assert!(first.cancelled);
            assert_eq!(first.deliveries.len(), 16);
            assert!(first
                .deliveries
                .iter()
                .all(|share| share.submission.accepted_urls.len() == 1));
        }
        for report in reports {
            let report = report.unwrap();
            assert!(report.cancelled);
            assert_eq!(report.pending_share_indices.len(), SHARE_COUNT);
        }
        // A subsequent pass obtains all slots and only sends the unsent work.
        let resumed = ScriptedTransport::new(|_| ReplyPlan::default());
        assert_complete(fixture.deliver(resumed.clone(), &uncancelled).await, 3);
        assert_eq!(resumed.count(), SHARE_COUNT);
    }
}

#[tokio::test(start_paused = true)]
async fn cancellation_from_report_callback_stops_further_admission() {
    let fixture = Fixture::new(3);
    let cancelled = AtomicBool::new(false);
    let transport = ScriptedTransport::new(|_| ReplyPlan::default());
    let mut reported = Vec::new();
    let results = crate::vote::submit_confirmed_vote_shares(
        &fixture.votes,
        &fixture.db,
        &HelperClient::new(transport.clone(), HelperHealth::default()),
        ShareDeliverySubmissionParams {
            configured_server_urls: &fixture.configured,
            now_seconds: SUBMIT_AT,
        },
        &|| cancelled.load(Ordering::SeqCst),
        &mut |vote, _| {
            reported.push(vote.proposal_id());
            cancelled.store(true, Ordering::SeqCst);
        },
    )
    .await;
    assert_eq!(reported.len(), 3);
    assert_eq!(results.len(), 3);
    assert!(transport.count() < 3 * SHARE_COUNT);
    assert!(results.last().unwrap().delivery.as_ref().unwrap().cancelled);
    assert_eq!(transport.active.load(Ordering::SeqCst), 0);
}
