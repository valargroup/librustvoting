use super::fixtures::*;
use crate::{ObservabilityOptions, ObservationOutcome};

#[tokio::test(start_paused = true)]
async fn reported_tracking_waits_preserve_cadence_and_cancellation() {
    for cancel in [false, true] {
        let db = db_with_pending_share(60);
        let host = ScriptedHost::fixed(Some(VOTE_END));
        let control = ChainSubmissionControl::new(1);
        let client = client();
        let events = RecordingReporter::default();
        let driver = ShareTrackingDriver::new(&db, &client, ROUND_ID).with_policy(
            ShareTrackingDrivePolicy {
                max_passes: Some(2),
                ..Default::default()
            },
        );
        let run = driver.run_with_report(
            &host,
            &control,
            &events,
            Some(ObservabilityOptions::default()),
        );
        tokio::pin!(run);
        if cancel {
            assert!(tokio::time::timeout(Duration::from_millis(1), &mut run)
                .await
                .is_err());
            control.cancel();
        }
        let report = run.await;
        assert_eq!(report.result.passes, if cancel { 1 } else { 2 });
        assert_eq!(events.delays(), vec![Duration::from_secs(30)]);
        let diagnostics = report.observability.unwrap();
        let waits = diagnostics
            .records
            .iter()
            .filter(|record| record.stage.as_ref() == "helper::tracking_wait")
            .collect::<Vec<_>>();
        assert_eq!(waits.len(), 1);
        assert_eq!(
            waits[0].outcome,
            if cancel {
                ObservationOutcome::Cancelled
            } else {
                ObservationOutcome::Succeeded
            }
        );
        // Observation clocks are real monotonic clocks, independently of this
        // test's paused Tokio scheduling clock. Assert nesting, not 30s elapsed.
        let parent = diagnostics
            .records
            .iter()
            .find(|record| Some(record.id) == waits[0].parent_id)
            .unwrap();
        assert_eq!(parent.stage.as_ref(), "helper::tracking_run");
        assert!(
            waits[0].started_after_us + waits[0].elapsed_us
                <= parent.started_after_us + parent.elapsed_us
        );
    }
}
