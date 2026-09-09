//! A single stream refills share slots across commitment boundaries.

use super::{
    capacity,
    preparation::{self, PreparedVoteDelivery},
    reports::{ProposalDelivery, ShareResult},
    VoteDeliveryResult,
};
use crate::{
    helper::client::HelperClient,
    round::VotingDb,
    share::ShareOperationScope,
    share_tracking::{
        CommittedShareSubmissionRequest, ShareBatchDeliveryReport, ShareDeliveryOutcome,
        ShareDeliverySubmissionParams,
    },
    vote::CommittedVote,
};
use futures_util::{stream::FuturesUnordered, StreamExt};
use std::sync::Arc;

/// Lightweight queue entry; payloads and immutable plans are shared per vote.
struct ShareJob<'a> {
    proposal_position: usize,
    payload_position: usize,
    prepared: Arc<PreparedVoteDelivery<'a>>,
    observations: crate::ObservationScope,
    queued: crate::observability::ObservationStage,
}

/// Attribution retained when a share finishes ahead of earlier queue entries.
struct ShareJobCompletion {
    proposal_position: usize,
    payload_position: usize,
    delivery: ShareResult,
}

impl ShareJob<'_> {
    async fn deliver(
        self,
        db: &VotingDb,
        client: &HelperClient,
        scope: &ShareOperationScope,
        params: &ShareDeliverySubmissionParams<'_>,
        cancel: &(dyn Fn() -> bool + Send + Sync),
    ) -> ShareResult {
        let admission = capacity::acquire(cancel).await;
        self.queued.finish(
            match &admission {
                Ok(Some(_)) => crate::ObservationOutcome::Succeeded,
                Ok(None) => crate::ObservationOutcome::Cancelled,
                Err(_) => crate::ObservationOutcome::Failed,
            },
            admission
                .as_ref()
                .err()
                .map(crate::observability::voting_error_kind),
        );
        let Some(_permit) = admission? else {
            return Ok(None);
        };
        let active = self.observations.stage("helper::active_delivery");
        let observed_client = client.observing(active.scope());
        let client = &observed_client;
        let vote = self.prepared.vote;
        let share_index = vote.commit.share_payloads[self.payload_position]
            .enc_share
            .share_index;
        let submission = vote
            .submit_share_to_helpers_for_generation(
                db,
                client,
                CommittedShareSubmissionRequest {
                    share_index,
                    plan: &self.prepared.plan.share_plans[self.payload_position],
                    planning_server_urls: &self.prepared.plan.configured_server_urls,
                    configured_server_urls: params.configured_server_urls,
                    now_seconds: params.now_seconds,
                },
                &self.prepared.generation,
                scope,
                cancel,
            )
            .await;
        let outcome = match &submission {
            Err(_) => crate::ObservationOutcome::Failed,
            Ok(report) if !report.accepted_urls.is_empty() => crate::ObservationOutcome::Succeeded,
            Ok(report) if !report.ambiguous_urls.is_empty() => crate::ObservationOutcome::Pending,
            Ok(_) => crate::ObservationOutcome::Failed,
        };
        active.finish(
            outcome,
            submission
                .as_ref()
                .err()
                .map(crate::observability::voting_error_kind),
        );
        submission.map(|submission| {
            Some(ShareDeliveryOutcome {
                share_index,
                submission,
            })
        })
    }
}

/// Shared mechanism for the confirmed multi-vote boundary and the historical
/// singleton wrapper. Preparation revalidates durable confirmation even for
/// callers that already hold a ConfirmedVote.
pub(in crate::vote) async fn submit_votes<'a>(
    votes: impl IntoIterator<Item = &'a CommittedVote>,
    db: &VotingDb,
    client: &HelperClient,
    params: ShareDeliverySubmissionParams<'_>,
    cancel: &(dyn Fn() -> bool + Send + Sync),
    on_report: &mut (dyn FnMut(&CommittedVote, &ShareBatchDeliveryReport) + Send),
) -> Vec<VoteDeliveryResult<'a>> {
    let scope = ShareOperationScope::capture(db);
    let mut proposals = votes
        .into_iter()
        .map(|vote| ProposalDelivery::new(vote, preparation::prepare(vote, db, &scope, &params)))
        .collect::<Vec<_>>();
    let jobs = proposals
        .iter()
        .enumerate()
        .flat_map(|(proposal_position, proposal)| {
            proposal.prepared.iter().flat_map(move |prepared| {
                (0..prepared.plan.share_plans.len()).map(move |payload_position| {
                    let observations =
                        client
                            .observation_scope()
                            .attributed(crate::ObservationAttribution {
                                bundle_index: Some(prepared.vote.bundle_index()),
                                proposal_id: Some(prepared.vote.proposal_id()),
                                share_index: Some(
                                    prepared.vote.commit.share_payloads[payload_position]
                                        .enc_share
                                        .share_index,
                                ),
                            });
                    let queued = observations.stage("helper::delivery_queue_wait");
                    ShareJob {
                        proposal_position,
                        payload_position,
                        prepared: Arc::clone(prepared),
                        observations,
                        queued,
                    }
                })
            })
        })
        .collect::<Vec<_>>();
    // Empty plans are normally rejected by validation, but keep accounting
    // total without requiring a stream item to finalize an empty proposal.
    for proposal in &mut proposals {
        let vote = proposal.vote;
        if let Some(report) = proposal.finish(cancel()) {
            on_report(vote, report);
        }
    }
    let mut jobs = jobs.into_iter();
    let mut deliveries = FuturesUnordered::new();
    loop {
        while deliveries.len() < capacity::MAX_CONCURRENT_SHARE_DELIVERIES {
            let Some(job) = jobs.next() else {
                break;
            };
            deliveries.push(run_job(job, db, client, &scope, &params, cancel));
        }
        // A hard error must not drop live sibling POSTs or leave independent
        // proposals unsent. Cancelled jobs skip admission without touching storage.
        let Some(completion) = deliveries.next().await else {
            break;
        };
        let proposal = &mut proposals[completion.proposal_position];
        proposal.record(completion.payload_position, completion.delivery);
        let vote = proposal.vote;
        if let Some(report) = proposal.finish(cancel()) {
            on_report(vote, report);
        }
    }
    proposals
        .into_iter()
        .map(ProposalDelivery::into_result)
        .collect()
}

async fn run_job(
    job: ShareJob<'_>,
    db: &VotingDb,
    client: &HelperClient,
    scope: &ShareOperationScope,
    params: &ShareDeliverySubmissionParams<'_>,
    cancel: &(dyn Fn() -> bool + Send + Sync),
) -> ShareJobCompletion {
    let proposal_position = job.proposal_position;
    let payload_position = job.payload_position;
    let delivery = job.deliver(db, client, scope, params, cancel).await;
    ShareJobCompletion {
        proposal_position,
        payload_position,
        delivery,
    }
}
