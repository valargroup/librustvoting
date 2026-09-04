//! Bundle preparation, PCZT setup, PIR precompute, proof generation, and the
//! final prove-and-sign.

use crate::{
    delegate::{
        self, DelegationProgress, DelegationProofStatus, KeystoneSigningRequest,
        PrepareDelegationBundleParams, PreparedDelegationBundle, PreparedDelegationReport,
        SignedDelegationBundle,
    },
    pir::{PirFleet, PirProofSource},
    types::{DelegationProgressReporter, DelegationSetupField, NoopProgressReporter, VotingError},
};

use super::{
    proving_thread::start_proving_cache_warmup, DelegationPipeline, DelegationSigner,
    WalletDbOpener,
};

impl<W: WalletDbOpener> DelegationPipeline<W> {
    /// Prepares one bundle: round metadata, wallet snapshot, and witnesses.
    pub fn prepare(&self, bundle_index: u32) -> Result<PreparedDelegationBundle, VotingError> {
        let hotkey = self.hotkey()?;
        let wallet = self.wallet.open_for_read()?;
        delegate::prepare_delegation_bundle(
            self.scoped_voting_db()?,
            &wallet,
            PrepareDelegationBundleParams {
                lwd: self.lwd.clone(),
                session_json: self.session_json.as_deref(),
                account_uuid: &self.account_uuid,
                voting_hotkey: hotkey,
                bundle_index,
                bundle_policy: self.bundle_policy,
            },
        )
    }

    /// Whether a durable proof already exists for the bundle.
    ///
    /// Every post-proof phase counts, including the lifecycle-owned and
    /// terminal submission phases, so an offline resume or a retry after a
    /// rejected generation reuses the persisted proof instead of re-entering
    /// PIR. See [`crate::phases::DelegationPhase::has_persisted_proof`].
    pub fn has_persisted_proof(&self, bundle_index: u32) -> Result<bool, VotingError> {
        Ok(self
            .scoped_voting_db()?
            .delegation_phase(self.round_id(), bundle_index)?
            .has_persisted_proof())
    }

    /// Builds and persists the governance PCZT, or reuses the persisted setup.
    ///
    /// Setup is write-once. When a prior attempt already persisted the
    /// sighash and effects, the stored values are kept and no PCZT bytes are
    /// returned; signing then runs against the stored sighash.
    fn ensure_setup(
        &self,
        prepared: &PreparedDelegationBundle,
        progress: &dyn DelegationProgressReporter,
    ) -> Result<Vec<u8>, VotingError> {
        match prepared.setup(self.scoped_voting_db()?, progress) {
            Ok(setup) => Ok(setup.pczt_bytes),
            Err(VotingError::SetupAlreadyPersisted {
                field: DelegationSetupField::PcztSighash | DelegationSetupField::Tx1Effects,
                ..
            }) => Ok(Vec::new()),
            Err(error) => Err(error),
        }
    }

    /// Persists witnesses and padded secrets and warms the bundle's PIR rows.
    pub fn precompute_pir(
        &self,
        bundle_index: u32,
        pir: &PirFleet,
    ) -> Result<PreparedDelegationReport, VotingError> {
        let prepared = self.prepare(bundle_index)?;
        let wallet = self.wallet.open_for_read()?;
        pir.with_failover(|session| prepared.precompute(self.scoped_voting_db()?, &wallet, session))
    }

    /// Generates or reuses the bundle's durable proof without signing.
    ///
    /// A bundle whose proof is already persisted returns
    /// [`DelegationProofStatus::Reused`] without touching PIR.
    pub fn ensure_proof(
        &self,
        bundle_index: u32,
        pir: &PirFleet,
        progress: &dyn DelegationProgressReporter,
    ) -> Result<DelegationProofStatus, VotingError> {
        if self.has_persisted_proof(bundle_index)? {
            return Ok(DelegationProofStatus::Reused);
        }
        let prepared = self.prepare(bundle_index)?;
        self.ensure_setup(&prepared, &NoopProgressReporter)?;
        self.prove_with_fleet(&prepared, pir, progress)
    }

    fn prove_with_fleet(
        &self,
        prepared: &PreparedDelegationBundle,
        pir: &PirFleet,
        progress: &dyn DelegationProgressReporter,
    ) -> Result<DelegationProofStatus, VotingError> {
        start_proving_cache_warmup();
        let wallet = self.wallet.open_for_read()?;
        pir.with_failover(|session| {
            let source: &dyn PirProofSource = session;
            prepared.precompute(self.scoped_voting_db()?, &wallet, source)?;
            prepared
                .ensure_proof(self.scoped_voting_db()?, source, progress)
                .map(|completion| completion.status)
        })
    }

    /// Builds the redacted signing request for a Keystone device.
    pub fn keystone_request(
        &self,
        bundle_index: u32,
    ) -> Result<KeystoneSigningRequest, VotingError> {
        let prepared = self.prepare(bundle_index)?;
        prepared.keystone_request(self.scoped_voting_db()?, &NoopProgressReporter)
    }

    /// Proves and signs one bundle, blocking the current thread.
    ///
    /// Emits the full progress sequence: `SelectingNotes`, the PCZT and
    /// proof stages, `SigningPayload`, `PayloadReady`. Software signing
    /// builds the PCZT when no proof is persisted yet and reuses persisted
    /// setup otherwise; Keystone signing never rebuilds the PCZT the device
    /// signed. Retryable PIR failures move to the next fleet endpoint while
    /// reusing the same prepared bundle. A host-provided Keystone signature
    /// is persisted under the bundle once verified, so the signed payload is
    /// durable before this returns.
    pub fn prove_and_sign_blocking(
        &self,
        bundle_index: u32,
        signer: &DelegationSigner,
        pir: &PirFleet,
        progress: &dyn DelegationProgressReporter,
    ) -> Result<SignedDelegationBundle, VotingError> {
        progress.on_progress(DelegationProgress::SelectingNotes);
        let prepared = self.prepare(bundle_index)?;
        let proof_persisted = self.has_persisted_proof(bundle_index)?;
        let pczt_bytes = match signer {
            DelegationSigner::Software(_) if !proof_persisted => {
                self.ensure_setup(&prepared, progress)?
            }
            _ => Vec::new(),
        };
        if proof_persisted {
            progress.on_progress(DelegationProgress::ProofComplete);
        } else {
            self.prove_with_fleet(&prepared, pir, progress)?;
        }

        progress.on_progress(DelegationProgress::SigningPayload);
        let prepared_signer = self.spend_auth_signature(&prepared, bundle_index, signer)?;
        let signed =
            prepared.signed_bundle(self.scoped_voting_db()?, pczt_bytes, prepared_signer)?;
        self.retain_provided_keystone_signature(signer, &signed)?;
        progress.on_progress(DelegationProgress::PayloadReady);
        Ok(signed)
    }
}
