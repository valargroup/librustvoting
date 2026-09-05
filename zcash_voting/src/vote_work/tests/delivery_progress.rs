//! A delivery report decides whether a step advanced, must wait for
//! tracking, or failed to place a share.

use crate::{
    share_tracking::{
        ShareBatchDeliveryReport, ShareDeliveryOutcome, SharePlacementGuarantee,
        ShareSubmissionReport,
    },
    vote_work::vote_completion::{delivery_progress, DeliveryProgress},
};

fn report(outcomes: &[(&[&str], &[&str])], pending: &[u32]) -> ShareBatchDeliveryReport {
    ShareBatchDeliveryReport {
        deliveries: outcomes
            .iter()
            .enumerate()
            .map(
                |(share_index, (accepted, ambiguous))| ShareDeliveryOutcome {
                    share_index: share_index as u32,
                    submission: ShareSubmissionReport {
                        accepted_urls: accepted.iter().map(|url| url.to_string()).collect(),
                        ambiguous_urls: ambiguous.iter().map(|url| url.to_string()).collect(),
                        target_count: 2,
                    },
                },
            )
            .collect(),
        pending_share_indices: pending.to_vec(),
        cancelled: false,
        placement_guarantee: SharePlacementGuarantee::Strict,
    }
}

#[test]
fn a_share_every_helper_answered_ambiguously_waits_for_tracking_rather_than_advancing() {
    assert_eq!(
        delivery_progress(&report(&[(&["a"], &[]), (&["a"], &["b"])], &[])),
        DeliveryProgress::Complete
    );
    assert_eq!(
        delivery_progress(&report(&[(&["a"], &[]), (&[], &["b"])], &[])),
        DeliveryProgress::AwaitingAmbiguousHelpers,
        "an ambiguous-only share is not a placement the step can repeat"
    );
    assert_eq!(
        delivery_progress(&report(&[(&["a"], &[]), (&[], &[])], &[])),
        DeliveryProgress::Incomplete
    );
    assert_eq!(
        delivery_progress(&report(&[(&["a"], &[])], &[1])),
        DeliveryProgress::Incomplete
    );
}
