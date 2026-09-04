//! Completing committed votes: durable helper plans, chain advancement,
//! share delivery once confirmed, and focused share confirmation.

use crate::{
    session::{NextStep, VoteRecoveryWork, VoteRecoveryWorkKind},
    share_tracking::{
        confirm_pending_share, ShareConfirmationParams, ShareDeliveryPlanningParams,
        ShareDeliverySubmissionParams, ShareKey,
    },
    vote::{CommittedVote, SignedVoteBatch},
    AdvanceVote, AdvanceVoteBatch, ChainAdvanceOutcome, ChainAdvancePolicy, ChainAdvanceRequest,
    ChainSubmissionControl, ChainTransport,
};

use super::{
    execution::vote_key, steps::persisted_policy, RoundExecutor, RoundHostContext,
    RoundStepDisposition, RoundStepFailure, RoundStepFailureKind, RoundStepOutcome,
    RoundStepProgress, RoundStepProgressReporter, VoteRecoveryRequest, VoteShareDeliveryReport,
};

impl<T: ChainTransport> RoundExecutor<T> {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn run_persisted_vote_work(
        &self,
        step: NextStep,
        kind: VoteRecoveryWorkKind,
        bundle_index: u32,
        proposal_id: u32,
        host: &RoundHostContext,
        control: &ChainSubmissionControl,
        progress: &dyn RoundStepProgressReporter,
    ) -> Result<RoundStepOutcome, RoundStepFailure> {
        let binding = self
            .binding()
            .map_err(|error| self.step_voting_failure(error, Some(step.clone())))?;
        let proposal_ids = binding.proposal_ids();
        let request = VoteRecoveryRequest {
            round_id: &binding.round_id,
            proposal_ids: &proposal_ids,
            configured_helper_urls: &host.configured_helper_urls,
            now_seconds: host.now_seconds,
            vote_end_time_seconds: host.planning_vote_end_seconds(),
            last_moment_buffer_seconds: host.last_moment_buffer_seconds(),
        };
        let work = VoteRecoveryWork {
            kind,
            bundle_index,
            proposal_id,
            tx_hash: None,
            vc_tree_position: None,
            share_indexes: Vec::new(),
        };
        let (votes, batch) = self
            .recover_work_votes(&work, request)
            .map_err(|error| self.step_voting_failure(error, Some(step.clone())))?;
        let advance_chain = !matches!(kind, VoteRecoveryWorkKind::SubmitShares);
        let policy = persisted_policy(host);
        self.finish_vote_work(
            step,
            votes,
            batch,
            advance_chain,
            &policy,
            host,
            control,
            progress,
        )
        .await
    }

