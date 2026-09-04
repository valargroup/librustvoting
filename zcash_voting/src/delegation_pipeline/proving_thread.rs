//! Large-stack OS threads for proving: the async prove-and-sign entry point
//! and the process-lifetime proving-key warm-up.

use std::sync::{Arc, OnceLock};

use crate::{
    delegate::SignedDelegationBundle,
    pir::PirFleet,
    types::{DelegationProgressReporter, VotingError},
};

use super::{DelegationPipeline, DelegationSigner, WalletDbOpener};

// Matches the keygen warm-up threads in voting-circuits.
const PROVING_STACK_BYTES: usize = 64 * 1024 * 1024;

static PROVING_CACHE_WARMUP_STARTED: OnceLock<()> = OnceLock::new();

impl<W: WalletDbOpener + 'static> DelegationPipeline<W> {
    /// Proves and signs one bundle on a dedicated large-stack OS thread.
    ///
    /// The thread is not cancelled when the returned future is dropped; it
    /// runs the bundle to completion so durable state is never left half
    /// written. Callers that need cancellation should decide before calling.
    pub async fn prove_and_sign(
        self: Arc<Self>,
        bundle_index: u32,
        signer: DelegationSigner,
        pir: Arc<PirFleet>,
        progress: Arc<dyn DelegationProgressReporter>,
    ) -> Result<SignedDelegationBundle, VotingError> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        std::thread::Builder::new()
            .name("voting-delegation-prove".to_string())
            .stack_size(PROVING_STACK_BYTES)
            .spawn(move || {
                let result = self.prove_and_sign_blocking(bundle_index, &signer, &pir, &*progress);
                let _ = reply_tx.send(result);
            })
            .map_err(|error| VotingError::Internal {
                message: format!("failed to spawn delegation prove thread: {error}"),
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
    let _ = std::thread::Builder::new()
        .name("voting-proving-cache-warmup".to_string())
        .stack_size(PROVING_STACK_BYTES)
        .spawn(crate::warm_proving_caches);
}
