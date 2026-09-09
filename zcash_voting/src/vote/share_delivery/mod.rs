//! Initial delivery of complete, confirmed commitments through one bounded
//! queue. Preparation owns commitment-wide validation; admission owns the
//! process-wide share and aggregate fan-out ceilings; the per-share executor
//! owns every durable attempt and transport outcome.

mod capacity;
mod preparation;
mod queue;
mod reports;

use crate::{
    helper::client::HelperClient,
    round::VotingDb,
    share_tracking::{
        ShareBatchDeliveryReport, ShareDeliveryFailure, ShareDeliverySubmissionParams,
    },
    vote::{CommittedVote, ConfirmedVote},
};

pub(super) use queue::submit_votes;

/// One proposal's complete or partial delivery outcome, in input vote order.
pub(crate) struct VoteDeliveryResult<'a> {
    pub(crate) vote: &'a CommittedVote,
    pub(crate) delivery: Result<ShareBatchDeliveryReport, ShareDeliveryFailure>,
}

/// Delivers all eligible shares of a confirmed unit without proposal barriers.
///
/// Every proposal is validated in full before admission. A proposal-local error
/// does not stop independent work. The callback receives each finalized report
/// once, in completion order; returned results retain input order. Cancellation
/// stops admission, drains admitted workflows, and leaves unprocessed shares
/// pending. The callback must not block: it runs on the delivery task.
pub(crate) async fn submit_confirmed_vote_shares<'a>(
    votes: &'a [ConfirmedVote],
    db: &VotingDb,
    client: &HelperClient,
    params: ShareDeliverySubmissionParams<'_>,
    cancel: &(dyn Fn() -> bool + Send + Sync),
    on_report: &mut (dyn FnMut(&CommittedVote, &ShareBatchDeliveryReport) + Send),
) -> Vec<VoteDeliveryResult<'a>> {
    submit_votes(
        votes.iter().map(ConfirmedVote::vote),
        db,
        client,
        params,
        cancel,
        on_report,
    )
    .await
}
