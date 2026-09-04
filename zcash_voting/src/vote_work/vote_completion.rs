//! Completing a vote unit: durable helper plans, chain advancement, and
//! share delivery once confirmed. One path serves a fresh cast and every
//! resumed unit; they differ only in when helper plans are made durable.

use crate::{
    round_planning::VoteUnitId,
    share_tracking::{
        ShareBatchDeliveryReport, ShareDeliveryPlanningParams, ShareDeliverySubmissionParams,
    },
    vote::{recover_atomic_vote_batch, CommittedVote, SignedVoteBatch},
    AdvanceVote, AdvanceVoteBatch, ChainAdvanceOutcome, ChainAdvancePolicy, ChainAdvanceRequest,
    ChainTransport, VotingError,
};

use super::{
    step_ledger::StepLedger, step_scope::vote_key, step_scope::StepScope, steps::persisted_policy,
    RoundExecutor, RoundHostContext, RoundStepDisposition, RoundStepFailure, RoundStepFailureKind,
    RoundStepOutcome, RoundStepProgress, RoundStepProgressReporter, VoteShareDeliveryReport,
};

/// Where a vote unit's work stands when completion takes it over, which
/// decides when its helper plans are made durable.
#[derive(Clone, Copy, Debug)]
pub(super) enum CompletionEntry {
    /// The step just committed the vote: plans are prepared before the chain
    /// broadcast so a confirmed vote never lacks a durable plan.
    FreshCast,
    /// Work recovered from durable state. With `plans_first` the unit has
    /// never been dispatched, so it completes as a fresh cast does: plans
    /// before the broadcast. Otherwise the chain is reconciled first (when
    /// `advance_chain`), and each vote's plan is loaded or created only after
    /// confirmation, right before its shares are delivered.
    Resume {
        advance_chain: bool,
        plans_first: bool,
    },
}

/// What one vote's helper delivery report says about the shares it covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DeliveryProgress {
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
pub(super) fn delivery_progress(report: &ShareBatchDeliveryReport) -> DeliveryProgress {
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

/// Planning inputs for one vote's helper plan under the host's clock.
fn planning_params<'a>(
    fleet: &'a crate::helper::client::HelperFleetPreflight,
    host: &RoundHostContext,
    proposal_ids: &'a [u32],
) -> ShareDeliveryPlanningParams<'a> {
    ShareDeliveryPlanningParams {
        fleet,
        now_seconds: host.now_seconds,
        vote_end_time_seconds: host.planning_vote_end_seconds(),
        last_moment_buffer_seconds: host.last_moment_buffer_seconds(),
        proposal_ids,
    }
}

impl<T: ChainTransport> RoundExecutor<T> {
    /// Drives a committed or on-wire unit through the chain lifecycle and,
    /// once confirmed, delivers its shares. An `undispatched` unit (no POST
    /// reserved yet, typically a cast whose plan preparation failed) makes
    /// its plans durable before the broadcast, as the cast would have.
    pub(super) async fn run_reconcile_chain(
        &self,
        scope: &StepScope<'_>,
        unit: VoteUnitId,
        ordered_proposal_ids: &[u32],
        undispatched: bool,
        progress: &dyn RoundStepProgressReporter,
    ) -> Result<RoundStepOutcome, RoundStepFailure> {
        let ledger = StepLedger::default();
        let (votes, batch) = self
            .recover_unit_votes(&scope.round_id, unit, ordered_proposal_ids)
            .map_err(|error| self.step_voting_failure(error, Some(&scope.step), &ledger))?;
        self.complete_vote_unit(
            scope,
            votes,
            batch,
            CompletionEntry::Resume {
                advance_chain: true,
                plans_first: undispatched,
            },
            &persisted_policy(scope.host),
            ledger,
            progress,
        )
        .await
    }

    /// Delivers the shares a confirmed vote still owes.
    pub(super) async fn run_deliver(
        &self,
        scope: &StepScope<'_>,
        bundle_index: u32,
        proposal_id: u32,
        progress: &dyn RoundStepProgressReporter,
    ) -> Result<RoundStepOutcome, RoundStepFailure> {
        let ledger = StepLedger::default();
        let vote =
            CommittedVote::recover(&self.database, &scope.round_id, bundle_index, proposal_id)
                .map_err(|error| self.step_voting_failure(error, Some(&scope.step), &ledger))?;
        self.complete_vote_unit(
            scope,
            vec![vote],
            None,
            CompletionEntry::Resume {
                advance_chain: false,
                plans_first: false,
            },
            &persisted_policy(scope.host),
            ledger,
            progress,
        )
        .await
    }

