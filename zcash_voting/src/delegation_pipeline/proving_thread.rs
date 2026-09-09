//! Blocking orchestration for the async prove-and-sign entry point
//! and the process-lifetime proving-key warm-up.

use std::sync::{Arc, OnceLock};

use crate::{
    delegate::SignedDelegationBundle,
    pir::PirFleet,
    types::{DelegationProgressReporter, VotingError},
};

use super::{DelegationPipeline, DelegationSigner, WalletDbOpener};

static PROVING_CACHE_WARMUP_STARTED: OnceLock<()> = OnceLock::new();

impl<W: WalletDbOpener + 'static> DelegationPipeline<W> {
    /// Proves and signs one bundle using blocking orchestration and shared CPU execution.
    ///
    /// Dropping the future removes queued proof work. Running CPU work finishes;
    /// subsequent signing and persistence boundaries observe interruption.
    pub async fn prove_and_sign(
        self: Arc<Self>,
        bundle_index: u32,
        signer: DelegationSigner,
        pir: Arc<PirFleet>,
        progress: Arc<dyn DelegationProgressReporter>,
    ) -> Result<SignedDelegationBundle, VotingError> {
        self.observe_prove_and_sign(
            bundle_index,
            signer,
            pir,
            progress,
            &crate::ObservationScope::disabled(),
        )
        .await
    }

    pub(crate) async fn observe_prove_and_sign(
        self: Arc<Self>,
        bundle_index: u32,
        signer: DelegationSigner,
        pir: Arc<PirFleet>,
        progress: Arc<dyn DelegationProgressReporter>,
        observations: &crate::ObservationScope,
    ) -> Result<SignedDelegationBundle, VotingError> {
        observations.bind_round_id(self.round_id());
        let attributed = observations.attributed(crate::ObservationAttribution {
            bundle_index: Some(bundle_index),
            ..Default::default()
        });
        let stage = attributed.stage("delegation::prove_and_sign");
        let result = self
            .execute_prove_and_sign(bundle_index, signer, pir, progress, stage.scope())
            .await;
        let outcome = if result.is_ok() {
            crate::ObservationOutcome::Succeeded
        } else {
            crate::ObservationOutcome::Failed
        };
        stage.finish(
            outcome,
            result
                .as_ref()
                .err()
                .map(crate::observability::voting_error_kind),
        );
        result
    }

    /// Runs this workflow with optional per-call diagnostics, including on errors.
    pub async fn prove_and_sign_with_report(
        self: Arc<Self>,
        bundle_index: u32,
        signer: DelegationSigner,
        pir: Arc<PirFleet>,
        progress: Arc<dyn DelegationProgressReporter>,
        options: Option<crate::ObservabilityOptions>,
    ) -> crate::OperationReport<Result<SignedDelegationBundle, VotingError>> {
        let invocation = crate::ObservationScope::new(options).invocation();

        let result = self
            .observe_prove_and_sign(bundle_index, signer, pir, progress, invocation.scope())
            .await;
        let outcome = if result.is_ok() {
            crate::ObservationOutcome::Succeeded
        } else {
            crate::ObservationOutcome::Failed
        };
        invocation.complete("prove_and_sign", outcome, result)
    }

    pub(crate) async fn execute_prove_and_sign(
        self: Arc<Self>,
        bundle_index: u32,
        signer: DelegationSigner,
        pir: Arc<PirFleet>,
        progress: Arc<dyn DelegationProgressReporter>,
        observations: &crate::ObservationScope,
    ) -> Result<SignedDelegationBundle, VotingError> {
        let observations = observations.clone();
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let operation = crate::proving_runtime::Operation::current();
        let _operation_owner = operation.owner();
        crate::proving_runtime::spawn_orchestration(move || {
            operation.enter(|| {
                let result = self.execute_prove_and_sign_blocking(
                    bundle_index,
                    &signer,
                    &pir,
                    &*progress,
                    &observations,
                );
                let _ = reply_tx.send(result);
            })
        })?;
        reply_rx.await.map_err(|_| VotingError::Internal {
            message: "delegation prove thread exited without a result".to_string(),
        })?
    }
}

/// Starts the process-lifetime proving-key warm-up once and returns at once.
///
/// The first proof that needs keys waits on the shared cache until this
/// warm-up, or an inline cold keygen, finishes. Later calls are no-ops.
pub fn start_proving_cache_warmup() {
    if PROVING_CACHE_WARMUP_STARTED.set(()).is_err() {
        return;
    }
    // One process-lifetime coordinator; key generation itself uses the shared pool.
    let _ = std::thread::Builder::new()
        .name("voting-cache-coordinator".to_string())
        .spawn(crate::warm_proving_caches);
}
