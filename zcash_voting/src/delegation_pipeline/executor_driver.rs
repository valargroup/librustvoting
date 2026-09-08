//! The object-safe delegation surface the round executor drives.

use crate::{
    delegate::{PreparedSigner, SignedDelegationBundle},
    pir::PirFleet,
    round::VotingDb,
    types::{DelegationProgressReporter, Network, VotingError, VotingHotkey, VotingHotkeyTarget},
};

use super::{DelegationPipeline, DelegationSigner, WalletDbOpener};

/// Object-safe delegation stages the round executor drives.
///
/// [`DelegationPipeline`] implements this for any wallet opener, so the
/// executor does not carry the opener's type parameter.
pub trait DelegationDriver: Send + Sync {
    /// Round the driver is bound to.
    fn round_id(&self) -> &str;

    /// Network the driver's round and hotkey belong to.
    ///
    /// The executor refuses a driver whose network differs from its binding,
    /// so a proof for one network is never generated and persisted against a
    /// chain client configured for another.
    fn network(&self) -> Network;

    /// The voting-hotkey target the driver delegates to, when it holds a
    /// hotkey.
    ///
    /// The executor compares it with the target derived from
    /// `RoundBinding::hotkey_secret` before any delegation stage, so a
    /// delegation cannot land for one hotkey while `CastVote` later
    /// reconstructs another that cannot spend the confirmed VAN.
    fn delegation_target(&self) -> Option<VotingHotkeyTarget>;

    /// Wallet the driver captured at construction.
    ///
    /// The round executor refuses a driver whose wallet differs from its own
    /// frozen scope, so delegation work cannot be persisted under one
    /// wallet while another wallet's bundle lock is held.
    fn wallet_id(&self) -> &str;

    /// Whether the driver persists into the same sidecar connection as
    /// `database`. The executor requires this together with a matching
    /// wallet id before invoking any delegation stage.
    fn shares_database_with(&self, database: &VotingDb) -> bool;

    /// Prepares and persists a delegation proof without requesting a signature
    /// or broadcasting. Repeated calls validate and reuse the stored proof.
    fn prepare_blocking(
        &self,
        bundle_index: u32,
        pir: &PirFleet,
        progress: &dyn DelegationProgressReporter,
    ) -> Result<crate::delegate::DelegationProofStatus, VotingError>;

    /// SDK observation hook for proof-only preparation.
    #[doc(hidden)]
    fn prepare_blocking_observed(
        &self,
        bundle_index: u32,
        pir: &PirFleet,
        progress: &dyn DelegationProgressReporter,
        _observations: &crate::ObservationScope,
    ) -> Result<crate::delegate::DelegationProofStatus, VotingError> {
        self.prepare_blocking(bundle_index, pir, progress)
    }

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

    /// SDK observation hook; existing implementations may use the default.
    #[doc(hidden)]
    fn prove_and_sign_blocking_observed(
        &self,
        bundle_index: u32,
        signer: &DelegationSigner,
        pir: &PirFleet,
        progress: &dyn DelegationProgressReporter,
        _observations: &crate::observability::ObservationScope,
    ) -> Result<SignedDelegationBundle, VotingError> {
        self.prove_and_sign_blocking(bundle_index, signer, pir, progress)
    }

    /// Produces a fresh SpendAuth signature over the bundle's persisted
    /// sighash, for re-dispatching a delegation that is already prepared.
    fn resign_blocking(
        &self,
        bundle_index: u32,
        signer: &DelegationSigner,
    ) -> Result<[u8; 64], VotingError>;

    /// SDK observation hook; existing implementations may use the default.
    #[doc(hidden)]
    fn resign_blocking_observed(
        &self,
        bundle_index: u32,
        signer: &DelegationSigner,
        _observations: &crate::observability::ObservationScope,
    ) -> Result<[u8; 64], VotingError> {
        self.resign_blocking(bundle_index, signer)
    }
}

impl<W: WalletDbOpener> DelegationDriver for DelegationPipeline<W> {
    fn round_id(&self) -> &str {
        DelegationPipeline::round_id(self)
    }

    fn network(&self) -> Network {
        DelegationPipeline::network(self)
    }

    fn delegation_target(&self) -> Option<VotingHotkeyTarget> {
        self.hotkey.as_ref().map(VotingHotkey::delegation_target)
    }

    fn wallet_id(&self) -> &str {
        DelegationPipeline::wallet_id(self)
    }

    fn shares_database_with(&self, database: &VotingDb) -> bool {
        self.voting_db.shares_connection_with(database)
    }

    fn prepare_blocking(
        &self,
        bundle_index: u32,
        pir: &PirFleet,
        progress: &dyn DelegationProgressReporter,
    ) -> Result<crate::delegate::DelegationProofStatus, VotingError> {
        self.ensure_proof(bundle_index, pir, progress)
    }

    fn prepare_blocking_observed(
        &self,
        bundle_index: u32,
        pir: &PirFleet,
        progress: &dyn DelegationProgressReporter,
        observations: &crate::ObservationScope,
    ) -> Result<crate::delegate::DelegationProofStatus, VotingError> {
        self.observe_ensure_proof(bundle_index, pir, progress, observations)
    }

    fn prove_and_sign_blocking(
        &self,
        bundle_index: u32,
        signer: &DelegationSigner,
        pir: &PirFleet,
        progress: &dyn DelegationProgressReporter,
    ) -> Result<SignedDelegationBundle, VotingError> {
        self.execute_prove_and_sign_blocking(
            bundle_index,
            signer,
            pir,
            progress,
            &crate::ObservationScope::disabled(),
        )
    }

    fn prove_and_sign_blocking_observed(
        &self,
        bundle_index: u32,
        signer: &DelegationSigner,
        pir: &PirFleet,
        progress: &dyn DelegationProgressReporter,
        observations: &crate::ObservationScope,
    ) -> Result<SignedDelegationBundle, VotingError> {
        DelegationPipeline::execute_prove_and_sign_blocking(
            self,
            bundle_index,
            signer,
            pir,
            progress,
            observations,
        )
    }

    fn resign_blocking(
        &self,
        bundle_index: u32,
        signer: &DelegationSigner,
    ) -> Result<[u8; 64], VotingError> {
        self.resign_blocking_observed(bundle_index, signer, &crate::ObservationScope::disabled())
    }

    fn resign_blocking_observed(
        &self,
        bundle_index: u32,
        signer: &DelegationSigner,
        observations: &crate::ObservationScope,
    ) -> Result<[u8; 64], VotingError> {
        let prepared = self.execute_prepare(bundle_index, observations)?;
        let PreparedSigner::Signature { sig, .. } =
            self.spend_auth_signature(&prepared, bundle_index, signer, observations)?;
        Ok(sig)
    }
}
