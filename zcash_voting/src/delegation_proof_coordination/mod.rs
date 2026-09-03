//! Process-local coordination for delegation proof generation.
//!
//! Proofs for distinct bundle identities remain independent. Calls for the
//! same wallet, round, and bundle serialize around a durable-state recheck so
//! only one caller performs expensive ZKP1 generation.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock, TryLockError, Weak},
};

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
}

type ProofLockRegistry = Mutex<HashMap<DelegationProofIdentity, Weak<Mutex<()>>>>;

static DELEGATION_PROOF_LOCKS: OnceLock<ProofLockRegistry> = OnceLock::new();

/// Runs one proof operation exclusively for `identity`.
///
/// `on_wait` runs before blocking behind an existing operation. The operation
/// must re-read durable proof state after admission; coordination itself does
/// not treat an earlier caller's success as authoritative.
pub(super) fn coordinate<T>(
    identity: DelegationProofIdentity,
    on_wait: impl FnOnce(),
    operation: impl FnOnce() -> T,
) -> T {
    let proof_lock = proof_lock_for(&identity);
    let proof_guard = match proof_lock.try_lock() {
        Ok(guard) => guard,
        Err(TryLockError::WouldBlock) => {
            on_wait();
            proof_lock
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
        }
        Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
    };

    let output = operation();
    drop(proof_guard);
    remove_unused_lock(&identity, &proof_lock);
    output
}

fn proof_lock_for(identity: &DelegationProofIdentity) -> Arc<Mutex<()>> {
    let mut locks = DELEGATION_PROOF_LOCKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    locks.retain(|_, proof_lock| proof_lock.strong_count() > 0);
    if let Some(proof_lock) = locks.get(identity).and_then(Weak::upgrade) {
        return proof_lock;
    }

    let proof_lock = Arc::new(Mutex::new(()));
    locks.insert(identity.clone(), Arc::downgrade(&proof_lock));
    proof_lock
}

fn remove_unused_lock(identity: &DelegationProofIdentity, proof_lock: &Arc<Mutex<()>>) {
    let mut locks = DELEGATION_PROOF_LOCKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if Arc::strong_count(proof_lock) == 1
        && locks
            .get(identity)
            .is_some_and(|registered| registered.ptr_eq(&Arc::downgrade(proof_lock)))
    {
        locks.remove(identity);
    }
}

#[cfg(test)]
mod tests;
