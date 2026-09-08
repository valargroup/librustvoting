use super::*;
use crate::{ObservabilityOptions, ObservationOutcome};

#[tokio::test]
async fn enabled_reports_distinguish_cancelled_and_ambiguous_submissions() {
    let transport = Arc::new(MockTransport::default());
    let client = client_with(transport.clone());
    let cancelled = client
        .submit_share_with_report(
            helper(),
            &valid_share_json(),
            10,
            &|| true,
            Some(ObservabilityOptions::default()),
        )
        .await;
    assert!(matches!(cancelled.result, Err(HelperError::Cancelled)));
    assert_eq!(
        cancelled.observability.unwrap().outcome,
        ObservationOutcome::Cancelled
    );
    assert_eq!(transport.call_count(&post_url()), 0);

    transport.queue_post(
        &post_url(),
        Ok(HelperResponse::json(200, b"not json".to_vec())),
    );
    let ambiguous = client
        .submit_share_with_report(
            helper(),
            &valid_share_json(),
            10,
            &never_cancel(),
            Some(ObservabilityOptions::default()),
        )
        .await;
    assert!(ambiguous.result.unwrap_err().is_ambiguous());
    let diagnostics = ambiguous.observability.unwrap();
    assert_eq!(diagnostics.outcome, ObservationOutcome::PossiblyDispatched);
    assert_eq!(
        diagnostics.records[0].outcome,
        ObservationOutcome::PossiblyDispatched
    );
    assert_eq!(transport.call_count(&post_url()), 1);
}

#[tokio::test]
async fn enabled_reports_keep_helper_status_pending() {
    let transport = Arc::new(MockTransport::default());
    transport.queue_post(&post_url(), json_status("queued"));
    let client = client_with(transport);
    let report = client
        .submit_share_with_report(
            helper(),
            &valid_share_json(),
            10,
            &never_cancel(),
            Some(ObservabilityOptions::default()),
        )
        .await;
    assert_eq!(report.result.unwrap(), ShareSubmissionStatus::Queued);
    assert_eq!(
        report.observability.unwrap().outcome,
        ObservationOutcome::Pending
    );
}

#[tokio::test(start_paused = true)]
async fn reported_retry_numbers_real_http_attempts() {
    let transport = Arc::new(MockTransport::default());
    transport.queue_post(&post_url(), http_status(429));
    transport.queue_post(&post_url(), json_status("queued"));
    let report = client_with(transport.clone())
        .submit_share_with_report(
            helper(),
            &valid_share_json(),
            10,
            &never_cancel(),
            Some(ObservabilityOptions::default()),
        )
        .await;
    assert_eq!(report.result.unwrap(), ShareSubmissionStatus::Queued);
    let diagnostics = report.observability.unwrap();
    let attempts: Vec<_> = diagnostics
        .records
        .iter()
        .filter(|record| record.stage.as_ref() == "helper.http.post_json")
        .map(|record| (record.attempt, record.http_status))
        .collect();
    assert_eq!(attempts, vec![(Some(1), Some(429)), (Some(2), Some(200))]);
    assert_eq!(transport.call_count(&post_url()), 2);
}

#[tokio::test]
async fn concurrent_reported_calls_are_independent_and_preserve_plain_results() {
    let transport = Arc::new(MockTransport::default());
    for _ in 0..4 {
        transport.queue_post(&post_url(), json_status("queued"));
    }
    let client = client_with(transport.clone());
    let payload = valid_share_json();
    let cancel = never_cancel();
    let plain = client
        .submit_share(helper(), &payload, 10, &cancel)
        .await
        .unwrap();
    let disabled = client
        .submit_share_with_report(helper(), &payload, 10, &cancel, None)
        .await;
    assert_eq!(disabled.result.unwrap(), plain);
    assert!(disabled.observability.is_none());
    let (first, second) = tokio::join!(
        client.submit_share_with_report(
            helper(),
            &payload,
            10,
            &cancel,
            Some(ObservabilityOptions::default())
        ),
        client.submit_share_with_report(
            helper(),
            &payload,
            10,
            &cancel,
            Some(ObservabilityOptions::default())
        ),
    );
    for report in [first, second] {
        assert_eq!(report.result.unwrap(), plain);
        let diagnostics = report.observability.unwrap();
        assert_eq!(
            diagnostics
                .records
                .iter()
                .filter(|record| record.stage.as_ref() == "helper.http.post_json")
                .count(),
            1
        );
        assert!(diagnostics
            .records
            .iter()
            .all(|record| record.outcome != ObservationOutcome::Unfinished));
    }
    assert_eq!(transport.call_count(&post_url()), 4);
}
