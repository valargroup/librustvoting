use crate::{
    session::{resume_plan, VoteRecoveryWorkKind},
    share_tracking::{ShareDeliveryPlanningParams, ShareDeliverySubmissionParams},
    vote::{recover_atomic_vote_batch, CommittedVote},
    AdvanceVote, AdvanceVoteBatch, ChainAdvanceOutcome, ChainAdvancePolicy, ChainAdvanceRequest,
    ChainRecoveryMode, ChainSubmissionControl, ChainSubmissionFailure, ChainSubmissionFailureKind,
    ChainSubmissionResult, ChainTransport, VotingError, VotingErrorKind,
    MAX_CHAIN_SUBMISSION_DIAGNOSTIC_BYTES,
};

use super::{
    round_lock, step_control::StepControl, RoundExecutor, VoteRecoveryAdvance,
    VoteRecoveryDisposition, VoteRecoveryFailure, VoteRecoveryFailureKind, VoteRecoveryKey,
    VoteRecoveryProgress, VoteRecoveryProgressReporter, VoteRecoveryRequest,
    VoteShareDeliveryReport,
};

impl<T: ChainTransport> RoundExecutor<T> {
    /// Advances the first SDK-grouped persisted vote work by one bounded pass.
    ///
    /// The method serializes callers per wallet and round, derives work only
    /// from a fresh [`crate::session::RoundPlan`], persists complete helper
    /// plans before any chain POST, and submits shares only after durable chain
    /// confirmation. Reinvoke after `Pending` according to host scheduling.
    /// The operation epoch is captured on entry; cancellation or an epoch
    /// change is observed at every boundary and ends the pass as `Cancelled`.
    pub async fn advance(
        &self,
        request: VoteRecoveryRequest<'_>,
        control: &ChainSubmissionControl,
        progress: &dyn VoteRecoveryProgressReporter,
    ) -> Result<VoteRecoveryAdvance, VoteRecoveryFailure> {
        let step_control = StepControl::capture(control);
        let control = &step_control;
        let wallet_id = self
            .wallet_scope()
            .map(str::to_string)
            .map_err(|error| self.voting_failure(error, None, request))?;
        // A malformed round id would otherwise match no rows and be reported
        // as an idle round; the documented canonical form is required first.
        parse_round_id(request.round_id)
            .map_err(|error| self.voting_failure(error, None, request))?;
        // No binding is required here, so check the stored round's network
        // against the chain client before helper preflight or any plan write.
        self.ensure_stored_round_network(request.round_id, "this recovery request")
            .map_err(|error| self.voting_failure(error, None, request))?;
        let initial_plan = resume_plan(&self.database, request.round_id, request.proposal_ids)
            .map_err(|error| self.voting_failure(error, None, request))?;
        let Some(work) = initial_plan.recovered_vote_work.first().cloned() else {
            return Ok(VoteRecoveryAdvance {
                attempted_work: None,
                disposition: VoteRecoveryDisposition::NoWork,
                chain_outcome: None,
                share_deliveries: Vec::new(),
                round_plan: initial_plan,
            });
        };

        let Some(_round_guard) = round_lock::acquire(
            self.database.sidecar_id(),
            wallet_id,
            request.round_id,
            None,
            control.chain(),
            control.entry_epoch(),
        )
        .await
        .map_err(|message| {
            self.failure(
                VoteRecoveryFailureKind::InvariantViolation,
                Some(work.clone()),
                None,
                None,
                message,
                request,
            )
        })?
        else {
            return self.cancelled(Some(work), None, request);
        };

        // Another caller may have completed the originally selected work while
        // this pass waited for the round lock. Select again under the lock.
        let locked_plan = resume_plan(&self.database, request.round_id, request.proposal_ids)
            .map_err(|error| self.voting_failure(error, Some(work.clone()), request))?;
        let Some(work) = locked_plan.recovered_vote_work.first().cloned() else {
            return Ok(VoteRecoveryAdvance {
                attempted_work: None,
                disposition: VoteRecoveryDisposition::NoWork,
                chain_outcome: None,
                share_deliveries: Vec::new(),
                round_plan: locked_plan,
            });
        };
        if control.interrupted() {
            return self.cancelled(Some(work), None, request);
        }
        progress.report(VoteRecoveryProgress::Selected(work.clone()));

        let (votes, batch) = self
            .recover_work_votes(&work, request)
            .map_err(|error| self.voting_failure(error, Some(work.clone()), request))?;
        // Recovered work reconciles the chain before any helper-plan
        // preparation: the plan was made at cast time (or the vote predates
        // plans), and an open ballot must not keep an already-dispatched
        // vote from being polled or recovered. Plans are ensured per vote
        // after confirmation, right before delivery.
        let mut fleet_preflight = None;

        let mut chain_outcome = None;
        if !matches!(work.kind, VoteRecoveryWorkKind::SubmitShares) {
            let round_id = parse_round_id(request.round_id)
                .map_err(|error| self.voting_failure(error, Some(work.clone()), request))?;
            let chain_request = match work.kind {
                VoteRecoveryWorkKind::AdvanceVote => ChainAdvanceRequest::Vote(AdvanceVote {
                    vote_round_id: round_id,
                    bundle_index: work.bundle_index,
                    proposal_id: work.proposal_id,
                }),
                VoteRecoveryWorkKind::AdvanceVoteBatch => {
                    let batch = batch.as_ref().ok_or_else(|| {
                        self.failure(
                            VoteRecoveryFailureKind::InvariantViolation,
                            Some(work.clone()),
                            None,
                            None,
                            "atomic vote recovery did not retain its durable batch",
                            request,
                        )
                    })?;
                    ChainAdvanceRequest::VoteBatch(AdvanceVoteBatch {
                        vote_round_id: round_id,
                        bundle_index: work.bundle_index,
                        ordered_batch_digest: batch.batch_digest,
                        ordered_proposal_ids: batch
                            .commitments
                            .iter()
                            .map(|commitment| commitment.proposal_id)
                            .collect(),
                    })
                }
                VoteRecoveryWorkKind::SubmitShares => unreachable!("handled above"),
            };
            // One exact-tree pass, bound to the epoch this pass began under so
            // an epoch change during helper-plan persistence is observed by
            // the chain episode instead of adopted by it.
            let outcome = self
                .chain_client
                .advance_until_terminal_in_epoch(
                    chain_request,
                    &ChainAdvancePolicy {
                        initial_recovery_mode: ChainRecoveryMode::ExactTree,
                        max_passes: 1,
                        ..ChainAdvancePolicy::default()
                    },
                    control.chain(),
                    control.entry_epoch(),
                )
                .await
                .map(ChainAdvanceOutcome::into_result)
                .map_err(|failure| self.chain_failure(failure, work.clone(), request))?;
            progress.report(VoteRecoveryProgress::ChainOutcome(outcome.clone()));
            chain_outcome = Some(outcome.clone());
            match outcome {
                ChainSubmissionResult::Pending(_) => {
                    return self.advance_result(
                        work,
                        VoteRecoveryDisposition::Pending,
                        chain_outcome,
                        Vec::new(),
                        request,
                    );
                }
                ChainSubmissionResult::Cancelled => {
                    return self.cancelled(Some(work), chain_outcome, request);
                }
                ChainSubmissionResult::SubmittedWithoutHash(diagnostic) => {
                    return Err(self.failure(
                        VoteRecoveryFailureKind::ChainTerminal,
                        Some(work),
                        None,
                        chain_outcome,
                        diagnostic.message(),
                        request,
                    ));
                }
                ChainSubmissionResult::Rejected(diagnostic) => {
                    return Err(self.failure(
                        VoteRecoveryFailureKind::ChainTerminal,
                        Some(work),
                        None,
                        chain_outcome,
                        diagnostic.message(),
                        request,
                    ));
                }
                ChainSubmissionResult::Confirmed(_) => {}
            }
        }

        let mut deliveries = Vec::with_capacity(votes.len());
        for vote in votes {
            if control.interrupted() {
                return self.advance_result(
                    work,
                    VoteRecoveryDisposition::Cancelled,
                    chain_outcome,
                    deliveries,
                    request,
                );
            }
            // Confirmation updates the durable recovery generation. Always
            // recover a fresh handle before deriving share payloads.
            let vote = CommittedVote::recover(
                &self.database,
                request.round_id,
                vote.bundle_index(),
                vote.proposal_id(),
            )
            .map_err(|error| {
                self.voting_failure_after_chain(
                    error,
                    Some(work.clone()),
                    chain_outcome.clone(),
                    request,
                )
                .with_share_deliveries(deliveries.clone())
            })?;
            if fleet_preflight.is_none() {
                let preflight = self
                    .helper_client
                    .preflight_fleet(request.configured_helper_urls)
                    .await
                    .map_err(|error| {
                        self.voting_failure_after_chain(
                            error,
                            Some(work.clone()),
                            chain_outcome.clone(),
                            request,
                        )
                        .with_share_deliveries(deliveries.clone())
                    })?;
                fleet_preflight = Some(preflight);
            }
            let preflight = fleet_preflight
                .as_ref()
                .expect("preflight was just taken for recovered work");
            vote.prepare_share_delivery(
                &self.database,
                ShareDeliveryPlanningParams {
                    fleet: preflight,
                    now_seconds: request.now_seconds,
                    vote_end_time_seconds: request.vote_end_time_seconds,
                    last_moment_buffer_seconds: request.last_moment_buffer_seconds,
                    proposal_ids: request.proposal_ids,
                },
            )
            .map_err(|error| {
                self.voting_failure_after_chain(
                    error,
                    Some(work.clone()),
                    chain_outcome.clone(),
                    request,
                )
                .with_share_deliveries(deliveries.clone())
            })?;
            progress.report(VoteRecoveryProgress::HelperPlansPrepared(vec![vote_key(
                &vote,
            )]));
            let vote = vote
                .confirmed(&self.database)
                .map_err(|error| {
                    self.voting_failure_after_chain(error, Some(work.clone()), chain_outcome.clone(), request)
                        .with_share_deliveries(deliveries.clone())
                })?
                .ok_or_else(|| {
                    self.failure(
                        VoteRecoveryFailureKind::InvariantViolation,
                        Some(work.clone()),
                        None,
                        chain_outcome.clone(),
                        "vote was reported confirmed but its recovery material has no tree position",
                        request,
                    )
                    .with_share_deliveries(deliveries.clone())
                })?;
            let cancel = || control.interrupted();
            let delivery = match vote
                .submit_prepared_shares_keeping_partial_report(
                    &self.database,
                    &self.helper_client,
                    ShareDeliverySubmissionParams {
                        configured_server_urls: request.configured_helper_urls,
                        now_seconds: request.now_seconds,
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
                        progress.report(VoteRecoveryProgress::ShareOutcome(report.clone()));
                        deliveries.push(report);
                    }
                    return Err(self
                        .voting_failure_after_chain(
                            failure.error,
                            Some(work),
                            chain_outcome,
                            request,
                        )
                        .with_share_deliveries(deliveries));
                }
            };
            let report = VoteShareDeliveryReport {
                vote: vote_key(vote.vote()),
                delivery,
            };
            progress.report(VoteRecoveryProgress::ShareOutcome(report.clone()));
            if report.delivery.cancelled {
                deliveries.push(report);
                return self.advance_result(
                    work,
                    VoteRecoveryDisposition::Cancelled,
                    chain_outcome,
                    deliveries,
                    request,
                );
            }
            if !report.delivery.pending_share_indices.is_empty()
                || report.delivery.deliveries.iter().any(|delivery| {
                    delivery.submission.accepted_urls.is_empty()
                        && delivery.submission.ambiguous_urls.is_empty()
                })
            {
                deliveries.push(report);
                return Err(self
                    .failure(
                        VoteRecoveryFailureKind::HelperDeliveryIncomplete,
                        Some(work),
                        None,
                        chain_outcome,
                        "helper delivery ended with pending shares",
                        request,
                    )
                    .with_share_deliveries(deliveries));
            }
            deliveries.push(report);
        }

