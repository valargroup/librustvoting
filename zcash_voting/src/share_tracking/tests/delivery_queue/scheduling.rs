use super::{fixtures::*, *};
use tokio::sync::Semaphore;

#[tokio::test(start_paused = true)]
async fn completed_slots_refill_across_three_proposals_without_a_barrier() {
    let fixture = Fixture::new(3);
    let gate = Arc::new(Semaphore::new(0));
    let slow = Arc::new(Semaphore::new(0));
    let transport = ScriptedTransport::new({
        let gate = gate.clone();
        let slow = slow.clone();
        move |wire| ReplyPlan {
            gate: Some(if wire.proposal_id == 1 && wire.share_index == 0 {
                slow.clone()
            } else {
                gate.clone()
            }),
            ..Default::default()
        }
    });
    let observe = async {
        transport.wait_for(SHARE_COUNT).await;
        assert_eq!(transport.active.load(Ordering::SeqCst), 16);
        for admitted in SHARE_COUNT + 1..=SHARE_COUNT * 3 {
            gate.add_permits(1);
            transport.wait_for(admitted).await;
            assert_eq!(transport.count(), admitted);
            assert!(!transport.completed.lock().unwrap().contains(&(1, 0)));
            assert!(transport.peak.load(Ordering::SeqCst) <= 16);
        }
        gate.add_permits(SHARE_COUNT);
        slow.add_permits(1);
    };
    let (reports, ()) = tokio::join!(fixture.deliver(transport.clone(), &uncancelled), observe);
    assert_complete(reports, 3);
    assert_eq!(transport.count(), SHARE_COUNT * 3);
    assert_eq!(transport.active.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn batch_and_singleton_calls_share_the_process_wide_sixteen_slots() {
    let batch = Fixture::new(2);
    let singleton = Fixture::new(1);
    let gate = Arc::new(Semaphore::new(0));
    let transport = ScriptedTransport::new({
        let gate = gate.clone();
        move |_| ReplyPlan {
            gate: Some(gate.clone()),
            ..Default::default()
        }
    });
    let client = HelperClient::new(transport.clone(), HelperHealth::default());
    let run_singleton = async {
        transport.wait_for(16).await;
        singleton.votes[0]
            .submit_prepared_shares(
                &singleton.db,
                &client,
                ShareDeliverySubmissionParams {
                    configured_server_urls: &singleton.configured,
                    now_seconds: SUBMIT_AT,
                },
                &uncancelled,
            )
            .await
            .unwrap()
    };
    let observe = async {
        transport.wait_for(16).await;
        // Give the independent caller time to queue its permits while the
        // first batch still owns every slot.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(transport.count(), 16);
        gate.add_permits(1);
        transport.wait_for(17).await;
        assert_eq!(transport.count(), 17);
        gate.add_permits(3 * SHARE_COUNT);
    };
    let (batch_reports, single_report, ()) = tokio::join!(
        batch.deliver(transport.clone(), &uncancelled),
        run_singleton,
        observe,
    );
    assert_complete(batch_reports, 2);
    assert_complete(vec![Ok(single_report)], 1);
    assert_eq!(transport.peak.load(Ordering::SeqCst), 16);
}

#[tokio::test(start_paused = true)]
async fn thirty_seven_proposals_finish_faster_with_identical_durable_results() {
    async fn measure(
        sequential: bool,
    ) -> (
        Duration,
        Vec<ShareBatchDeliveryReport>,
        Vec<(u32, u32, Vec<String>, u64)>,
        usize,
    ) {
        let fixture = Fixture::new(37);
        let transport = ScriptedTransport::new(|wire| ReplyPlan {
            delay: if wire.share_index == 0 {
                Duration::from_secs(2)
            } else {
                Duration::from_millis(100)
            },
            ..Default::default()
        });
        let start = tokio::time::Instant::now();
        let reports = if sequential {
            let client = HelperClient::new(transport.clone(), HelperHealth::default());
            let mut reports = Vec::new();
            for vote in &fixture.votes {
                reports.push(
                    vote.submit_prepared_shares(
                        &fixture.db,
                        &client,
                        ShareDeliverySubmissionParams {
                            configured_server_urls: &fixture.configured,
                            now_seconds: SUBMIT_AT,
                        },
                        &uncancelled,
                    )
                    .await
                    .unwrap(),
                );
            }
            reports
        } else {
            fixture
                .deliver(transport.clone(), &uncancelled)
                .await
                .into_iter()
                .map(Result::unwrap)
                .collect()
        };
        let elapsed = start.elapsed();
        let durable = share::list(&fixture.db, ROUND_ID)
            .unwrap()
            .into_iter()
            .map(|share| {
                (
                    share.proposal_id,
                    share.share_index,
                    share.sent_to_urls,
                    share.submit_at,
                )
            })
            .collect();
        (
            elapsed,
            reports,
            durable,
            transport.peak.load(Ordering::SeqCst),
        )
    }
    let (sequential, old_reports, old_durable, old_peak) = measure(true).await;
    let (queued, reports, durable, peak) = measure(false).await;
    assert_eq!(reports, old_reports);
    assert_eq!(durable, old_durable);
    assert_eq!(peak, old_peak);
    assert_eq!(peak, 16);
    assert!(
        queued < sequential / 2,
        "queued {queued:?}, sequential {sequential:?}"
    );
}
