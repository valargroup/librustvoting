//! Delegation steps: prove and sign a bundle on a large-stack thread, or
//! re-sign and re-dispatch an already prepared delegation.

use std::sync::Arc;

use crate::{
    session::NextStep, types::DelegationProgressBridge, AdvanceDelegation, ChainAdvanceRequest,
    ChainTransport, VotingHotkey,
};

use super::{
    round_lock::HeldRoundLock,
    step_control::StepControl,
    steps::{persisted_policy, PROVING_STACK_BYTES},
    RoundExecutor, RoundHostContext, RoundStepFailure, RoundStepFailureKind, RoundStepOutcome,
    RoundStepProgress, RoundStepProgressReporter,
};

impl<T: ChainTransport> RoundExecutor<T> {
    pub(super) async fn run_delegate(
        &self,
        step: NextStep,
        bundle_index: u32,
        host: &RoundHostContext,
        lock: &HeldRoundLock,
        control: &StepControl<'_>,
        progress: &dyn RoundStepProgressReporter,
    ) -> Result<RoundStepOutcome, RoundStepFailure> {
        let inputs = self.delegation_inputs(&step, host)?;
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let driver = Arc::clone(&inputs.driver);
        let signer = inputs.signer.clone();
        let pir = Arc::clone(&inputs.pir);
        // The prover keeps the bundle lock until it has finished persisting,
        // even if this future is dropped, so no second pass can start a
        // competing proof for the same bundle meanwhile.
        let held_lock = Arc::clone(lock);
        std::thread::Builder::new()
            .name("voting-delegation-step".to_string())
            .stack_size(PROVING_STACK_BYTES)
            .spawn(move || {
                let _held_lock = held_lock;
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
        // The driver emits `PayloadReady` itself and the drain above forwards
        // it, so reporting one here would deliver the terminal event twice.
        let signed = signed.map_err(|error| self.step_voting_failure(error, Some(step.clone())))?;
        if control.interrupted() {
            // Proof, setup, and a provided Keystone signature are durable by
            // now, so the next pass re-dispatches through AdvanceDelegation
            // without proving or asking the device again. The signed bundle
            // is still returned so an in-process host can submit it directly.
            return self.step_cancelled(Some(step), None, Vec::new(), Some(signed));
        }
        let request = AdvanceDelegation {
            vote_round_id: self.round_id_bytes(&step)?,
            bundle_index,
            spend_auth_signature: signed.submission.spend_auth_sig,
        };
        let outcome = self
            .chain_client
            .advance_until_terminal_in_epoch(
                ChainAdvanceRequest::Delegation(request),
                &host.chain_policy,
                control.chain(),
                control.entry_epoch(),
            )
            .await
            .map_err(|failure| self.step_chain_failure(failure, Some(step.clone())))?;
        self.chain_step_outcome(step, outcome, Some(signed), progress)
    }

    pub(super) async fn run_advance_delegation(
        &self,
        step: NextStep,
        bundle_index: u32,
        host: &RoundHostContext,
        lock: &HeldRoundLock,
        control: &StepControl<'_>,
        progress: &dyn RoundStepProgressReporter,
    ) -> Result<RoundStepOutcome, RoundStepFailure> {
        let inputs = self.delegation_inputs(&step, host)?;
        let driver = Arc::clone(&inputs.driver);
        let signer = inputs.signer.clone();
        // The signing task keeps the bundle lock while it runs, so an aborted
        // future cannot let a new pass prompt the host signer concurrently.
        let held_lock = Arc::clone(lock);
        let signature = tokio::task::spawn_blocking(move || {
            let _held_lock = held_lock;
            driver.resign_blocking(bundle_index, &signer)
        })
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
            .advance_until_terminal_in_epoch(
                ChainAdvanceRequest::Delegation(request),
                &persisted_policy(host),
                control.chain(),
                control.entry_epoch(),
            )
            .await
            .map_err(|failure| self.step_chain_failure(failure, Some(step.clone())))?;
        self.chain_step_outcome(step, outcome, None, progress)
    }

    pub(super) fn delegation_inputs(
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
        let binding = self
            .binding()
            .map_err(|error| self.step_voting_failure(error, Some(step.clone())))?;
        let round_id = binding.round_id.clone();
        if inputs.driver.network() != binding.network {
            return Err(self.step_failure(
                RoundStepFailureKind::InvalidInput,
                Some(step.clone()),
                None,
                None,
                format!(
                    "delegation driver network {:?} does not match the round binding network {:?}",
                    inputs.driver.network(),
                    binding.network
                ),
            ));
        }
        if inputs.driver.round_id() != round_id {
            return Err(self.step_failure(
                RoundStepFailureKind::InvalidInput,
                Some(step.clone()),
                None,
                None,
                "delegation driver is bound to a different round",
            ));
        }
        if inputs.driver.wallet_id() != self.wallet_id {
            return Err(self.step_failure(
                RoundStepFailureKind::InvalidInput,
                Some(step.clone()),
                None,
                None,
                format!(
                    "delegation driver is scoped to wallet {} but the executor to wallet {}",
                    inputs.driver.wallet_id(),
                    self.wallet_id
                ),
            ));
        }
        if !inputs.driver.shares_database_with(&self.database) {
            return Err(self.step_failure(
                RoundStepFailureKind::InvalidInput,
                Some(step.clone()),
                None,
                None,
                "delegation driver persists into a different voting database than the executor",
            ));
        }
        // The delegation must land for the hotkey CastVote will later
        // reconstruct from the binding; otherwise the confirmed VAN cannot be
        // spent by the executor's own votes.
        if let Some(secret) = binding.hotkey_secret.as_ref() {
            let bound_target = VotingHotkey::from_stored_secret(secret, binding.network)
                .map_err(|error| self.step_voting_failure(error, Some(step.clone())))?
                .delegation_target();
            match inputs.driver.delegation_target() {
                Some(target) if target == bound_target => {}
                Some(_) => {
                    return Err(self.step_failure(
                        RoundStepFailureKind::InvalidInput,
                        Some(step.clone()),
                        None,
                        None,
                        "delegation driver delegates to a different voting hotkey than the round binding",
                    ));
                }
                None => {
                    return Err(self.step_failure(
                        RoundStepFailureKind::InvalidInput,
                        Some(step.clone()),
                        None,
                        None,
                        "delegation driver holds no voting hotkey while the round binding does",
                    ));
                }
            }
        }
        Ok(inputs)
    }
}
