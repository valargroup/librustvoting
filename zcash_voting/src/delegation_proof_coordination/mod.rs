//! Process-local coordination for delegation proof generation.
//!
//! Proofs for distinct bundle identities remain independent. Calls for the
//! same wallet, round, and bundle serialize around a durable-state recheck so
//! only one caller performs expensive ZKP1 generation.

mod proof_lock;

use std::{
    sync::{mpsc, Mutex},
    thread,
};

use crate::{delegate::DelegationProgress, types::DelegationProgressReporter, VotingError};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct DelegationProofIdentity {
    /// The sidecar the proof is persisted in: one id per sidecar file in the
    /// process, so separately opened connections to one file single-flight
    /// on the same proof, while two sidecars that share a wallet id do not
    /// serialize on each other's proofs.
    sidecar_id: u64,
    wallet_id: String,
    round_id: String,
    bundle_index: u32,
}

impl DelegationProofIdentity {
    pub(super) fn new(
        sidecar_id: u64,
        wallet_id: String,
        round_id: &str,
        bundle_index: u32,
    ) -> Self {
        Self {
            sidecar_id,
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

/// Runs `operation` while a dedicated delivery thread forwards its progress to
/// `host` as it happens.
///
/// The operation only enqueues events, so it never blocks on the host. A host
/// callback may therefore enter proof coordination directly or hand it to
/// another thread: at worst that work waits for the operation to release its
/// proof lock, which the operation does without waiting on the callback.
/// Delivery preserves emission order, and this function returns only after
/// every emitted event has been delivered.
///
/// A panic inside a host callback surfaces here after the operation finishes.
pub(super) fn with_live_progress<T>(
    host: &dyn DelegationProgressReporter,
    operation: impl FnOnce(&dyn DelegationProgressReporter) -> T,
) -> T {
    thread::scope(|scope| {
        let (sender, receiver) = mpsc::channel::<DelegationProgress>();
        scope.spawn(move || {
            for event in receiver {
                host.on_progress(event);
            }
        });
        let relay = LiveProgressRelay {
            sender: Mutex::new(sender),
        };
        let output = operation(&relay);
        // Closing the channel lets the delivery thread drain and exit; the
        // scope then joins it before returning.
        drop(relay);
        output
    })
}

/// Enqueues proof progress for the delivery thread in [`with_live_progress`].
struct LiveProgressRelay {
    sender: Mutex<mpsc::Sender<DelegationProgress>>,
}

impl DelegationProgressReporter for LiveProgressRelay {
    fn on_progress(&self, progress: DelegationProgress) {
        // A closed receiver only happens if delivery already stopped; there is
        // nobody left to notify, so dropping the event is the correct outcome.
        let _ = self
            .sender
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .send(progress);
    }
}

/// Runs one proof operation exclusively for `identity`.
///
/// `on_wait` runs before blocking behind an existing operation. The operation
/// must re-read durable proof state after admission; coordination itself does
/// not treat an earlier caller's success as authoritative. Any nested proof
/// coordination on the current thread returns [`VotingError::Busy`], including
/// a different identity, to prevent callback-driven lock-order deadlocks.
/// Host progress callbacks never run on the operation's thread; see
/// [`with_live_progress`].
pub(super) fn coordinate<T>(
    identity: DelegationProofIdentity,
    on_wait: impl FnOnce(),
    operation: impl FnOnce(&DelegationProofIdentity) -> Result<T, VotingError>,
) -> Result<T, VotingError> {
    proof_lock::run_exclusively(identity, on_wait, operation)
}

#[cfg(test)]
mod tests;
