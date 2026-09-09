//! A delivery report decides whether a step advanced, must wait for
//! tracking, or failed to place a share.

use crate::share_tracking::{
    delivery_progress, DeliveryProgress, ShareBatchDeliveryReport, ShareDeliveryOutcome,
    SharePlacementGuarantee, ShareSubmissionReport,
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
                        local_capacity_exhausted: false,
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

/// Marks share `index` as having placed nothing because local admission
/// expired, rather than because a helper answered.
fn throttled(mut report: ShareBatchDeliveryReport, index: usize) -> ShareBatchDeliveryReport {
    report.deliveries[index].submission.local_capacity_exhausted = true;
    report
}

#[test]
fn a_share_this_process_never_got_to_send_waits_instead_of_failing() {
    // Nothing was POSTed, so no helper refused anything and every durable row
    // is clean. Reporting that as an incomplete delivery turned this SDK's own
    // POST admission queue into a hard step failure the voter had to see.
    assert_eq!(
        delivery_progress(&throttled(report(&[(&["a"], &[]), (&[], &[])], &[]), 1)),
        DeliveryProgress::AwaitingLocalCapacity
    );
    assert_eq!(
        delivery_progress(&throttled(report(&[(&[], &[]), (&[], &[])], &[]), 1)),
        DeliveryProgress::Incomplete,
        "a share that placed nothing after really asking is still a failure"
    );
    assert_eq!(
        delivery_progress(&throttled(report(&[(&["a"], &[])], &[1]), 0)),
        DeliveryProgress::Incomplete,
        "an unfinished share is incomplete whatever throttled its siblings"
    );
}
