use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, OnceLock, TryLockError, Weak},
};

use crate::VotingError;

use super::DelegationProofIdentity;

type ProofLockRegistry = Mutex<HashMap<DelegationProofIdentity, Weak<Mutex<()>>>>;

static DELEGATION_PROOF_LOCKS: OnceLock<ProofLockRegistry> = OnceLock::new();

thread_local! {
    static ACTIVE_PROOF_IDENTITIES: RefCell<HashSet<DelegationProofIdentity>> =
        RefCell::new(HashSet::new());
}

pub(super) fn run_exclusively<T>(
    identity: DelegationProofIdentity,
    on_wait: impl FnOnce(),
    operation: impl FnOnce(&DelegationProofIdentity) -> Result<T, VotingError>,
) -> Result<T, VotingError> {
    if proof_is_active_on_current_thread(&identity) {
        return Err(VotingError::Busy {
            message: format!(
                "delegation proof generation is already active on this thread for round {} bundle {}",
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

fn proof_is_active_on_current_thread(identity: &DelegationProofIdentity) -> bool {
    ACTIVE_PROOF_IDENTITIES.with(|active| active.borrow().contains(identity))
}

struct ActiveProofIdentity {
    identity: DelegationProofIdentity,
}

impl ActiveProofIdentity {
    fn enter(identity: DelegationProofIdentity) -> Self {
        ACTIVE_PROOF_IDENTITIES.with(|active| {
            let inserted = active.borrow_mut().insert(identity.clone());
            debug_assert!(
                inserted,
                "reentrant proof identity passed the preflight check"
            );
        });
        Self { identity }
    }
}

impl Drop for ActiveProofIdentity {
    fn drop(&mut self) {
        ACTIVE_PROOF_IDENTITIES.with(|active| {
            active.borrow_mut().remove(&self.identity);
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
