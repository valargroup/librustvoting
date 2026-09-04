use std::{
    cell::RefCell,
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock, TryLockError, Weak},
};

use crate::VotingError;

use super::DelegationProofIdentity;

type ProofLockRegistry = Mutex<HashMap<DelegationProofIdentity, Weak<Mutex<()>>>>;

static DELEGATION_PROOF_LOCKS: OnceLock<ProofLockRegistry> = OnceLock::new();

thread_local! {
    static ACTIVE_PROOF_IDENTITY: RefCell<Option<DelegationProofIdentity>> =
        RefCell::new(None);
}

pub(super) fn run_exclusively<T>(
    identity: DelegationProofIdentity,
    on_wait: impl FnOnce(),
    operation: impl FnOnce(&DelegationProofIdentity) -> Result<T, VotingError>,
) -> Result<T, VotingError> {
    if let Some(active_identity) = active_proof_identity_on_current_thread() {
        return Err(VotingError::Busy {
            message: format!(
                "delegation proof generation is already active on this thread for round {} bundle {}; cannot enter round {} bundle {}",
                active_identity.round_id(),
                active_identity.bundle_index(),
                identity.round_id(),
                identity.bundle_index(),
            ),
        });
    }

    // Cover both the wait callback and the admitted operation. A callback may
    // synchronously enter the public proof facade before this thread owns the
    // process-local lock.
    let active_identity = ActiveProofIdentity::enter(identity.clone());
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

    let output = operation(&identity);
    drop(active_identity);
    drop(proof_guard);
    remove_unused_lock(&identity, &proof_lock);
    output
}

fn active_proof_identity_on_current_thread() -> Option<DelegationProofIdentity> {
    ACTIVE_PROOF_IDENTITY.with(|active| active.borrow().clone())
}

struct ActiveProofIdentity {
    identity: DelegationProofIdentity,
}

impl ActiveProofIdentity {
    fn enter(identity: DelegationProofIdentity) -> Self {
        ACTIVE_PROOF_IDENTITY.with(|active| {
            let previous = active.borrow_mut().replace(identity.clone());
            debug_assert!(
                previous.is_none(),
                "nested proof passed the preflight check"
            );
        });
        Self { identity }
    }
}

impl Drop for ActiveProofIdentity {
    fn drop(&mut self) {
        ACTIVE_PROOF_IDENTITY.with(|active| {
            let removed = active.borrow_mut().take();
            debug_assert_eq!(removed.as_ref(), Some(&self.identity));
        });
    }
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
