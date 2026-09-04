//! `RoundStepFailureView`: the delivery reports a failed step accumulated
//! survive the wire, and a payload without them still parses.

use crate::{
    share_tracking::{
        ShareBatchDeliveryReport, ShareDeliveryOutcome, SharePlacementGuarantee,
        ShareSubmissionReport,
    },
    wire::RoundStepFailureView,
    RoundStepFailure, RoundStepFailureKind, VoteRecoveryKey, VoteShareDeliveryReport,
};

fn partial_delivery() -> VoteShareDeliveryReport {
    VoteShareDeliveryReport {
        vote: VoteRecoveryKey {
            bundle_index: 0,
            proposal_id: 7,
        },
        delivery: ShareBatchDeliveryReport {
            deliveries: vec![ShareDeliveryOutcome {
                share_index: 0,
                submission: ShareSubmissionReport {
                    accepted_urls: vec!["https://helper-a.example".to_string()],
                    ambiguous_urls: vec!["https://helper-b.example".to_string()],
                    target_count: 2,
                },
            }],
            pending_share_indices: vec![1],
            cancelled: false,
            placement_guarantee: SharePlacementGuarantee::LegacyBestEffort,
        },
    }
}

#[test]
fn a_failed_step_keeps_its_partial_delivery_reports_on_the_wire() {
    let failure = RoundStepFailure {
        kind: RoundStepFailureKind::HelperDeliveryIncomplete,
        step: None,
        strongest_chain_state: None,
        chain_outcome: None,
        message: "helper delivery ended with pending shares".to_string(),
        plan: None,
        share_deliveries: Vec::new(),
    }
    .with_share_deliveries(vec![partial_delivery()]);

    let view = RoundStepFailureView::try_from(failure).unwrap();
    let json = serde_json::to_string(&view).unwrap();
    let decoded: RoundStepFailureView = serde_json::from_str(&json).unwrap();

    assert_eq!(decoded.share_deliveries.len(), 1);
    let report = &decoded.share_deliveries[0];
    assert_eq!(report.pending_share_indices, vec![1]);
    assert_eq!(
        report.deliveries[0].accepted_urls,
        vec!["https://helper-a.example".to_string()]
    );
    assert_eq!(
        report.deliveries[0].ambiguous_urls,
        vec!["https://helper-b.example".to_string()]
    );
}

#[test]
fn a_failure_payload_without_delivery_reports_still_parses() {
    let json = r#"{"kind":"helper_delivery_incomplete","step":null,"strongest_chain_state":null,"chain_outcome":null,"message":"pending","plan":null}"#;
    let decoded: RoundStepFailureView = serde_json::from_str(json).unwrap();
    assert!(decoded.share_deliveries.is_empty());
}
