//! Step execution for [`RoundExecutor`].
//!
//! Every step runs under a lock, re-plans from durable state, and reports
//! progress at durable and network boundaries. Proving never runs on the
//! async runtime: delegation and vote proofs run on dedicated large-stack
//! threads and stream their progress back through channels.

use std::sync::Arc;

use crate::{
    delegate::DelegationProgress,
    session::{resume_plan, NextStep, RoundPlan, VoteRecoveryWork, VoteRecoveryWorkKind},
    share_tracking::{
        confirm_pending_share, ShareConfirmationParams, ShareDeliveryPlanningParams,
        ShareDeliverySubmissionParams, ShareKey,
    },
    types::{DelegationProgressBridge, VoteCommitStageBridge},
    vote::{
        persist_prepared_vote_work, prepare_vote_work, validate_draft_votes, CommittedVote,
        SignedVoteBatch, VoteCommitStage, VoteCommitmentRecovery, VoteSigner, VoteWorkRequest,
    },
    wire::DraftVote,
    AdvanceDelegation, AdvanceImportedDelegation, AdvanceVote, AdvanceVoteBatch,
    ChainAdvanceOutcome, ChainAdvancePolicy, ChainAdvanceRequest, ChainRecoveryMode,
    ChainSubmissionControl, ChainSubmissionFailure, ChainSubmissionFailureKind,
    ChainSubmissionResult, ChainTransport, VotingError, VotingErrorKind, VotingHotkey,
};

use super::{
    execution::{bounded_message, parse_round_id, vote_key},
    round_lock, BallotIntent, RoundExecutor, RoundHostContext, RoundStepDisposition,
    RoundStepFailure, RoundStepFailureKind, RoundStepOutcome, RoundStepProgress,
    RoundStepProgressReporter, VoteRecoveryRequest, VoteShareDeliveryReport,
};

// Matches the keygen warm-up threads in voting-circuits.
const PROVING_STACK_BYTES: usize = 64 * 1024 * 1024;

impl<T: ChainTransport> RoundExecutor<T> {
    /// Plans the bound round from durable state.
    pub fn plan(&self) -> Result<RoundPlan, VotingError> {
        let binding = self.binding()?;
        resume_plan(&self.database, &binding.round_id, &binding.proposal_ids())
    }

    /// Records ballot decisions and returns the refreshed plan.
    ///
    /// Option counts come from the bound roster, so a decision for an unknown
    /// proposal is rejected before anything is written. The whole batch is
    /// resolved against the roster first and then written in one transaction,
    /// so a rejected batch leaves durable intent unchanged.
    pub fn set_ballot_intents(&self, intents: &[BallotIntent]) -> Result<RoundPlan, VotingError> {
        let binding = self.binding()?;
        let resolved = intents
            .iter()
            .map(|intent| {
                let num_options = binding.num_options(intent.proposal_id).ok_or_else(|| {
                    VotingError::InvalidInput {
                        message: format!(
                            "proposal {} is not in the round roster",
                            intent.proposal_id
                        ),
                    }
                })?;
                Ok((intent.proposal_id, intent.decision, num_options))
            })
            .collect::<Result<Vec<_>, VotingError>>()?;
        self.database
            .set_ballot_intents(&binding.round_id, &resolved)?;
        self.plan()
    }

    /// Runs the first planned step, if any.
    pub async fn advance_next(
        &self,
        host: &RoundHostContext,
        control: &ChainSubmissionControl,
        progress: &dyn RoundStepProgressReporter,
    ) -> Result<RoundStepOutcome, RoundStepFailure> {
        let plan = self
            .plan()
            .map_err(|error| self.step_voting_failure(error, None))?;
        let Some(step) = plan.next_steps.first().cloned() else {
            return Ok(self.no_work(None, plan));
        };
        self.advance_step(step, host, control, progress).await
    }