    /// Prepares durable helper plans, advances the chain when needed, and
    /// delivers shares once the vote is confirmed.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn finish_vote_work(
        &self,
        step: NextStep,
        votes: Vec<CommittedVote>,
        batch: Option<SignedVoteBatch>,
        advance_chain: bool,
        policy: &ChainAdvancePolicy,
        host: &RoundHostContext,
        control: &ChainSubmissionControl,
        progress: &dyn RoundStepProgressReporter,
    ) -> Result<RoundStepOutcome, RoundStepFailure> {
        let binding = self
            .binding()
            .map_err(|error| self.step_voting_failure(error, Some(step.clone())))?;
        let round_id = binding.round_id.clone();
        let proposal_ids = binding.proposal_ids();
        let Some(first) = votes.first() else {
            return Err(self.step_failure(
                RoundStepFailureKind::InvariantViolation,
                Some(step),
                None,
                None,
                "vote work recovered no committed votes",
            ));
        };
        let bundle_index = first.bundle_index();
        let first_proposal = first.proposal_id();

        let preflight = self
            .helper_client
            .preflight_fleet(&host.configured_helper_urls)
            .await
            .map_err(|error| self.step_voting_failure(error, Some(step.clone())))?;
        if control.is_cancelled() {
            return self.step_cancelled(Some(step), None, Vec::new(), None);
        }
        for vote in &votes {
            vote.prepare_share_delivery(
                &self.database,
                ShareDeliveryPlanningParams {
                    fleet: &preflight,
                    now_seconds: host.now_seconds,
                    vote_end_time_seconds: host.planning_vote_end_seconds(),
                    last_moment_buffer_seconds: host.last_moment_buffer_seconds(),
                    proposal_ids: &proposal_ids,
                },
            )
            .map_err(|error| self.step_voting_failure(error, Some(step.clone())))?;
        }
        progress.report(RoundStepProgress::HelperPlansPrepared(
            votes.iter().map(vote_key).collect(),
        ));

        let mut chain_outcome = None;
        if advance_chain {
            let vote_round_id = self.round_id_bytes(&step)?;
            let request = match &batch {
                Some(batch) => ChainAdvanceRequest::VoteBatch(AdvanceVoteBatch {
                    vote_round_id,
                    bundle_index,
                    ordered_batch_digest: batch.batch_digest,
                    ordered_proposal_ids: batch
                        .commitments
                        .iter()
                        .map(|commitment| commitment.proposal_id)
                        .collect(),
                }),
                None => ChainAdvanceRequest::Vote(AdvanceVote {
                    vote_round_id,
                    bundle_index,
                    proposal_id: first_proposal,
                }),
            };
            let outcome = self
                .chain_client
                .advance_until_terminal(request, policy, control)
                .await
                .map_err(|failure| self.step_chain_failure(failure, Some(step.clone())))?;
            let result = outcome.clone().into_result();
            progress.report(RoundStepProgress::ChainOutcome(result.clone()));
            chain_outcome = Some(result);
            match outcome {
                ChainAdvanceOutcome::Confirmed(_) => {}
                ChainAdvanceOutcome::StillPending(_) => {
                    return self.outcome(
                        step,
                        RoundStepDisposition::Pending,
                        chain_outcome,
                        Vec::new(),
                        None,
                    );
                }
                ChainAdvanceOutcome::Cancelled => {
                    return self.step_cancelled(Some(step), chain_outcome, Vec::new(), None);
                }
                ChainAdvanceOutcome::SubmittedWithoutHash(_) | ChainAdvanceOutcome::Rejected(_) => {
                    return self.outcome(
                        step,
                        RoundStepDisposition::ChainTerminal,
                        chain_outcome,
                        Vec::new(),
                        None,
                    );
                }
            }
        }

        let mut deliveries = Vec::with_capacity(votes.len());
        for vote in votes {
            if control.is_cancelled() {
                return self.step_cancelled(Some(step), chain_outcome, deliveries, None);
            }
            // Confirmation updates the durable recovery generation, so recover
            // a fresh handle and let the type system prove it is confirmed.
            let vote = CommittedVote::recover(
                &self.database,
                &round_id,
                vote.bundle_index(),
                vote.proposal_id(),
            )
            .and_then(|vote| vote.confirmed(&self.database))
            .map_err(|error| self.step_voting_failure(error, Some(step.clone())))?
            .ok_or_else(|| {
                self.step_failure(
                    RoundStepFailureKind::InvariantViolation,
                    Some(step.clone()),
                    None,
                    chain_outcome.clone(),
                    "vote was reported confirmed but its recovery material has no tree position",
                )
            })?;
            let cancel = || control.is_cancelled();
            let delivery = vote
                .submit_prepared_shares(
                    &self.database,
                    &self.helper_client,
                    ShareDeliverySubmissionParams {
                        configured_server_urls: &host.configured_helper_urls,
                        now_seconds: host.now_seconds,
                    },
                    &cancel,
                )
                .await
                .map_err(|error| self.step_voting_failure(error, Some(step.clone())))?;
            let report = VoteShareDeliveryReport {
                vote: vote_key(vote.vote()),
                delivery,
            };
            progress.report(RoundStepProgress::ShareOutcome(report.clone()));
            let cancelled = report.delivery.cancelled;
            let incomplete = !report.delivery.pending_share_indices.is_empty()
                || report.delivery.deliveries.iter().any(|delivery| {
                    delivery.submission.accepted_urls.is_empty()
                        && delivery.submission.ambiguous_urls.is_empty()
                });
            deliveries.push(report);
            if cancelled {
                return self.step_cancelled(Some(step), chain_outcome, deliveries, None);
            }
            if incomplete {
                return Err(self.step_failure(
                    RoundStepFailureKind::HelperDeliveryIncomplete,
                    Some(step),
                    None,
                    chain_outcome,
                    "helper delivery ended with pending shares",
                ));
            }
        }
        self.outcome(
            step,
            RoundStepDisposition::Advanced,
            chain_outcome,
            deliveries,
            None,
        )
    }

    pub(super) async fn run_confirm_share(
        &self,
        step: NextStep,
        share: ShareKey,
        host: &RoundHostContext,
        control: &ChainSubmissionControl,
        progress: &dyn RoundStepProgressReporter,
    ) -> Result<RoundStepOutcome, RoundStepFailure> {
        let round_id = self
            .binding()
            .map(|binding| binding.round_id.clone())
            .map_err(|error| self.step_voting_failure(error, Some(step.clone())))?;
        let cancel = || control.is_cancelled();
        let report = confirm_pending_share(
            &self.database,
            &ShareConfirmationParams {
                round_id: &round_id,
                share,
                configured_server_urls: &host.configured_helper_urls,
                now_seconds: host.now_seconds,
            },
            &self.helper_client,
            &cancel,
        )
        .await
        .map_err(|error| self.step_voting_failure(error, Some(step.clone())))?;
        progress.report(RoundStepProgress::ShareConfirmed {
            share,
            confirmed: report.confirmed,
        });
        let disposition = if report.confirmed {
            RoundStepDisposition::Advanced
        } else if control.is_cancelled() {
            RoundStepDisposition::Cancelled
        } else {
            RoundStepDisposition::Pending
        };
        self.outcome(step, disposition, None, Vec::new(), None)
    }
}
