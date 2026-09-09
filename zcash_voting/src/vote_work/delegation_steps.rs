//! Delegation steps: prove and sign a bundle on a large-stack thread, or
//! re-sign and re-dispatch an already prepared delegation.

use std::sync::Arc;

use crate::{
    types::DelegationProgressBridge, AdvanceDelegation, ChainAdvanceRequest, ChainTransport,
    VotingHotkey,
};

use super::{
    round_lock::HeldRoundLock,
    step_ledger::StepLedger,
    step_scope::StepScope,
    steps::{persisted_policy, PROVING_STACK_BYTES},
    RoundExecutor, RoundStepFailure, RoundStepFailureKind, RoundStepOutcome, RoundStepProgress,
    RoundStepProgressReporter,
};

impl<T: ChainTransport> RoundExecutor<T> {
    pub(super) async fn run_delegate(
        &self,
        scope: &StepScope<'_>,
        bundle_index: u32,
        lock: &HeldRoundLock,
        progress: &dyn RoundStepProgressReporter,
    ) -> Result<RoundStepOutcome, RoundStepFailure> {
        let ledger = StepLedger::default();
        let inputs = self.delegation_inputs(scope)?;
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let driver = Arc::clone(&inputs.driver);
        let pir = Arc::clone(&inputs.pir);
        // The prover keeps the bundle lock until it has finished persisting,
        // even if this future is dropped, so no second pass can start a
        // competing proof for the same bundle meanwhile.
        let held_lock = Arc::clone(lock);
        let observations = scope.observations.clone();
        std::thread::Builder::new()
            .name("voting-delegation-step".to_string())
            .stack_size(PROVING_STACK_BYTES)
            .spawn(move || {
                let _held_lock = held_lock;
                let reporter = DelegationProgressBridge::new(move |progress| {
                    let _ = progress_tx.send(progress);
                });
                let driver_stage = observations.stage("delegation::driver_prepare");
                let result = driver.prepare_blocking_observed(
                    bundle_index,
                    &pir,
                    &reporter,
                    driver_stage.scope(),
                );
                driver_stage.finish(
                    if result.is_ok() {
                        crate::ObservationOutcome::Succeeded
                    } else {
                        crate::ObservationOutcome::Failed
                    },
                    result
                        .as_ref()
                        .err()
                        .map(crate::observability::voting_error_kind),
                );
                let _ = done_tx.send(result);
            })
            .map_err(|error| {
                self.step_failure(
                    RoundStepFailureKind::InvariantViolation,
                    Some(&scope.step),
                    None,
                    &ledger,
                    format!("failed to spawn delegation thread: {error}"),
                )
            })?;
        tokio::pin!(done_rx);
        let proof_status = loop {
            tokio::select! {
                Some(update) = progress_rx.recv() => {
                    progress.report(RoundStepProgress::Delegation { bundle_index, progress: update });
                }
                result = &mut done_rx => {
                    break result.map_err(|_| {
                        self.step_failure(
                            RoundStepFailureKind::InvariantViolation,
                            Some(&scope.step),
                            None,
                            &ledger,
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
        proof_status
            .map_err(|error| self.step_voting_failure(error, Some(&scope.step), &ledger))?;
        if scope.interrupted() {
            return self.step_cancelled(scope, ledger);
        }
        self.outcome(scope, super::RoundStepDisposition::Advanced, ledger)
    }

    pub(super) async fn run_advance_delegation(
        &self,
        scope: &StepScope<'_>,
        bundle_index: u32,
        lock: &HeldRoundLock,
        progress: &dyn RoundStepProgressReporter,
    ) -> Result<RoundStepOutcome, RoundStepFailure> {
        let ledger = StepLedger::default();
        let inputs = self.delegation_inputs(scope)?;
        let driver = Arc::clone(&inputs.driver);
        let signer = inputs.signer.clone();
        // The signing task keeps the bundle lock while it runs, so an aborted
        // future cannot let a new pass prompt the host signer concurrently.
        let held_lock = Arc::clone(lock);
        let observations = scope.observations.clone();
        let signature = tokio::task::spawn_blocking(move || {
            let _held_lock = held_lock;
            driver.resign_blocking_observed(bundle_index, &signer, &observations)
        })
        .await
        .map_err(|error| {
            self.step_failure(
                RoundStepFailureKind::InvariantViolation,
                Some(&scope.step),
                None,
                &ledger,
                format!("delegation signing task failed: {error}"),
            )
        })?
        .map_err(|error| self.step_voting_failure(error, Some(&scope.step), &ledger))?;
        let request = AdvanceDelegation {
            vote_round_id: scope.round_id_bytes,
            bundle_index,
            spend_auth_signature: signature,
        };
        let outcome = self
            .chain_client
            .advance_until_terminal_in_epoch(
                ChainAdvanceRequest::Delegation(request),
                &persisted_policy(scope.host),
                scope.chain(),
                scope.entry_epoch(),
            )
            .await
            .map_err(|failure| self.step_chain_failure(failure, Some(&scope.step), &ledger))?;
        self.chain_step_outcome(scope, outcome, ledger, progress)
    }

    /// The host's delegation inputs, refused unless the driver is bound to
    /// the same network, round, wallet, database and voting hotkey as this
    /// step's scope.
    pub(super) fn delegation_inputs(
        &self,
        scope: &StepScope<'_>,
    ) -> Result<super::DelegationStepInputs, RoundStepFailure> {
        let ledger = StepLedger::default();
        let refuse = |message: String| {
            self.step_failure(
                RoundStepFailureKind::InvalidInput,
                Some(&scope.step),
                None,
                &ledger,
                message,
            )
        };
        let inputs = scope.host.delegation.clone().ok_or_else(|| {
            refuse("delegation steps require RoundHostContext::delegation".to_string())
        })?;
        if inputs.driver.network() != scope.network {
            return Err(refuse(format!(
                "delegation driver network {:?} does not match the round binding network {:?}",
                inputs.driver.network(),
                scope.network
            )));
        }
        if inputs.driver.round_id() != scope.round_id {
            return Err(refuse(
                "delegation driver is bound to a different round".to_string(),
            ));
        }
        if inputs.driver.wallet_id() != scope.wallet_id {
            return Err(refuse(format!(
                "delegation driver is scoped to wallet {} but the executor to wallet {}",
                inputs.driver.wallet_id(),
                scope.wallet_id
            )));
        }
        if !inputs.driver.shares_database_with(&self.database) {
            return Err(refuse(
                "delegation driver persists into a different voting database than the executor"
                    .to_string(),
            ));
        }
        // The delegation must land for the hotkey CastVote will later
        // reconstruct from the binding; otherwise the confirmed VAN cannot be
        // spent by the executor's own votes.
        if let Some(secret) = scope.hotkey_secret.as_ref() {
            let bound_target = VotingHotkey::from_stored_secret(secret, scope.network)
                .map_err(|error| self.step_voting_failure(error, Some(&scope.step), &ledger))?
                .delegation_target();
            match inputs.driver.delegation_target() {
                Some(target) if target == bound_target => {}
                Some(_) => {
                    return Err(refuse(
                        "delegation driver delegates to a different voting hotkey than the round binding"
                            .to_string(),
                    ));
                }
                None => {
                    return Err(refuse(
                        "delegation driver holds no voting hotkey while the round binding does"
                            .to_string(),
                    ));
                }
            }
        }
        Ok(inputs)
    }
}