    /// Runs one planned step by one bounded pass.
    ///
    /// The step is re-validated against a fresh plan under the lock; a step
    /// another pass already completed returns `NoWork`. `Delegate` and
    /// `AdvanceDelegation` lock their bundle; every other step locks the
    /// round.
    pub async fn advance_step(
        &self,
        step: NextStep,
        host: &RoundHostContext,
        control: &ChainSubmissionControl,
        progress: &dyn RoundStepProgressReporter,
    ) -> Result<RoundStepOutcome, RoundStepFailure> {
        let round_id = self
            .binding()
            .map(|binding| binding.round_id.clone())
            .map_err(|error| self.step_voting_failure(error, Some(step.clone())))?;
        let scope = match &step {
            NextStep::Delegate { bundle_index } | NextStep::AdvanceDelegation { bundle_index } => {
                Some(*bundle_index)
            }
            _ => None,
        };
        let Some(_guard) =
            round_lock::acquire(self.database.wallet_id(), &round_id, scope, control)
                .await
                .map_err(|message| {
                    self.step_failure(
                        RoundStepFailureKind::InvariantViolation,
                        Some(step.clone()),
                        None,
                        None,
                        message,
                    )
                })?
        else {
            return self.step_cancelled(Some(step), None, Vec::new());
        };

        let plan = self
            .plan()
            .map_err(|error| self.step_voting_failure(error, Some(step.clone())))?;
        if !plan.next_steps.contains(&step) {
            return Ok(self.no_work(Some(step), plan));
        }
        if control.is_cancelled() {
            return self.step_cancelled(Some(step), None, Vec::new());
        }
        progress.report(RoundStepProgress::Selected(step.clone()));

        match step.clone() {
            NextStep::Delegate { bundle_index } => {
                self.run_delegate(step, bundle_index, host, control, progress)
                    .await
            }
            NextStep::AdvanceDelegation { bundle_index } => {
                self.run_advance_delegation(step, bundle_index, host, control, progress)
                    .await
            }
            NextStep::AdvanceImportedDelegation { bundle_index } => {
                let request = AdvanceImportedDelegation {
                    vote_round_id: self.round_id_bytes(&step)?,
                    bundle_index,
                };
                let outcome = self
                    .chain_client
                    .advance_until_terminal(
                        ChainAdvanceRequest::ImportedDelegation(request),
                        &persisted_policy(host),
                        control,
                    )
                    .await
                    .map_err(|failure| self.step_chain_failure(failure, Some(step.clone())))?;
                self.chain_step_outcome(step, outcome, None, progress)
            }
            NextStep::CastVote { bundle_index, .. } => {
                self.run_cast_vote(step, bundle_index, &plan, host, control, progress)
                    .await
            }
            NextStep::AdvanceVote {
                bundle_index,
                proposal_id,
            }
            | NextStep::AdvanceVoteBatch {
                bundle_index,
                proposal_id,
            }
            | NextStep::SubmitShares {
                bundle_index,
                proposal_id,
                ..
            } => {
                let kind = match &step {
                    NextStep::AdvanceVote { .. } => VoteRecoveryWorkKind::AdvanceVote,
                    NextStep::AdvanceVoteBatch { .. } => VoteRecoveryWorkKind::AdvanceVoteBatch,
                    _ => VoteRecoveryWorkKind::SubmitShares,
                };
                self.run_persisted_vote_work(
                    step,
                    kind,
                    bundle_index,
                    proposal_id,
                    host,
                    control,
                    progress,
                )
                .await
            }
            NextStep::ConfirmShare {
                bundle_index,
                proposal_id,
                share_index,
            } => {
                self.run_confirm_share(
                    step,
                    ShareKey {
                        bundle_index,
                        proposal_id,
                        share_index,
                    },
                    host,
                    control,
                    progress,
                )
                .await
            }
        }
    }

    fn round_id_bytes(&self, step: &NextStep) -> Result<[u8; 32], RoundStepFailure> {
        let round_id = self
            .binding()
            .map(|binding| binding.round_id.clone())
            .map_err(|error| self.step_voting_failure(error, Some(step.clone())))?;
        parse_round_id(&round_id)
            .map_err(|error| self.step_voting_failure(error, Some(step.clone())))
    }

