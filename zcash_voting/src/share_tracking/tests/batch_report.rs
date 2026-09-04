//! `batch_delivery_report`: a failing share does not discard its siblings'
//! results.

use crate::{
    share_tracking::{
        batch_delivery_report, ShareDeliveryOutcome, SharePlacementGuarantee, ShareSubmissionReport,
    },
    VotingError,
};

fn accepted(share_index: u32) -> ShareDeliveryOutcome {
    ShareDeliveryOutcome {
        share_index,
        submission: ShareSubmissionReport {
            accepted_urls: vec![format!("https://helper-{share_index}.example")],
            ambiguous_urls: Vec::new(),
            target_count: 1,
        },
    }
}

#[test]
fn a_failed_share_keeps_the_completed_siblings_in_the_partial_report() {
    let results = vec![
        Ok(Some(accepted(2))),
        Err(VotingError::Storage {
            message: "disk full".to_string(),
        }),
        Ok(Some(accepted(0))),
        Ok(None),
    ];

    let failure = batch_delivery_report(results, 0..4, false, SharePlacementGuarantee::Strict)
        .expect_err("the storage error is reported");

    assert!(matches!(failure.error, VotingError::Storage { .. }));
    let partial = failure
        .partial
        .expect("siblings completed before the error");
    assert_eq!(
        partial
            .deliveries
            .iter()
            .map(|delivery| delivery.share_index)
            .collect::<Vec<_>>(),
        vec![0, 2],
        "completed shares are kept, in index order"
    );
    assert_eq!(partial.pending_share_indices, vec![1, 3]);
    assert!(!partial.cancelled);
}

#[test]
fn a_pass_without_errors_reports_every_unprocessed_share_pending() {
    let report = batch_delivery_report(
        vec![Ok(Some(accepted(1))), Ok(None)],
        0..3,
        true,
        SharePlacementGuarantee::LegacyBestEffort,
    )
    .unwrap();
    assert_eq!(report.pending_share_indices, vec![0, 2]);
    assert!(report.cancelled);
    assert_eq!(
        report.placement_guarantee,
        SharePlacementGuarantee::LegacyBestEffort
    );
}