        self.advance_result(
            work,
            VoteRecoveryDisposition::Advanced,
            chain_outcome,
            deliveries,
            request,
        )
    }

    pub(super) fn recover_work_votes(
        &self,
        work: &crate::session::VoteRecoveryWork,
        request: VoteRecoveryRequest<'_>,
    ) -> Result<(Vec<CommittedVote>, Option<crate::vote::SignedVoteBatch>), VotingError> {
        match work.kind {
            VoteRecoveryWorkKind::AdvanceVote | VoteRecoveryWorkKind::SubmitShares => Ok((
                vec![CommittedVote::recover(
                    &self.database,
                    request.round_id,
                    work.bundle_index,
                    work.proposal_id,
                )?],
                None,
            )),
            VoteRecoveryWorkKind::AdvanceVoteBatch => {
                let batch = recover_atomic_vote_batch(
                    &self.database,
                    request.round_id,
                    work.bundle_index,
                    work.proposal_id,
                )?;
                let votes = batch
                    .commitments
                    .iter()
                    .map(|commitment| {
                        CommittedVote::recover(
                            &self.database,
                            request.round_id,
                            work.bundle_index,
                            commitment.proposal_id,
                        )
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok((votes, Some(batch)))
            }
        }
    }

    fn advance_result(
        &self,
        work: crate::session::VoteRecoveryWork,
        disposition: VoteRecoveryDisposition,
        chain_outcome: Option<ChainSubmissionResult>,
        share_deliveries: Vec<VoteShareDeliveryReport>,
        request: VoteRecoveryRequest<'_>,
    ) -> Result<VoteRecoveryAdvance, VoteRecoveryFailure> {
        let round_plan = resume_plan(&self.database, request.round_id, request.proposal_ids)
            .map_err(|error| {
                self.voting_failure_after_chain(
                    error,
                    Some(work.clone()),
                    chain_outcome.clone(),
                    request,
                )
                .with_share_deliveries(share_deliveries.clone())
            })?;
        Ok(VoteRecoveryAdvance {
            attempted_work: Some(work),
            disposition,
            chain_outcome,
            share_deliveries,
            round_plan,
        })
    }

    fn cancelled(
        &self,
        work: Option<crate::session::VoteRecoveryWork>,
        chain_outcome: Option<ChainSubmissionResult>,
        request: VoteRecoveryRequest<'_>,
    ) -> Result<VoteRecoveryAdvance, VoteRecoveryFailure> {
        let round_plan = resume_plan(&self.database, request.round_id, request.proposal_ids)
            .map_err(|error| {
                self.voting_failure_after_chain(error, work.clone(), chain_outcome.clone(), request)
            })?;
        Ok(VoteRecoveryAdvance {
            attempted_work: work,
            disposition: VoteRecoveryDisposition::Cancelled,
            chain_outcome,
            share_deliveries: Vec::new(),
            round_plan,
        })
    }

    fn voting_failure(
        &self,
        error: VotingError,
        work: Option<crate::session::VoteRecoveryWork>,
        request: VoteRecoveryRequest<'_>,
    ) -> VoteRecoveryFailure {
        self.voting_failure_after_chain(error, work, None, request)
    }

    /// [`Self::voting_failure`] for an error raised after the chain already
    /// produced `chain_outcome`, which stays on the failure so a durable
    /// confirmation is not lost behind a later delivery error.
    fn voting_failure_after_chain(
        &self,
        error: VotingError,
        work: Option<crate::session::VoteRecoveryWork>,
        chain_outcome: Option<ChainSubmissionResult>,
        request: VoteRecoveryRequest<'_>,
    ) -> VoteRecoveryFailure {
        let kind = match error.kind() {
            VotingErrorKind::InvalidInput
            | VotingErrorKind::InsufficientEligibility
            | VotingErrorKind::NoSpendableNotes
            | VotingErrorKind::SetupAlreadyPersisted => VoteRecoveryFailureKind::InvalidInput,
            VotingErrorKind::Busy | VotingErrorKind::DbBusy => VoteRecoveryFailureKind::Busy,
            VotingErrorKind::Storage => VoteRecoveryFailureKind::Storage,
            VotingErrorKind::PirUnavailable => VoteRecoveryFailureKind::Transport,
            VotingErrorKind::KeystoneSignatureConflict
            | VotingErrorKind::ProofFailed
            | VotingErrorKind::Internal => VoteRecoveryFailureKind::InvariantViolation,
        };
        self.failure(kind, work, None, chain_outcome, error.to_string(), request)
    }

    fn chain_failure(
        &self,
        error: ChainSubmissionFailure,
        work: crate::session::VoteRecoveryWork,
        request: VoteRecoveryRequest<'_>,
    ) -> VoteRecoveryFailure {
        let kind = match error.kind() {
            ChainSubmissionFailureKind::InvalidInput => VoteRecoveryFailureKind::InvalidInput,
            ChainSubmissionFailureKind::InvariantViolation => {
                VoteRecoveryFailureKind::InvariantViolation
            }
            ChainSubmissionFailureKind::Storage => VoteRecoveryFailureKind::Storage,
            ChainSubmissionFailureKind::Transport => VoteRecoveryFailureKind::Transport,
            ChainSubmissionFailureKind::Protocol => VoteRecoveryFailureKind::Protocol,
        };
        self.failure(
            kind,
            Some(work),
            error.strongest_state(),
            None,
            error.message(),
            request,
        )
    }

    fn failure(
        &self,
        kind: VoteRecoveryFailureKind,
        work: Option<crate::session::VoteRecoveryWork>,
        strongest_chain_state: Option<crate::ChainSubmissionFailureState>,
        chain_outcome: Option<ChainSubmissionResult>,
        message: impl AsRef<str>,
        request: VoteRecoveryRequest<'_>,
    ) -> VoteRecoveryFailure {
        VoteRecoveryFailure {
            kind,
            attempted_work: work,
            strongest_chain_state,
            chain_outcome,
            message: bounded_message(message.as_ref()),
            round_plan: resume_plan(&self.database, request.round_id, request.proposal_ids)
                .ok()
                .map(Box::new),
            share_deliveries: Vec::new(),
        }
    }
}

pub(super) fn vote_key(vote: &CommittedVote) -> VoteRecoveryKey {
    VoteRecoveryKey {
        bundle_index: vote.bundle_index(),
        proposal_id: vote.proposal_id(),
    }
}

pub(super) fn parse_round_id(round_id: &str) -> Result<[u8; 32], VotingError> {
    crate::types::validate_vote_round_id_hex(round_id)?;
    let bytes = hex::decode(round_id).map_err(|error| VotingError::InvalidInput {
        message: format!("vote_round_id is not valid hex: {error}"),
    })?;
    bytes
        .try_into()
        .map_err(|bytes: Vec<u8>| VotingError::InvalidInput {
            message: format!("vote_round_id must be 32 bytes, got {}", bytes.len()),
        })
}

pub(super) fn bounded_message(message: &str) -> String {
    let mut bounded =
        String::with_capacity(message.len().min(MAX_CHAIN_SUBMISSION_DIAGNOSTIC_BYTES));
    for character in message.chars() {
        let escaped = character.escape_default().collect::<String>();
        if bounded.len() + escaped.len() > MAX_CHAIN_SUBMISSION_DIAGNOSTIC_BYTES {
            break;
        }
        bounded.push_str(&escaped);
    }
    bounded
}