    async fn run_delegate(
        &self,
        step: NextStep,
        bundle_index: u32,
        host: &RoundHostContext,
        control: &ChainSubmissionControl,
        progress: &dyn RoundStepProgressReporter,
    ) -> Result<RoundStepOutcome, RoundStepFailure> {
        let inputs = self.delegation_inputs(&step, host)?;
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let driver = Arc::clone(&inputs.driver);
        let signer = inputs.signer.clone();
        let pir = Arc::clone(&inputs.pir);
        std::thread::Builder::new()
            .name("voting-delegation-step".to_string())
            .stack_size(PROVING_STACK_BYTES)
            .spawn(move || {
                let reporter = DelegationProgressBridge::new(move |progress| {
                    let _ = progress_tx.send(progress);
                });
                let result = driver.prove_and_sign_blocking(bundle_index, &signer, &pir, &reporter);
                let _ = done_tx.send(result);
            })
            .map_err(|error| {
                self.step_failure(
                    RoundStepFailureKind::InvariantViolation,
                    Some(step.clone()),
                    None,
                    None,
                    format!("failed to spawn delegation thread: {error}"),
                )
            })?;
        tokio::pin!(done_rx);
        let signed = loop {
            tokio::select! {
                Some(update) = progress_rx.recv() => {
                    progress.report(RoundStepProgress::Delegation { bundle_index, progress: update });
                }
                result = &mut done_rx => {
                    break result.map_err(|_| {
                        self.step_failure(
                            RoundStepFailureKind::InvariantViolation,
                            Some(step.clone()),
                            None,
                            None,
                            "delegation thread exited without a result",
                        )
                    })?;
                }
            }
        };
        while let Ok(update) = progress_rx.try_recv() {
            progress.report(RoundStepProgress::Delegation {
                bundle_index,
                progress: update,
            });
        }
        let signed = signed.map_err(|error| self.step_voting_failure(error, Some(step.clone())))?;
        progress.report(RoundStepProgress::Delegation {
            bundle_index,
            progress: DelegationProgress::PayloadReady,
        });
        if control.is_cancelled() {
            // The signed payload is durable; the next pass re-dispatches it
            // through AdvanceDelegation without proving again.
            return self.step_cancelled(Some(step), None, Vec::new());
        }
        let request = AdvanceDelegation {
            vote_round_id: self.round_id_bytes(&step)?,
            bundle_index,
            spend_auth_signature: signed.submission.spend_auth_sig,
        };
        let outcome = self
            .chain_client
            .advance_until_terminal(
                ChainAdvanceRequest::Delegation(request),
                &host.chain_policy,
                control,
            )
            .await
            .map_err(|failure| self.step_chain_failure(failure, Some(step.clone())))?;
        self.chain_step_outcome(step, outcome, Some(signed), progress)
    }

    async fn run_advance_delegation(
        &self,
        step: NextStep,
        bundle_index: u32,
        host: &RoundHostContext,
        control: &ChainSubmissionControl,
        progress: &dyn RoundStepProgressReporter,
    ) -> Result<RoundStepOutcome, RoundStepFailure> {
        let inputs = self.delegation_inputs(&step, host)?;
        let driver = Arc::clone(&inputs.driver);
        let signer = inputs.signer.clone();
        let signature =
            tokio::task::spawn_blocking(move || driver.resign_blocking(bundle_index, &signer))
                .await
                .map_err(|error| {
                    self.step_failure(
                        RoundStepFailureKind::InvariantViolation,
                        Some(step.clone()),
                        None,
                        None,
                        format!("delegation signing task failed: {error}"),
                    )
                })?
                .map_err(|error| self.step_voting_failure(error, Some(step.clone())))?;
        let request = AdvanceDelegation {
            vote_round_id: self.round_id_bytes(&step)?,
            bundle_index,
            spend_auth_signature: signature,
        };
        let outcome = self
            .chain_client
            .advance_until_terminal(
                ChainAdvanceRequest::Delegation(request),
                &persisted_policy(host),
                control,
            )
            .await
            .map_err(|failure| self.step_chain_failure(failure, Some(step.clone())))?;
        self.chain_step_outcome(step, outcome, None, progress)
    }

    fn delegation_inputs(
        &self,
        step: &NextStep,
        host: &RoundHostContext,
    ) -> Result<super::DelegationStepInputs, RoundStepFailure> {
        let inputs = host.delegation.clone().ok_or_else(|| {
            self.step_failure(
                RoundStepFailureKind::InvalidInput,
                Some(step.clone()),
                None,
                None,
                "delegation steps require RoundHostContext::delegation",
            )
        })?;
        let round_id = self
            .binding()
            .map(|binding| binding.round_id.clone())
            .map_err(|error| self.step_voting_failure(error, Some(step.clone())))?;
        if inputs.driver.round_id() != round_id {
            return Err(self.step_failure(
                RoundStepFailureKind::InvalidInput,
                Some(step.clone()),
                None,
                None,
                "delegation driver is bound to a different round",
            ));
        }
        Ok(inputs)
    }

