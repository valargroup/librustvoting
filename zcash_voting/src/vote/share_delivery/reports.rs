//! Deterministic per-proposal accounting for out-of-order share completion.

use super::{preparation::PreparedVoteDelivery, VoteDeliveryResult};
use crate::{
    share_tracking::{
        batch_delivery_report, ShareBatchDeliveryReport, ShareDeliveryFailure, ShareDeliveryOutcome,
    },
    vote::CommittedVote,
    VotingError,
};
use std::{collections::BTreeMap, sync::Arc};

pub(super) type ShareResult = Result<Option<ShareDeliveryOutcome>, VotingError>;

/// Owns one proposal's validation and its single final report. Results are
/// indexed by payload position so the primary error is independent of timing.
pub(super) struct ProposalDelivery<'a> {
    pub(super) vote: &'a CommittedVote,
    pub(super) prepared: Option<Arc<PreparedVoteDelivery<'a>>>,
    shares: BTreeMap<usize, ShareResult>,
    remaining: usize,
    delivery: Option<Result<ShareBatchDeliveryReport, ShareDeliveryFailure>>,
}

impl<'a> ProposalDelivery<'a> {
    pub(super) fn new(
        vote: &'a CommittedVote,
        prepared: Result<PreparedVoteDelivery<'a>, VotingError>,
    ) -> Self {
        match prepared {
            Ok(prepared) => Self {
                vote,
                remaining: prepared.plan.share_plans.len(),
                prepared: Some(Arc::new(prepared)),
                shares: BTreeMap::new(),
                delivery: None,
            },
            Err(error) => Self {
                vote,
                prepared: None,
                shares: BTreeMap::new(),
                remaining: 0,
                delivery: Some(Err(ShareDeliveryFailure {
                    error,
                    partial: None,
                })),
            },
        }
    }

    /// Records a completed or cancellation-skipped job. Each position is
    /// produced once by the queue; failed jobs remain pending in the report.
    pub(super) fn record(&mut self, position: usize, result: ShareResult) {
        self.shares.insert(position, result);
        self.remaining -= 1;
    }

    /// Finalizes once, including all completed siblings of a failed share.
    pub(super) fn finish(&mut self, cancelled: bool) -> Option<&ShareBatchDeliveryReport> {
        if self.remaining != 0 || self.delivery.is_some() {
            return None;
        }
        let prepared = self.prepared.as_ref()?;
        self.delivery = Some(batch_delivery_report(
            std::mem::take(&mut self.shares).into_values().collect(),
            self.vote
                .commit
                .share_payloads
                .iter()
                .map(|payload| payload.enc_share.share_index),
            cancelled,
            prepared.plan.placement_guarantee,
        ));
        match self.delivery.as_ref()? {
            Ok(report) => Some(report),
            Err(failure) => failure.partial.as_ref(),
        }
    }

    pub(super) fn into_result(self) -> VoteDeliveryResult<'a> {
        VoteDeliveryResult {
            vote: self.vote,
            delivery: self
                .delivery
                .expect("every proposal is finalized after the queue drains"),
        }
    }
}
