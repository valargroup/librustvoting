//! What one step has already accomplished.
//!
//! A step records its chain outcome, every helper delivery report, and the
//! delegation it signed here as they happen. Every outcome, cancellation and
//! failure is built from the ledger, so a later error cannot drop an earlier
//! durable confirmation or a delivery that reached the helpers.

use crate::{delegate::SignedDelegationBundle, ChainSubmissionResult};

use super::VoteShareDeliveryReport;

#[derive(Clone, Debug, Default)]
pub(super) struct StepLedger {
    /// The authoritative outcome of the step's chain episode, once it ran.
    pub(super) chain_outcome: Option<ChainSubmissionResult>,
    /// Helper delivery reports; vote completion normalizes these to unit order
    /// after the queue drains, independently of progress-event order.
    pub(super) share_deliveries: Vec<VoteShareDeliveryReport>,
    /// The signed delegation a `Delegate` step produced.
    pub(super) delegation: Option<SignedDelegationBundle>,
}

impl StepLedger {
    pub(super) fn with_delegation(delegation: SignedDelegationBundle) -> Self {
        Self {
            delegation: Some(delegation),
            ..Self::default()
        }
    }

    pub(super) fn record_chain_outcome(&mut self, outcome: ChainSubmissionResult) {
        self.chain_outcome = Some(outcome);
    }

    pub(super) fn record_delivery(&mut self, report: VoteShareDeliveryReport) {
        self.share_deliveries.push(report);
    }
}