    async fn run_cast_vote(
        &self,
        step: NextStep,
        bundle_index: u32,
        plan: &RoundPlan,
        host: &RoundHostContext,
        control: &ChainSubmissionControl,
        progress: &dyn RoundStepProgressReporter,
    ) -> Result<RoundStepOutcome, RoundStepFailure> {
        let binding = self
            .binding()
            .map_err(|error| self.step_voting_failure(error, Some(step.clone())))?;
        let single_share = host.is_last_moment();
        let mut drafts = Vec::new();
        for planned in &plan.next_steps {
            if let NextStep::CastVote {
                bundle_index: planned_bundle,
                proposal_id,
                choice,
            } = planned
            {
                if *planned_bundle != bundle_index {
                    continue;
                }
                let num_options = binding.num_options(*proposal_id).ok_or_else(|| {
                    self.step_failure(
                        RoundStepFailureKind::InvalidInput,
                        Some(step.clone()),
                        None,
                        None,
                        format!("proposal {proposal_id} is not in the round roster"),
                    )
                })?;
                drafts.push(DraftVote {
                    proposal_id: *proposal_id,
                    choice: *choice,
                    num_options,
                    vc_tree_position: 0,
                    single_share,
                });
            }
        }
        validate_draft_votes(&drafts)
            .map_err(|error| self.step_voting_failure(error, Some(step.clone())))?;
        let hotkey_secret = binding.hotkey_secret.clone().ok_or_else(|| {
            self.step_failure(
                RoundStepFailureKind::InvalidInput,
                Some(step.clone()),
                None,
                None,
                "casting a vote requires the round voting hotkey",
            )
        })?;
        let network = binding.network;
        let round_id = binding.round_id.clone();

        // Tree sync and witness generation block on their own HTTP runtime.
        // Nodes are tried in order; a failed sync resets the cached tree so
        // the next node starts from a consistent state.
        if host.vote_tree_node_urls.is_empty() {
            return Err(self.step_failure(
                RoundStepFailureKind::InvalidInput,
                Some(step),
                None,
                None,
                "casting a vote requires at least one vote-tree node URL",
            ));
        }
        let mut height = None;
        let mut last_failure = None;
        for (index, node_url) in host.vote_tree_node_urls.iter().enumerate() {
            let db = Arc::clone(&self.database);
            let round_id = round_id.clone();
            let node_url = node_url.clone();
            let transport = self.tree_transport.clone();
            let reset_first = index > 0;
            let synced = self
                .blocking(&step, "vote tree sync", move || {
                    if reset_first {
                        crate::precompute::reset_vote_tree(&db, &round_id)?;
                    }
                    match transport {
                        Some(transport) => crate::precompute::sync_vote_tree_with(
                            &db, &round_id, &node_url, transport,
                        ),
                        None => crate::precompute::sync_vote_tree(&db, &round_id, &node_url),
                    }
                })
                .await;
            match synced {
                Ok(synced) => {
                    height = Some(synced);
                    break;
                }
                Err(failure) => {
                    if control.is_cancelled() {
                        return Err(failure);
                    }
                    last_failure = Some(failure);
                }
            }
        }
        let Some(height) = height else {
            return Err(last_failure.expect("at least one node URL was tried"));
        };
        progress.report(RoundStepProgress::TreeSynced { height });
        let witness = {
            let db = Arc::clone(&self.database);
            let round_id = round_id.clone();
            self.blocking(&step, "VAN witness", move || {
                crate::precompute::van_witness(&db, &round_id, bundle_index, height)
            })
            .await?
        };
        if control.is_cancelled() {
            return self.step_cancelled(Some(step), None, Vec::new());
        }

        // Proving runs on a dedicated large-stack thread; stages stream back.
        let (stage_tx, mut stage_rx) = tokio::sync::mpsc::unbounded_channel::<VoteCommitStage>();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let db = Arc::clone(&self.database);
        let max_proof_concurrency = host.max_proof_concurrency.max(1);
        std::thread::Builder::new()
            .name("voting-vote-commit".to_string())
            .stack_size(PROVING_STACK_BYTES)
            .spawn(move || {
                let result = (|| {
                    let hotkey =
                        VotingHotkey::from_stored_secret(hotkey_secret.as_slice(), network)?;
                    let stages = VoteCommitStageBridge::new(move |stage| {
                        let _ = stage_tx.send(stage);
                    });
                    let prepared = prepare_vote_work(
                        &db,
                        VoteSigner::hotkey(&hotkey),
                        VoteWorkRequest {
                            round_id: &round_id,
                            bundle_index,
                            drafts: &drafts,
                            witness: &witness,
                            stages: &stages,
                            max_proof_concurrency,
                        },
                    )?;
                    persist_prepared_vote_work(&db, prepared)
                })();
                let _ = done_tx.send(result);
            })
            .map_err(|error| {
                self.step_failure(
                    RoundStepFailureKind::InvariantViolation,
                    Some(step.clone()),
                    None,
                    None,
                    format!("failed to spawn vote commit thread: {error}"),
                )
            })?;
        tokio::pin!(done_rx);
        let recovery = loop {
            tokio::select! {
                Some(stage) = stage_rx.recv() => {
                    progress.report(RoundStepProgress::VoteCommit(stage));
                }
                result = &mut done_rx => {
                    break result.map_err(|_| {
                        self.step_failure(
                            RoundStepFailureKind::InvariantViolation,
                            Some(step.clone()),
                            None,
                            None,
                            "vote commit thread exited without a result",
                        )
                    })?;
                }
            }
        };
        while let Ok(stage) = stage_rx.try_recv() {
            progress.report(RoundStepProgress::VoteCommit(stage));
        }
        let recovery =
            recovery.map_err(|error| self.step_voting_failure(error, Some(step.clone())))?;

        let round_id = self
            .binding()
            .map(|b| b.round_id.clone())
            .unwrap_or_default();
        let (votes, batch) = self
            .recover_committed(&round_id, recovery)
            .map_err(|error| self.step_voting_failure(error, Some(step.clone())))?;
        self.finish_vote_work(
            step,
            votes,
            batch,
            true,
            &host.chain_policy,
            host,
            control,
            progress,
        )
        .await
    }