    /// Recovers the committed vote handles `unit` names: one for a
    /// singleton, every member (with the signed batch) for an atomic batch.
    fn recover_unit_votes(
        &self,
        round_id: &str,
        unit: VoteUnitId,
        ordered_proposal_ids: &[u32],
    ) -> Result<(Vec<CommittedVote>, Option<SignedVoteBatch>), VotingError> {
        match unit {
            VoteUnitId::Singleton {
                bundle_index,
                proposal_id,
            } => Ok((
                vec![CommittedVote::recover(
                    &self.database,
                    round_id,
                    bundle_index,
                    proposal_id,
                )?],
                None,
            )),
            VoteUnitId::Batch { bundle_index, .. } => {
                let anchor =
                    ordered_proposal_ids
                        .first()
                        .copied()
                        .ok_or_else(|| VotingError::Internal {
                            message: "an atomic batch obligation names no members".to_string(),
                        })?;
                let batch =
                    recover_atomic_vote_batch(&self.database, round_id, bundle_index, anchor)?;
                let votes = batch
                    .commitments
                    .iter()
                    .map(|commitment| {
                        CommittedVote::recover(
                            &self.database,
                            round_id,
                            bundle_index,
                            commitment.proposal_id,
                        )
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok((votes, Some(batch)))
            }
        }
    }

    /// Prepares durable helper plans, advances the chain when needed, and
    /// delivers shares once the unit is confirmed. Everything accomplished
    /// is recorded in `ledger`, which every outcome and failure carries.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn complete_vote_unit(
        &self,
        scope: &StepScope<'_>,
        votes: Vec<CommittedVote>,
        batch: Option<SignedVoteBatch>,
        entry: CompletionEntry,
        policy: &ChainAdvancePolicy,
        mut ledger: StepLedger,
        progress: &dyn RoundStepProgressReporter,
    ) -> Result<RoundStepOutcome, RoundStepFailure> {
        let host = scope.host;
        let round_id = scope.round_id.as_str();
        let proposal_ids = scope.proposal_ids();
        let Some(first) = votes.first() else {
            return Err(self.step_failure(
                RoundStepFailureKind::InvariantViolation,
                Some(&scope.step),
                None,
                &ledger,
                "vote work recovered no committed votes",
            ));
        };
        let bundle_index = first.bundle_index();
        let first_proposal = first.proposal_id();

        // Proving may have taken minutes; do not start contacting helpers for
        // a step the host has since cancelled or moved past.
        if scope.interrupted() {
            return self.step_cancelled(scope, ledger);
        }
        let (advance_chain, plans_before_chain) = match entry {
            CompletionEntry::FreshCast => (true, true),
            CompletionEntry::Resume {
                advance_chain,
                plans_first,
            } => (advance_chain, plans_first),
        };
        let mut fleet_preflight = None;
        if plans_before_chain {
            // A fresh cast, and a committed unit that was never dispatched,
            // make their plans durable before the broadcast, so a crash
            // between the two cannot leave a confirmed vote without a plan.
            // Work already on the wire went through this at cast time (or
            // predates plans); it reconciles the chain first, so an open
            // ballot cannot keep an already-dispatched vote from being polled
            // or recovered, and ensures its plan only before delivery.
            let preflight = self
                .helper_client
                .preflight_fleet(&host.configured_helper_urls)
                .await
                .map_err(|error| self.step_voting_failure(error, Some(&scope.step), &ledger))?;
            if scope.interrupted() {
                return self.step_cancelled(scope, ledger);
            }
            for vote in &votes {
                vote.prepare_share_delivery(
                    &self.database,
                    planning_params(&preflight, host, &proposal_ids),
                )
                .map_err(|error| self.step_voting_failure(error, Some(&scope.step), &ledger))?;
            }
            progress.report(RoundStepProgress::HelperPlansPrepared(
                votes.iter().map(vote_key).collect(),
            ));
            fleet_preflight = Some(preflight);
        }

        if advance_chain {
            let request = match &batch {
                Some(batch) => ChainAdvanceRequest::VoteBatch(AdvanceVoteBatch {
                    vote_round_id: scope.round_id_bytes,
                    bundle_index,
                    ordered_batch_digest: batch.batch_digest,
                    ordered_proposal_ids: batch
                        .commitments
                        .iter()
                        .map(|commitment| commitment.proposal_id)
                        .collect(),
                }),
                None => ChainAdvanceRequest::Vote(AdvanceVote {
                    vote_round_id: scope.round_id_bytes,
                    bundle_index,
                    proposal_id: first_proposal,
                }),
            };
            let outcome = self
                .chain_client
                .advance_until_terminal_in_epoch(
                    request,
                    policy,
                    scope.chain(),
                    scope.entry_epoch(),
                )
                .await
                .map_err(|failure| self.step_chain_failure(failure, Some(&scope.step), &ledger))?;
            let result = outcome.clone().into_result();
            progress.report(RoundStepProgress::ChainOutcome(result.clone()));
            ledger.record_chain_outcome(result);
            match outcome {
                ChainAdvanceOutcome::Confirmed(_) => {}
                ChainAdvanceOutcome::StillPending(_) => {
                    return self.outcome(scope, RoundStepDisposition::Pending, ledger);
                }
                ChainAdvanceOutcome::Cancelled => {
                    return self.step_cancelled(scope, ledger);
                }
                ChainAdvanceOutcome::SubmittedWithoutHash(_) | ChainAdvanceOutcome::Rejected(_) => {
                    return self.outcome(scope, RoundStepDisposition::ChainTerminal, ledger);
                }
            }
        }

        for vote in votes {
            if scope.interrupted() {
                return self.step_cancelled(scope, ledger);
            }
            // Confirmation updates the durable recovery generation, so recover
            // a fresh handle and let the type system prove it is confirmed.
            let vote = CommittedVote::recover(
                &self.database,
                round_id,
                vote.bundle_index(),
                vote.proposal_id(),
            )
            .map_err(|error| self.step_voting_failure(error, Some(&scope.step), &ledger))?;
            if !plans_before_chain {
                // Resumed work: load the plan made at cast time, or create
                // one now for a vote that predates plans.
                if fleet_preflight.is_none() {
                    let preflight = self
                        .helper_client
                        .preflight_fleet(&host.configured_helper_urls)
                        .await
                        .map_err(|error| {
                            self.step_voting_failure(error, Some(&scope.step), &ledger)
                        })?;
                    // The preflight may have waited on unreachable helpers;
                    // a plan (and the round's immutable designation) must not
                    // be written for a step the host has since left.
                    if scope.interrupted() {
                        return self.step_cancelled(scope, ledger);
                    }
                    fleet_preflight = Some(preflight);
                }
                let preflight = fleet_preflight
                    .as_ref()
                    .expect("preflight was just taken for resumed work");
                vote.prepare_share_delivery(
                    &self.database,
                    planning_params(preflight, host, &proposal_ids),
                )
                .map_err(|error| self.step_voting_failure(error, Some(&scope.step), &ledger))?;
                progress.report(RoundStepProgress::HelperPlansPrepared(vec![vote_key(
                    &vote,
                )]));
            }
            let vote = vote
                .confirmed(&self.database)
                .map_err(|error| self.step_voting_failure(error, Some(&scope.step), &ledger))?
                .ok_or_else(|| {
                    self.step_failure(
                        RoundStepFailureKind::InvariantViolation,
                        Some(&scope.step),
                        None,
                        &ledger,
                        "vote was reported confirmed but its recovery material has no tree position",
                    )
                })?;
            let cancel = || scope.interrupted();
            // Reports of the votes delivered so far ride on every failure
            // from here on: their network effects happened and are otherwise
            // visible only to a progress reporter.
            let delivery = match vote
                .submit_prepared_shares_keeping_partial_report(
                    &self.database,
                    &self.helper_client,
                    ShareDeliverySubmissionParams {
                        configured_server_urls: &host.configured_helper_urls,
                        now_seconds: host.now_seconds,
                    },
                    &cancel,
                )
                .await
            {
                Ok(delivery) => delivery,
                Err(failure) => {
                    // Sibling shares of the failing vote may already have
                    // reached the helpers; their report rides on the failure.
                    if let Some(partial) = failure.partial {
                        let report = VoteShareDeliveryReport {
                            vote: vote_key(vote.vote()),
                            delivery: partial,
                        };
                        progress.report(RoundStepProgress::ShareOutcome(report.clone()));
                        ledger.record_delivery(report);
                    }
                    return Err(self.step_voting_failure(
                        failure.error,
                        Some(&scope.step),
                        &ledger,
                    ));
                }
            };
            let report = VoteShareDeliveryReport {
                vote: vote_key(vote.vote()),
                delivery,
            };
            progress.report(RoundStepProgress::ShareOutcome(report.clone()));
            let cancelled = report.delivery.cancelled;
            let delivery_progress = delivery_progress(&report.delivery);
            ledger.record_delivery(report);
            if cancelled {
                return self.step_cancelled(scope, ledger);
            }
            match delivery_progress {
                DeliveryProgress::Complete => {}
                DeliveryProgress::AwaitingAmbiguousHelpers => {
                    // The step made no definite placement it could repeat:
                    // only tracking can classify those attempts. Report
                    // pending so the host schedules again instead of
                    // rerunning delivery at once.
                    return self.outcome(scope, RoundStepDisposition::Pending, ledger);
                }
                DeliveryProgress::Incomplete => {
                    return Err(self.step_failure(
                        RoundStepFailureKind::HelperDeliveryIncomplete,
                        Some(&scope.step),
                        None,
                        &ledger,
                        "helper delivery ended with pending shares",
                    ));
                }
            }
        }
        self.outcome(scope, RoundStepDisposition::Advanced, ledger)
    }
}
