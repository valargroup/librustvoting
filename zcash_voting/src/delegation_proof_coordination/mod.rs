//! Process-local coordination for delegation proof generation.
//!
//! Proofs for distinct bundle identities remain independent. Calls for the
//! same wallet, round, and bundle serialize around a durable-state recheck so
//! only one caller performs expensive ZKP1 generation.

mod proof_lock;

use std::sync::Mutex;

use crate::{delegate::DelegationProgress, types::DelegationProgressReporter, VotingError};

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

/// Collects proof progress while coordination is locked for delivery after
/// the durable proof operation has released its lock.
#[derive(Default)]
pub(super) struct DeferredProgressReporter {
    events: Mutex<Vec<DelegationProgress>>,
}

impl DeferredProgressReporter {
    pub(super) fn replay(&self, reporter: &dyn DelegationProgressReporter) {
        let events = self
            .events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for event in events.iter().copied() {
            reporter.on_progress(event);
        }
    }
}

impl DelegationProgressReporter for DeferredProgressReporter {
    fn on_progress(&self, progress: DelegationProgress) {
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(progress);
    }
}

/// Runs one proof operation exclusively for `identity`.
///
/// `on_wait` runs before blocking behind an existing operation. The operation
/// must re-read durable proof state after admission; coordination itself does
/// not treat an earlier caller's success as authoritative. Any nested proof
/// coordination on the current thread returns [`VotingError::Busy`], including
/// a different identity, to prevent callback-driven lock-order deadlocks.
pub(super) fn coordinate<T>(
    identity: DelegationProofIdentity,
    on_wait: impl FnOnce(),
    operation: impl FnOnce(&DelegationProofIdentity) -> Result<T, VotingError>,
) -> Result<T, VotingError> {
    proof_lock::run_exclusively(identity, on_wait, operation)
}

#[cfg(test)]
mod tests;
