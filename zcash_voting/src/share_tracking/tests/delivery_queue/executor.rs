use super::super::observability::{atomic_delivery_executor, DeliveryHost};
use super::{fixtures::*, *};
use crate::{RoundHostSource, RoundStepDisposition, RoundStepFailureKind};

#[tokio::test(start_paused = true)]
async fn round_driver_refills_across_confirmed_members_and_retains_every_report() {
    let db = Arc::new(db_with_round_and_bundle());
    let transport = ScriptedTransport::new({
        let db = db.clone();
        move |wire| {
            // Every share sees atomic confirmation and its own durable reservation.
            let confirmed: i64 = db
                .conn()
                .query_row(
                    "SELECT COUNT(*) FROM votes WHERE vc_tree_position IS NOT NULL",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(confirmed, 2);
            let rows = share::list(&db, ROUND_ID).unwrap();
            let row = rows
                .iter()
                .find(|row| {
                    row.proposal_id == wire.proposal_id && row.share_index == wire.share_index
                })
                .unwrap();
            assert_eq!(row.attempting_urls, helpers(1));
            ReplyPlan {
                delay: if wire.proposal_id == 1 && wire.share_index == 0 {
                    Duration::from_secs(5)
                } else {
                    Duration::ZERO
                },
                ..Default::default()
            }
        }
    });
    let executor = atomic_delivery_executor(db.clone(), transport.clone());
    let report = crate::RoundDriver::new(&executor)
        .with_policy(crate::RoundDrivePolicy {
            max_dispatches: 1,
            ..Default::default()
        })
        .run(
            &DeliveryHost,
            &crate::ChainSubmissionControl::new(1),
            &crate::NoopRoundDriveReporter::default(),
        )
        .await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert_eq!(
        report
            .share_deliveries
            .iter()
            .map(|report| report.vote.proposal_id)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(report.chain_outcomes.len(), 1);
    assert_eq!(transport.completed.lock().unwrap().last(), Some(&(1, 0)));
    assert_eq!(transport.count(), SHARE_COUNT * 2);
    assert!(transport.peak.load(Ordering::SeqCst) <= 16);
}

#[tokio::test(start_paused = true)]
async fn round_completion_folds_all_proposals_before_deciding_disposition() {
    for (statuses, expected) in [
        ([200, 200], Ok(RoundStepDisposition::Advanced)),
        ([503, 200], Ok(RoundStepDisposition::Pending)),
        ([200, 503], Ok(RoundStepDisposition::Pending)),
        (
            [503, 400],
            Err(RoundStepFailureKind::HelperDeliveryIncomplete),
        ),
        (
            [400, 503],
            Err(RoundStepFailureKind::HelperDeliveryIncomplete),
        ),
    ] {
        let transport = ScriptedTransport::new(move |wire| ReplyPlan {
            status: statuses[wire.proposal_id as usize - 1],
            ..Default::default()
        });
        let executor =
            atomic_delivery_executor(Arc::new(db_with_round_and_bundle()), transport.clone());
        let step = executor.plan().unwrap().next_steps[0].clone();
        let control = crate::ChainSubmissionControl::new(1);
        let result = executor
            .advance_step_in_epoch(
                step,
                &DeliveryHost.host_context(),
                &control,
                control.operation_epoch(),
                &crate::NoopRoundStepProgressReporter {},
            )
            .await;
        match (result, expected) {
            (Ok(report), Ok(disposition)) => {
                assert_eq!(report.disposition, disposition);
                assert_eq!(report.share_deliveries.len(), 2);
                assert!(report.chain_outcome.is_some());
            }
            (Err(failure), Err(kind)) => {
                assert_eq!(failure.kind, kind);
                assert_eq!(failure.share_deliveries.len(), 2);
                assert!(failure.chain_outcome.is_some());
            }
            (actual, expected) => panic!("expected {expected:?}, got {actual:?}"),
        }
        assert_eq!(transport.count(), SHARE_COUNT * 2);
    }
}

#[tokio::test(start_paused = true)]
async fn round_failure_retains_confirmation_and_successes_from_later_proposals() {
    let db = Arc::new(db_with_round_and_bundle());
    db.conn()
        .execute_batch(
            "CREATE TRIGGER fail_round_delivery BEFORE UPDATE OF sent_to_urls ON share_delegations
         WHEN NEW.proposal_id = 1 AND NEW.share_index = 0 AND NEW.sent_to_urls != '[]'
         BEGIN SELECT RAISE(FAIL, 'round delivery journal failed'); END;",
        )
        .unwrap();
    let transport = ScriptedTransport::new(|wire| ReplyPlan {
        delay: if wire.proposal_id == 1 && wire.share_index == 0 {
            Duration::from_secs(5)
        } else {
            Duration::ZERO
        },
        ..Default::default()
    });
    let executor = atomic_delivery_executor(db, transport.clone());
    let report = crate::RoundDriver::new(&executor)
        .with_policy(crate::RoundDrivePolicy {
            max_dispatches: 1,
            ..Default::default()
        })
        .run(
            &DeliveryHost,
            &crate::ChainSubmissionControl::new(1),
            &crate::NoopRoundDriveReporter::default(),
        )
        .await;
    assert_eq!(report.failures.len(), 1);
    assert_eq!(report.chain_outcomes.len(), 1);
    assert_eq!(report.share_deliveries.len(), 2);
    assert_eq!(
        report.share_deliveries[0].delivery.pending_share_indices,
        vec![0]
    );
    assert_eq!(
        report.share_deliveries[1].delivery.deliveries.len(),
        SHARE_COUNT
    );
    assert_eq!(transport.count(), SHARE_COUNT * 2);
}

#[tokio::test(start_paused = true)]
async fn hard_error_outranks_callback_cancellation_after_all_durable_effects_are_kept() {
    struct CancelOnReport(crate::ChainSubmissionControl);
    impl crate::RoundStepProgressReporter for CancelOnReport {
        fn report(&self, progress: crate::RoundStepProgress) {
            if matches!(progress, crate::RoundStepProgress::ShareOutcome(_)) {
                self.0.cancel();
            }
        }
    }
    let db = Arc::new(db_with_round_and_bundle());
    db.conn()
        .execute_batch(
            "CREATE TRIGGER fail_slow_share BEFORE UPDATE OF sent_to_urls ON share_delegations
         WHEN NEW.proposal_id = 1 AND NEW.share_index = 0 AND NEW.sent_to_urls != '[]'
         BEGIN SELECT RAISE(FAIL, 'slow share journal failure'); END;",
        )
        .unwrap();
    let transport = ScriptedTransport::new(|wire| ReplyPlan {
        delay: if wire.proposal_id == 1 && wire.share_index == 0 {
            Duration::from_secs(5)
        } else {
            Duration::ZERO
        },
        ..Default::default()
    });
    let executor = atomic_delivery_executor(db, transport.clone());
    let control = crate::ChainSubmissionControl::new(1);
    let failure = executor
        .advance_step_in_epoch(
            executor.plan().unwrap().next_steps[0].clone(),
            &DeliveryHost.host_context(),
            &control,
            control.operation_epoch(),
            &CancelOnReport(control.clone()),
        )
        .await
        .unwrap_err();
    assert!(failure.message.contains("slow share journal failure"));
    assert_eq!(failure.share_deliveries.len(), 2);
    assert!(failure.chain_outcome.is_some());
    assert_eq!(transport.completed.lock().unwrap().len(), SHARE_COUNT * 2);
    assert_eq!(transport.active.load(Ordering::SeqCst), 0);
}
