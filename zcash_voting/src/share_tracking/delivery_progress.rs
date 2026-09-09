//! Placement evidence from a completed initial-delivery pass.

use super::ShareBatchDeliveryReport;

/// What one vote's helper delivery report says about the shares it covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DeliveryProgress {
    /// Every share reached at least one helper definitely.
    Complete,
    /// Every share reached the helpers, but some only ambiguously: no helper
    /// definitely holds it yet, and tracking must reconcile those attempts
    /// before another delivery can make progress.
    AwaitingAmbiguousHelpers,
    /// A share reached no helper because this process ran out of POST
    /// admission slots, not because a helper answered. Nothing was sent and
    /// nothing was refused, so another pass is the response, exactly as for
    /// ambiguity — the empty result here is about this SDK, not the fleet.
    AwaitingLocalCapacity,
    /// Some share reached no helper at all, or was left pending.
    Incomplete,
}

/// Classifies `report`. Ambiguous attempts are excluded from the next
/// delivery pass, so treating them as complete would let a step report
/// `Advanced` forever without a share ever landing.
pub(crate) fn delivery_progress(report: &ShareBatchDeliveryReport) -> DeliveryProgress {
    let placed_nothing = |delivery: &super::ShareDeliveryOutcome| {
        delivery.submission.accepted_urls.is_empty()
            && delivery.submission.ambiguous_urls.is_empty()
    };
    if !report.pending_share_indices.is_empty() || report.deliveries.iter().any(placed_nothing) {
        // A share that placed nothing is only a failure if the pass actually
        // got to ask. Local admission expiry means it did not, and reporting
        // that as an incomplete delivery turned a queue this SDK owns into a
        // hard step failure the voter had to see.
        if report
            .deliveries
            .iter()
            .filter(|delivery| placed_nothing(delivery))
            .all(|delivery| delivery.submission.local_capacity_exhausted)
            && report.pending_share_indices.is_empty()
        {
            return DeliveryProgress::AwaitingLocalCapacity;
        }
        return DeliveryProgress::Incomplete;
    }
    if report
        .deliveries
        .iter()
        .any(|delivery| delivery.submission.accepted_urls.is_empty())
    {
        return DeliveryProgress::AwaitingAmbiguousHelpers;
    }
    DeliveryProgress::Complete
}
