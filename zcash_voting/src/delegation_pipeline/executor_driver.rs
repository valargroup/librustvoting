//! The object-safe delegation surface the round executor drives.

use crate::{
    delegate::{PreparedSigner, SignedDelegationBundle},
    pir::PirFleet,
    types::{DelegationProgressReporter, VotingError},
};

use super::{DelegationPipeline, DelegationSigner, WalletDbOpener};

/// Object-safe delegation stages the round executor drives.
///
/// [`DelegationPipeline`] implements this for any wallet opener, so the
/// executor does not carry the opener's type parameter.
pub trait DelegationDriver: Send + Sync {
    /// Round the driver is bound to.
    fn round_id(&self) -> &str;

    /// Proves and signs one bundle on the calling thread.
    ///
    /// Reports the full progress sequence through `progress`, ending in
    /// [`crate::delegate::DelegationProgress::PayloadReady`] on success. That
    /// event is the driver's to emit: the round executor forwards what it
    /// receives and adds no terminal event of its own, so an implementation
    /// that stays silent leaves hosts without a completion signal.
    fn prove_and_sign_blocking(
        &self,
        bundle_index: u32,
        signer: &DelegationSigner,
        pir: &PirFleet,
        progress: &dyn DelegationProgressReporter,
    ) -> Result<SignedDelegationBundle, VotingError>;

    /// Produces a fresh SpendAuth signature over the bundle's persisted
    /// sighash, for re-dispatching a delegation that is already prepared.
    fn resign_blocking(
        &self,
        bundle_index: u32,
        signer: &DelegationSigner,
    ) -> Result<[u8; 64], VotingError>;
}

impl<W: WalletDbOpener> DelegationDriver for DelegationPipeline<W> {
    fn round_id(&self) -> &str {
        DelegationPipeline::round_id(self)
    }

    fn prove_and_sign_blocking(
        &self,
        bundle_index: u32,
        signer: &DelegationSigner,
        pir: &PirFleet,
        progress: &dyn DelegationProgressReporter,
    ) -> Result<SignedDelegationBundle, VotingError> {
        DelegationPipeline::prove_and_sign_blocking(self, bundle_index, signer, pir, progress)
    }

    fn resign_blocking(
        &self,
        bundle_index: u32,
        signer: &DelegationSigner,
    ) -> Result<[u8; 64], VotingError> {
        let prepared = self.prepare(bundle_index)?;
        let PreparedSigner::Signature { sig, .. } =
            self.spend_auth_signature(&prepared, bundle_index, signer)?;
        Ok(sig)
    }
}
