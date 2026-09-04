//! Resolving a bundle's SpendAuth signature from the host's signer.
//!
//! The crate never holds seed material. A software wallet signs the prepared
//! request itself; a Keystone wallet supplies the device signature either from
//! memory or from the sidecar row it was stored in.

use std::sync::Arc;

use crate::{
    delegate::{
        DelegationSigningRequest, PreparedDelegationBundle, PreparedSigner, SignedDelegationBundle,
    },
    types::VotingError,
};

use super::{DelegationPipeline, WalletDbOpener};

/// Produces the account SpendAuth signature for a delegation.
///
/// The wallet keeps its seed; the crate hands over only the account index,
/// network, seed fingerprint, sighash, and randomizer, and receives the
/// 64-byte signature back.
pub trait SpendAuthSigner: Send + Sync {
    fn sign(&self, request: DelegationSigningRequest) -> Result<[u8; 64], VotingError>;
}

impl<F> SpendAuthSigner for F
where
    F: Fn(DelegationSigningRequest) -> Result<[u8; 64], VotingError> + Send + Sync,
{
    fn sign(&self, request: DelegationSigningRequest) -> Result<[u8; 64], VotingError> {
        self(request)
    }
}

/// Where a Keystone signature comes from.
#[derive(Clone, Debug)]
pub enum KeystoneSignatureSource {
    /// The signature stored for the bundle through
    /// `VotingDb::store_keystone_signatures_batch`.
    Stored,
    /// A signature the host holds in memory.
    ///
    /// Once a bundle has verified it, the pipeline persists it under the
    /// bundle, so a later pass or a restart recovers it through `Stored`
    /// without asking the device to sign again.
    Provided { sig: Vec<u8>, sighash: Vec<u8> },
}

/// Signer for one delegation bundle.
#[derive(Clone)]
pub enum DelegationSigner {
    /// A software wallet that derives and randomizes its own SpendAuth key.
    Software(Arc<dyn SpendAuthSigner>),
    /// A Keystone device signed the redacted PCZT from
    /// [`DelegationPipeline::keystone_request`].
    Keystone(KeystoneSignatureSource),
}

impl<W: WalletDbOpener> DelegationPipeline<W> {
    /// Resolves the SpendAuth signature for a prepared bundle.
    ///
    /// Software signers receive the bundle's signing request and return the
    /// signature over its sighash. Keystone sources are validated to 64
    /// signature bytes and a 32-byte sighash; a `Stored` source that has no
    /// row for the bundle fails with [`VotingError::InvalidInput`].
    pub(super) fn spend_auth_signature(
        &self,
        prepared: &PreparedDelegationBundle,
        bundle_index: u32,
        signer: &DelegationSigner,
    ) -> Result<PreparedSigner, VotingError> {
        match signer {
            DelegationSigner::Software(signer) => {
                let request = prepared.signing_request(self.scoped_voting_db()?)?;
                let sig = signer.sign(request)?;
                Ok(PreparedSigner::signature(sig, request.sighash))
            }
            DelegationSigner::Keystone(source) => self.keystone_signature(bundle_index, source),
        }
    }

    /// Resolves a Keystone signature for `bundle_index` from `source`.
    pub(super) fn keystone_signature(
        &self,
        bundle_index: u32,
        source: &KeystoneSignatureSource,
    ) -> Result<PreparedSigner, VotingError> {
        match source {
            KeystoneSignatureSource::Provided { sig, sighash } => {
                PreparedSigner::signature_from_bytes(sig, sighash)
            }
            KeystoneSignatureSource::Stored => {
                let record = self
                    .scoped_voting_db()?
                    .get_keystone_signatures(self.round_id())?
                    .into_iter()
                    .find(|record| record.bundle_index == bundle_index)
                    .ok_or_else(|| VotingError::InvalidInput {
                        message: format!("no stored Keystone signature for bundle {bundle_index}"),
                    })?;
                PreparedSigner::signature_from_bytes(&record.sig, &record.sighash)
            }
        }
    }

    /// Persists a host-provided Keystone signature once the bundle verified it.
    ///
    /// Software signatures and already-stored Keystone signatures need no
    /// write. For [`KeystoneSignatureSource::Provided`], the verified tuple
    /// (signature, sighash, randomized key) is stored under the bundle so a
    /// pass cancelled before chain dispatch, or a restart, resumes through
    /// [`KeystoneSignatureSource::Stored`]. Replaying the same tuple is
    /// idempotent; a different signing context for the bundle fails with
    /// [`VotingError::KeystoneSignatureConflict`].
    pub(super) fn retain_provided_keystone_signature(
        &self,
        signer: &DelegationSigner,
        signed: &SignedDelegationBundle,
    ) -> Result<(), VotingError> {
        if !matches!(
            signer,
            DelegationSigner::Keystone(KeystoneSignatureSource::Provided { .. })
        ) {
            return Ok(());
        }
        self.scoped_voting_db()?.store_keystone_signature(
            self.round_id(),
            signed.bundle_index,
            &signed.submission.spend_auth_sig,
            &signed.submission.sighash,
            &signed.submission.rk,
        )
    }
}
