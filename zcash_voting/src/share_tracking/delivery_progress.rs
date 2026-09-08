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
    /// Some share reached no helper at all, or was left pending.
    Incomplete,
}

/// Classifies `report`. Ambiguous attempts are excluded from the next
/// delivery pass, so treating them as complete would let a step report
/// `Advanced` forever without a share ever landing.
pub(crate) fn delivery_progress(report: &ShareBatchDeliveryReport) -> DeliveryProgress {
    if !report.pending_share_indices.is_empty()
        || report.deliveries.iter().any(|delivery| {
            delivery.submission.accepted_urls.is_empty()
                && delivery.submission.ambiguous_urls.is_empty()
        })
    {
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
