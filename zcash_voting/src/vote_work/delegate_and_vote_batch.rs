//! Executes a fresh delegation and its dependent casts as one durable unit.

use std::sync::Arc;

use super::{
    round_lock::HeldRoundLock, step_ledger::StepLedger, step_scope::StepScope,
    steps::PROVING_STACK_BYTES, vote_completion::CompletionEntry, RoundExecutor, RoundStepFailure,
    RoundStepFailureKind, RoundStepOutcome, RoundStepProgress, RoundStepProgressReporter,
};
use crate::{
    vote::{DraftVote, VoteSigner},
    ChainTransport, VotingHotkey,
};

impl<T: ChainTransport> RoundExecutor<T> {
    pub(super) async fn run_delegate_and_vote_batch(
        &self,
        scope: &StepScope<'_>,
        bundle_index: u32,
        drafts: Vec<DraftVote>,
        lock: &HeldRoundLock,
        progress: &dyn RoundStepProgressReporter,
    ) -> Result<RoundStepOutcome, RoundStepFailure> {
        let ledger = StepLedger::default();
        let inputs = self.delegation_inputs(scope)?;
        let secret = scope.hotkey_secret.clone().ok_or_else(|| {
            self.step_failure(
                RoundStepFailureKind::InvalidInput,
                Some(&scope.step),
                None,
                &ledger,
                "combined casting requires the voting hotkey",
            )
        })?;
        let db = Arc::clone(&self.database);
        let round_id = scope.round_id.clone();
        let network = scope.network;
        let concurrency = scope.host.max_proof_concurrency.max(1);
        let held_lock = Arc::clone(lock);
        let observations = scope.observations.clone();
        let (updates, mut events) = tokio::sync::mpsc::unbounded_channel();
        let (complete, mut completion) = tokio::sync::oneshot::channel();
        std::thread::Builder::new()
            .name("voting-delegate-cast".to_string())
            .stack_size(PROVING_STACK_BYTES)
            .spawn(move || {
                let _held_lock = held_lock;
                let mut signed_delegation = None;
                let outcome = (|| {
                    let delegation_updates = updates.clone();
                    let delegation_progress =
                        crate::types::DelegationProgressBridge::new(move |progress| {
                            let _ = delegation_updates.send(RoundStepProgress::Delegation {
                                bundle_index,
                                progress,
                            });
                        });
                    let signed = inputs.driver.prove_and_sign_blocking_observed(
                        bundle_index,
                        &inputs.signer,
                        &inputs.pir,
                        &delegation_progress,
                        &observations,
                    )?;
                    signed_delegation = Some(signed.clone());
                    let hotkey = VotingHotkey::from_stored_secret(secret.as_slice(), network)?;
                    let vote_progress = crate::types::VoteCommitStageBridge::new(move |stage| {
                        let _ = updates.send(RoundStepProgress::VoteCommit(stage));
                    });
                    let prepared = crate::vote::prepare_delegate_and_vote_batch(
                        &db,
                        VoteSigner::hotkey(&hotkey),
                        crate::delegate_and_vote_batch::DelegateAndVoteBatchRequest {
                            round_id: &round_id,
                            bundle_index,
                            drafts: &drafts,
                            spend_auth_signature: signed.submission.spend_auth_sig,
                            stages: &vote_progress,
                            max_proof_concurrency: concurrency,
                        },
                        &observations,
                    )?;
                    let batch = crate::vote::observe_persist_prepared_atomic_vote_batch(
                        &db,
                        prepared,
                        &observations,
                    )?;
                    Ok::<_, crate::VotingError>(batch)
                })();
                let _ = complete.send((signed_delegation, outcome));
            })
            .map_err(|error| {
                self.step_failure(
                    RoundStepFailureKind::InvariantViolation,
                    Some(&scope.step),
                    None,
                    &ledger,
                    format!("spawn combined prover: {error}"),
                )
            })?;
        let outcome = loop {
            tokio::select! {
                Some(update) = events.recv() => progress.report(update),
                outcome = &mut completion => break outcome,
            }
        };
        while let Ok(update) = events.try_recv() {
            progress.report(update);
        }
        let (signed, batch) = outcome.map_err(|_| {
            self.step_failure(
                RoundStepFailureKind::InvariantViolation,
                Some(&scope.step),
                None,
                &ledger,
                "combined prover exited without a result",
            )
        })?;
        let ledger = signed.map(StepLedger::with_delegation).unwrap_or_default();
        let batch =
            batch.map_err(|error| self.step_voting_failure(error, Some(&scope.step), &ledger))?;
        progress.report(RoundStepProgress::DelegateAndVoteBatchPersisted { bundle_index });
        if scope.interrupted() {
            return self.step_cancelled(scope, ledger);
        }
        let (votes, batch) = self
            .recover_committed(
                &scope.round_id,
                crate::vote::VoteCommitmentRecovery::AtomicBatch(batch),
                &scope.observations,
            )
            .map_err(|error| self.step_voting_failure(error, Some(&scope.step), &ledger))?;
        self.complete_vote_unit(
            scope,
            votes,
            batch,
            CompletionEntry::FreshCast,
            &scope.host.chain_policy,
            ledger,
            progress,
        )
        .await
    }
}
