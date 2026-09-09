use super::{fixtures::*, *};
use std::sync::atomic::AtomicBool;
use tokio::sync::Semaphore;

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
            transport.wait_for(16).await;
            if change_epoch {
                control.set_operation_epoch(entry_epoch + 1);
            } else {
                control.cancel();
            }
            gate.add_permits(16);
        };
        let (reports, ()) = tokio::join!(fixture.deliver(transport.clone(), &cancel), interrupt);
        assert_eq!(transport.count(), 16);
        assert_eq!(transport.active.load(Ordering::SeqCst), 0);
        let mut reports = reports.into_iter();
        let first = reports.next().unwrap().unwrap();
        assert!(first.cancelled);
        assert_eq!(first.deliveries.len(), 16);
        assert!(first
            .deliveries
            .iter()
            .all(|share| share.submission.accepted_urls.len() == 1));
        for report in reports {
            let report = report.unwrap();
            assert!(report.cancelled);
            assert_eq!(report.pending_share_indices.len(), SHARE_COUNT);
        }
        // A subsequent pass obtains all slots and only sends the unsent work.
        let resumed = ScriptedTransport::new(|_| ReplyPlan::default());
        assert_complete(fixture.deliver(resumed.clone(), &uncancelled).await, 3);
        assert_eq!(resumed.count(), SHARE_COUNT * 2);
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
