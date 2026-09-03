//! Process-local coordination for delegation proof generation.
//!
//! Proofs for distinct bundle identities remain independent. Calls for the
//! same wallet, round, and bundle serialize around a durable-state recheck so
//! only one caller performs expensive ZKP1 generation.

mod proof_lock;

use crate::VotingError;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct DelegationProofIdentity {
    wallet_id: String,
    round_id: String,
    bundle_index: u32,
}

impl DelegationProofIdentity {
    pub(super) fn new(wallet_id: String, round_id: &str, bundle_index: u32) -> Self {
        Self {
            wallet_id,
            round_id: round_id.to_string(),
            bundle_index,
        }
    }

    pub(super) fn wallet_id(&self) -> &str {
        &self.wallet_id
    }

    pub(super) fn round_id(&self) -> &str {
        &self.round_id
    }

    pub(super) fn bundle_index(&self) -> u32 {
        self.bundle_index
    }
}

/// Runs one proof operation exclusively for `identity`.
///
/// `on_wait` runs before blocking behind an existing operation. The operation
/// must re-read durable proof state after admission; coordination itself does
/// not treat an earlier caller's success as authoritative. Reentry for the
/// same identity on the current thread returns [`VotingError::Busy`] instead
/// of waiting on the thread's own lock.
pub(super) fn coordinate<T>(
    identity: DelegationProofIdentity,
    on_wait: impl FnOnce(),
    operation: impl FnOnce(&DelegationProofIdentity) -> Result<T, VotingError>,
) -> Result<T, VotingError> {
    proof_lock::run_exclusively(identity, on_wait, operation)
}

#[cfg(test)]
mod tests;
