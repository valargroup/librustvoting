//! Completing a vote unit: durable helper plans, chain advancement, and
//! share delivery once confirmed. One path serves a fresh cast and every
//! resumed unit; they differ only in when helper plans are made durable.

use crate::{
    round_planning::VoteUnitId,
    share_tracking::{
        delivery_progress, DeliveryProgress, ShareDeliveryPlanningParams,
        ShareDeliverySubmissionParams,
    },
    vote::{CommittedVote, SignedVoteBatch},
    AdvanceVote, ChainAdvanceOutcome, ChainAdvancePolicy, ChainAdvanceRequest, ChainTransport,
    VotingError,
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
            .recover_unit_votes(
                &scope.round_id,
                unit,
                ordered_proposal_ids,
                &scope.observations,
            )
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
        let vote = CommittedVote::observe_recover(
            &self.database,
            &scope.round_id,
            bundle_index,
            proposal_id,
            &scope.observations,
        )
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
        observations: &crate::ObservationScope,
    ) -> Result<(Vec<CommittedVote>, Option<SignedVoteBatch>), VotingError> {
        match unit {
            VoteUnitId::Singleton {
                bundle_index,
                proposal_id,
            } => Ok((
                vec![CommittedVote::observe_recover(
                    &self.database,
                    round_id,
                    bundle_index,
                    proposal_id,
                    observations,
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
                let batch = crate::vote::observe_recover_atomic_vote_batch(
                    &self.database,
                    round_id,
                    bundle_index,
                    anchor,
                    observations,
                )?;
                let votes = batch
                    .commitments
                    .iter()
                    .map(|commitment| {
                        CommittedVote::observe_recover(
                            &self.database,
                            round_id,
                            bundle_index,
                            commitment.proposal_id,
                            observations,
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
        let helper_client = self.helper_client.observing(&scope.observations);

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
                vote.observe_prepare_share_delivery(
                    &self.database,
                    planning_params(&preflight, host, &proposal_ids),
                    &scope.observations,
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
                Some(batch) => batch
                    .advance_request()
                    .map_err(|error| self.step_voting_failure(error, Some(&scope.step), &ledger))?,
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

        let vote_order = votes.iter().map(vote_key).collect::<Vec<_>>();
        let mut confirmed_votes = Vec::with_capacity(votes.len());
        let mut delivery_errors = std::collections::BTreeMap::new();
        for (vote_position, vote) in votes.into_iter().enumerate() {
            if scope.interrupted() {
                break;
            }
            // Confirmation updates the durable recovery generation, so recover
            // a fresh handle and let the type system prove it is confirmed.
            let vote = match CommittedVote::observe_recover(
                &self.database,
                round_id,
                vote.bundle_index(),
                vote.proposal_id(),
                &scope.observations,
            ) {
                Ok(vote) => vote,
                Err(error) => {
                    delivery_errors.insert(vote_position, error);
                    continue;
                }
            };
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
                        break;
                    }
                    fleet_preflight = Some(preflight);
                }
                let preflight = fleet_preflight
                    .as_ref()
                    .expect("preflight was just taken for resumed work");
                if let Err(error) = vote.observe_prepare_share_delivery(
                    &self.database,
                    planning_params(preflight, host, &proposal_ids),
                    &scope.observations,
                ) {
                    delivery_errors.insert(vote_position, error);
                    continue;
                }
                progress.report(RoundStepProgress::HelperPlansPrepared(vec![vote_key(
                    &vote,
                )]));
            }
            match vote.confirmed(&self.database) {
                Ok(Some(vote)) => confirmed_votes.push(vote),
                Ok(None) => {
                    delivery_errors.insert(vote_position, VotingError::Internal {
                        message: "vote was reported confirmed but its recovery material has no tree position".to_string(),
                    });
                }
                Err(error) => {
                    delivery_errors.insert(vote_position, error);
                }
            }
        }
        let cancel = || scope.interrupted();
        let deliveries = crate::vote::submit_confirmed_vote_shares(
            &confirmed_votes,
            &self.database,
            &helper_client,
            ShareDeliverySubmissionParams {
                configured_server_urls: &host.configured_helper_urls,
                now_seconds: host.now_seconds,
            },
            &cancel,
            &mut |vote, delivery| {
                let report = VoteShareDeliveryReport {
                    vote: vote_key(vote),
                    delivery: delivery.clone(),
                };
                // Keep the durable effects before entering arbitrary host code.
                ledger.record_delivery(report.clone());
                progress.report(RoundStepProgress::ShareOutcome(report));
            },
        )
        .await;
        // Events follow completion, but the final ledger keeps the unit's order.
        ledger
            .share_deliveries
            .sort_by_key(|report| vote_order.iter().position(|vote| vote == &report.vote));
        let mut cancelled = scope.interrupted();
        let mut incomplete = false;
        let mut awaiting_ambiguous = false;
        for delivery in deliveries {
            match delivery.delivery {
                Err(failure) => {
                    let position = vote_order
                        .iter()
                        .position(|vote| vote == &vote_key(delivery.vote))
                        .expect("delivery belongs to the completed vote unit");
                    delivery_errors.insert(position, failure.error);
                }
                Ok(report) => {
                    cancelled |= report.cancelled;
                    match delivery_progress(&report) {
                        DeliveryProgress::Complete => {}
                        DeliveryProgress::AwaitingAmbiguousHelpers => awaiting_ambiguous = true,
                        DeliveryProgress::Incomplete => incomplete = true,
                    }
                }
            }
        }
        if let Some((_, error)) = delivery_errors.into_iter().next() {
            return Err(self.step_voting_failure(error, Some(&scope.step), &ledger));
        }
        if cancelled {
            return self.step_cancelled(scope, ledger);
        }
        if incomplete {
            return Err(self.step_failure(
                RoundStepFailureKind::HelperDeliveryIncomplete,
                Some(&scope.step),
                None,
                &ledger,
                "helper delivery ended with pending shares",
            ));
        }
        let disposition = if awaiting_ambiguous {
            RoundStepDisposition::Pending
        } else {
            RoundStepDisposition::Advanced
        };
        self.outcome(scope, disposition, ledger)
    }
}
