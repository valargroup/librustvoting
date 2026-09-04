//! Delegation as one object.
//!
//! A [`DelegationPipeline`] binds the sidecar database, a way to open the
//! wallet database, the round's lightwalletd inputs, the account, the voting
//! hotkey, and the bundle policy once. Every stage of delegation for a bundle
//! then runs from that binding, in this order:
//!
//! 1. `bundle_setup`: round row, note selection, bundle layout, eligibility.
//! 2. `proving`: bundle preparation, PCZT setup, PIR precompute with fleet
//!    failover, durable proof generation, and the final prove-and-sign.
//! 3. `signing`: resolving the SpendAuth signature from the host's
//!    [`DelegationSigner`], never from seed material inside the crate.
//! 4. `proving_thread`: the large-stack OS thread the async entry point and
//!    the process-lifetime proving-key warm-up run on.
//! 5. `executor_driver`: the object-safe [`DelegationDriver`] surface the
//!    round executor drives.
//!
//! Signing never brings seed material into the crate. A software wallet
//! implements [`SpendAuthSigner`] over its own seed and returns only the
//! SpendAuth signature; a Keystone wallet supplies the device signature.

mod bundle_setup;
mod executor_driver;
mod proving;
mod proving_thread;
mod signing;
mod wallet_access;

use std::sync::Arc;

use crate::{
    delegate::DelegationLwdInputs,
    note_bundling::BundlePolicy,
    round::VotingDb,
    types::{Network, VotingError, VotingHotkey},
};

pub use bundle_setup::VotingEligibilityReport;
pub use executor_driver::DelegationDriver;
pub use proving_thread::start_proving_cache_warmup;
pub use signing::{DelegationSigner, KeystoneSignatureSource, SpendAuthSigner};
pub use wallet_access::{SqliteWalletDbOpener, WalletDbOpener};

/// One account's delegation work for one round.
///
/// The wallet is captured at construction: the pipeline works on its own
/// handle over the sidecar connection it was given, so a later
/// `set_wallet_id` on the host's handle cannot retarget delegation work.
pub struct DelegationPipeline<W: WalletDbOpener> {
    wallet_id: String,
    voting_db: Arc<VotingDb>,
    wallet: W,
    lwd: DelegationLwdInputs,
    session_json: Option<String>,
    account_uuid: String,
    hotkey: Option<VotingHotkey>,
    bundle_policy: BundlePolicy,
}

impl<W: WalletDbOpener> DelegationPipeline<W> {
    /// Binds the pipeline. `hotkey` may be `None` for stages that need no
    /// hotkey (bundle setup and eligibility); preparing a bundle requires it.
    ///
    /// # Errors
    ///
    /// Returns [`VotingError::InvalidInput`] when the round params are invalid
    /// or the hotkey network does not match the lightwalletd inputs.
    pub fn new(
        voting_db: Arc<VotingDb>,
        wallet: W,
        lwd: DelegationLwdInputs,
        account_uuid: &str,
        hotkey: Option<VotingHotkey>,
        bundle_policy: BundlePolicy,
        session_json: Option<&str>,
    ) -> Result<Self, VotingError> {
        crate::validate_round_params(&lwd.round_params)?;
        if let Some(hotkey) = hotkey.as_ref() {
            if hotkey.network() != lwd.network {
                return Err(VotingError::InvalidInput {
                    message: "delegation LWD network does not match voting hotkey network"
                        .to_string(),
                });
            }
        }
        if account_uuid.trim().is_empty() {
            return Err(VotingError::InvalidInput {
                message: "account_uuid must not be empty".to_string(),
            });
        }
        bundle_setup::decode_anchor_tree_state(&lwd.anchor_tree_state_bytes)?;
        let wallet_id = voting_db.wallet_id();
        let voting_db = Arc::new(voting_db.scoped(&wallet_id));
        Ok(Self {
            wallet_id,
            voting_db,
            wallet,
            lwd,
            session_json: session_json.map(str::to_string),
            account_uuid: account_uuid.to_string(),
            hotkey,
            bundle_policy,
        })
    }

    /// A handle on the pipeline's sidecar connection, scoped to its wallet.
    ///
    /// Each call returns a fresh handle over the same connection. The
    /// pipeline's own handle is never handed out, so re-scoping the returned
    /// one with `set_wallet_id` cannot move a running stage's persistence to
    /// another wallet.
    pub fn voting_db(&self) -> Arc<VotingDb> {
        Arc::new(self.voting_db.scoped(&self.wallet_id))
    }

    /// The handle every stage persists through, verified to still select the
    /// wallet captured at construction.
    ///
    /// The handle is private, so this check cannot fail through the public
    /// API; it guards the invariant against internal misuse and fails with
    /// [`VotingError::InvalidInput`] rather than persisting one wallet's
    /// delegation state under another wallet's scope.
    pub(super) fn scoped_voting_db(&self) -> Result<&Arc<VotingDb>, VotingError> {
        let current = self.voting_db.wallet_id();
        if current != self.wallet_id {
            return Err(VotingError::InvalidInput {
                message: format!(
                    "delegation pipeline is scoped to wallet {} but its database handle now selects wallet {current}",
                    self.wallet_id
                ),
            });
        }
        Ok(&self.voting_db)
    }

    /// The wallet every stage is scoped to, captured at construction.
    pub fn wallet_id(&self) -> &str {
        &self.wallet_id
    }

    pub fn round_id(&self) -> &str {
        &self.lwd.round_params.vote_round_id
    }

    pub fn network(&self) -> Network {
        self.lwd.network
    }

    pub fn snapshot_height(&self) -> u64 {
        self.lwd.round_params.snapshot_height
    }
}

#[cfg(test)]
mod tests;