    fn recover_committed(
        &self,
        round_id: &str,
        recovery: VoteCommitmentRecovery,
    ) -> Result<(Vec<CommittedVote>, Option<SignedVoteBatch>), VotingError> {
        let bundle_index = recovery.bundle_index();
        let votes = recovery
            .proposal_ids()
            .into_iter()
            .map(|proposal_id| {
                CommittedVote::recover(&self.database, round_id, bundle_index, proposal_id)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let batch = match recovery {
            VoteCommitmentRecovery::AtomicBatch(batch) => Some(batch),
            _ => None,
        };
        Ok((votes, batch))
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_persisted_vote_work(
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
    async fn finish_vote_work(
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
            return self.step_cancelled(Some(step), None, Vec::new());
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
                    return self.step_cancelled(Some(step), chain_outcome, Vec::new());
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
                return self.step_cancelled(Some(step), chain_outcome, deliveries);
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
                return self.step_cancelled(Some(step), chain_outcome, deliveries);
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

    async fn run_confirm_share(
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

    fn chain_step_outcome(
        &self,
        step: NextStep,
        outcome: ChainAdvanceOutcome,
        delegation: Option<crate::delegate::SignedDelegationBundle>,
        progress: &dyn RoundStepProgressReporter,
    ) -> Result<RoundStepOutcome, RoundStepFailure> {
        let disposition = match &outcome {
            ChainAdvanceOutcome::Confirmed(_) => RoundStepDisposition::Advanced,
            ChainAdvanceOutcome::StillPending(_) => RoundStepDisposition::Pending,
            ChainAdvanceOutcome::Cancelled => RoundStepDisposition::Cancelled,
            ChainAdvanceOutcome::SubmittedWithoutHash(_) | ChainAdvanceOutcome::Rejected(_) => {
                RoundStepDisposition::ChainTerminal
            }
        };
        let result = outcome.into_result();
        progress.report(RoundStepProgress::ChainOutcome(result.clone()));
        self.outcome(step, disposition, Some(result), Vec::new(), delegation)
    }

    async fn blocking<R: Send + 'static>(
        &self,
        step: &NextStep,
        label: &str,
        work: impl FnOnce() -> Result<R, VotingError> + Send + 'static,
    ) -> Result<R, RoundStepFailure> {
        tokio::task::spawn_blocking(work)
            .await
            .map_err(|error| {
                self.step_failure(
                    RoundStepFailureKind::InvariantViolation,
                    Some(step.clone()),
                    None,
                    None,
                    format!("{label} task failed: {error}"),
                )
            })?
            .map_err(|error| self.step_voting_failure(error, Some(step.clone())))
    }

    fn no_work(&self, step: Option<NextStep>, plan: RoundPlan) -> RoundStepOutcome {
        RoundStepOutcome {
            step,
            disposition: RoundStepDisposition::NoWork,
            chain_outcome: None,
            share_deliveries: Vec::new(),
            delegation: None,
            plan,
        }
    }

    fn outcome(
        &self,
        step: NextStep,
        disposition: RoundStepDisposition,
        chain_outcome: Option<ChainSubmissionResult>,
        share_deliveries: Vec<VoteShareDeliveryReport>,
        delegation: Option<crate::delegate::SignedDelegationBundle>,
    ) -> Result<RoundStepOutcome, RoundStepFailure> {
        let plan = self
            .plan()
            .map_err(|error| self.step_voting_failure(error, Some(step.clone())))?;
        Ok(RoundStepOutcome {
            step: Some(step),
            disposition,
            chain_outcome,
            share_deliveries,
            delegation,
            plan,
        })
    }

    fn step_cancelled(
        &self,
        step: Option<NextStep>,
        chain_outcome: Option<ChainSubmissionResult>,
        share_deliveries: Vec<VoteShareDeliveryReport>,
    ) -> Result<RoundStepOutcome, RoundStepFailure> {
        let plan = self
            .plan()
            .map_err(|error| self.step_voting_failure(error, step.clone()))?;
        Ok(RoundStepOutcome {
            step,
            disposition: RoundStepDisposition::Cancelled,
            chain_outcome,
            share_deliveries,
            delegation: None,
            plan,
        })
    }

    fn step_voting_failure(&self, error: VotingError, step: Option<NextStep>) -> RoundStepFailure {
        let kind = match error.kind() {
            VotingErrorKind::InvalidInput
            | VotingErrorKind::InsufficientEligibility
            | VotingErrorKind::NoSpendableNotes
            | VotingErrorKind::SetupAlreadyPersisted => RoundStepFailureKind::InvalidInput,
            VotingErrorKind::Busy | VotingErrorKind::DbBusy => RoundStepFailureKind::Busy,
            VotingErrorKind::Storage => RoundStepFailureKind::Storage,
            VotingErrorKind::PirUnavailable => RoundStepFailureKind::Transport,
            VotingErrorKind::ProofFailed => RoundStepFailureKind::ProofFailed,
            VotingErrorKind::KeystoneSignatureConflict => RoundStepFailureKind::Signing,
            VotingErrorKind::Internal => RoundStepFailureKind::InvariantViolation,
        };
        self.step_failure(kind, step, None, None, error.to_string())
    }

    fn step_chain_failure(
        &self,
        error: ChainSubmissionFailure,
        step: Option<NextStep>,
    ) -> RoundStepFailure {
        let kind = match error.kind() {
            ChainSubmissionFailureKind::InvalidInput => RoundStepFailureKind::InvalidInput,
            ChainSubmissionFailureKind::InvariantViolation => {
                RoundStepFailureKind::InvariantViolation
            }
            ChainSubmissionFailureKind::Storage => RoundStepFailureKind::Storage,
            ChainSubmissionFailureKind::Transport => RoundStepFailureKind::Transport,
            ChainSubmissionFailureKind::Protocol => RoundStepFailureKind::Protocol,
        };
        self.step_failure(kind, step, error.strongest_state(), None, error.message())
    }

    fn step_failure(
        &self,
        kind: RoundStepFailureKind,
        step: Option<NextStep>,
        strongest_chain_state: Option<crate::ChainSubmissionFailureState>,
        chain_outcome: Option<ChainSubmissionResult>,
        message: impl AsRef<str>,
    ) -> RoundStepFailure {
        RoundStepFailure {
            kind,
            step,
            strongest_chain_state,
            chain_outcome,
            message: bounded_message(message.as_ref()),
            plan: self.plan().ok().map(Box::new),
        }
    }
}

/// Persisted work always reconciles through the exact tree from its first
/// pass, as the resume planner requires; the host's cadence still applies.
fn persisted_policy(host: &RoundHostContext) -> ChainAdvancePolicy {
    ChainAdvancePolicy {
        initial_recovery_mode: ChainRecoveryMode::ExactTree,
        ..host.chain_policy.clone()
    }
}
