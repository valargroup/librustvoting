//! Process-local lifecycle serialization in the normative lock order.

use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex, Weak},
};

use tokio::sync::{OwnedMutexGuard, OwnedRwLockReadGuard, OwnedRwLockWriteGuard};

use crate::types::Network;

use super::{ChainSubmissionFailure, ChainSubmissionFailureKind, ChainSubmissionIdentity};

/// Immutable host and submission scope captured before lifecycle work begins.
#[derive(Clone)]
pub(super) struct CapturedSubmissionOperation {
    identity: ChainSubmissionIdentity,
    host_operation_epoch: u64,
}

impl CapturedSubmissionOperation {
    pub(super) fn new(identity: ChainSubmissionIdentity, host_operation_epoch: u64) -> Self {
        Self {
            identity,
            host_operation_epoch,
        }
    }

    pub(super) fn identity(&self) -> &ChainSubmissionIdentity {
        &self.identity
    }

    pub(super) fn host_operation_epoch(&self) -> u64 {
        self.host_operation_epoch
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct RoundOperationKey {
    wallet_id: String,
    network: u8,
    vote_round_id: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) struct BundleOperationKey {
    round: RoundOperationKey,
    bundle_index: u32,
}

impl BundleOperationKey {
    pub(super) fn from_identity(identity: &ChainSubmissionIdentity) -> Self {
        Self {
            round: round_key(identity),
            bundle_index: identity.bundle_index(),
        }
    }
}

/// Canonical process-local key for one submission identity.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) struct SubmissionOperationKey {
    bundle: BundleOperationKey,
    target_kind: u8,
    target_value: Vec<u8>,
}

impl SubmissionOperationKey {
    pub(super) fn from_identity(identity: &ChainSubmissionIdentity) -> Self {
        use super::ChainSubmissionTarget;

        let (target_kind, target_value) = match identity.target() {
            ChainSubmissionTarget::Delegation => (0, vec![]),
            ChainSubmissionTarget::Vote { proposal_id } => (1, proposal_id.to_be_bytes().to_vec()),
            ChainSubmissionTarget::VoteBatch {
                ordered_batch_digest,
            } => (2, ordered_batch_digest.to_vec()),
            ChainSubmissionTarget::DelegateAndVoteBatch {
                ordered_batch_digest,
            } => (3, ordered_batch_digest.to_vec()),
        };
        Self {
            bundle: BundleOperationKey {
                round: round_key(identity),
                bundle_index: identity.bundle_index(),
            },
            target_kind,
            target_value,
        }
    }
}

fn network_rank(network: Network) -> u8 {
    match network {
        Network::Mainnet => 0,
        Network::Testnet => 1,
        Network::Regtest => 2,
    }
}

fn round_key(identity: &ChainSubmissionIdentity) -> RoundOperationKey {
    RoundOperationKey {
        wallet_id: identity.wallet_id().to_string(),
        network: network_rank(identity.network()),
        vote_round_id: *identity.vote_round_id(),
    }
}

/// Shared registries used by every coordinator for one database authority.
///
/// Weak entries prevent a long-running wallet from retaining a lock for every
/// historical round. Registry mutexes are never held across an async wait.
#[derive(Default)]
pub(crate) struct SubmissionCoordination {
    account_gates: Mutex<HashMap<String, Weak<tokio::sync::RwLock<()>>>>,
    round_gates: Mutex<HashMap<RoundOperationKey, Weak<tokio::sync::RwLock<()>>>>,
    bundle_locks: Mutex<HashMap<BundleOperationKey, Weak<tokio::sync::Mutex<()>>>>,
    identity_locks: Mutex<HashMap<SubmissionOperationKey, Weak<tokio::sync::Mutex<()>>>>,
    in_flight: Arc<Mutex<BTreeMap<SubmissionOperationKey, usize>>>,
}

impl SubmissionCoordination {
    /// Acquires shared round access, the bundle, then all identities in sorted
    /// order. Store methods may acquire their database handle only afterwards.
    pub(super) async fn acquire(
        &self,
        operation: &CapturedSubmissionOperation,
        applicable_identities: &[ChainSubmissionIdentity],
    ) -> Result<SubmissionOperationLease, ChainSubmissionFailure> {
        let account_gate = shared_lock(
            &self.account_gates,
            operation.identity().wallet_id().to_string(),
            || tokio::sync::RwLock::new(()),
        )?;
        let account_guard = account_gate.read_owned().await;
        let round_key = round_key(operation.identity());
        let round_gate = shared_lock(&self.round_gates, round_key.clone(), || {
            tokio::sync::RwLock::new(())
        })?;
        let round_guard = round_gate.read_owned().await;

        let bundle_key = BundleOperationKey::from_identity(operation.identity());
        let bundle_lock = shared_lock(&self.bundle_locks, bundle_key, || {
            tokio::sync::Mutex::new(())
        })?;
        let bundle_guard = bundle_lock.lock_owned().await;

        let mut identity_keys = applicable_identities
            .iter()
            .map(SubmissionOperationKey::from_identity)
            .chain(std::iter::once(SubmissionOperationKey::from_identity(
                operation.identity(),
            )))
            .collect::<Vec<_>>();
        identity_keys.sort();
        identity_keys.dedup();

        let mut identity_guards = Vec::with_capacity(identity_keys.len());
        for key in &identity_keys {
            let identity_lock = shared_lock(&self.identity_locks, key.clone(), || {
                tokio::sync::Mutex::new(())
            })?;
            identity_guards.push(identity_lock.lock_owned().await);
        }

        Ok(SubmissionOperationLease {
            _account_guard: account_guard,
            _round_guard: round_guard,
            _bundle_guard: bundle_guard,
            _identity_guards: identity_guards,
            identity_keys,
        })
    }

    pub(crate) fn try_acquire_account_exclusive(
        &self,
        wallet_id: &str,
    ) -> Result<ExclusiveAccountLease, ExclusiveRoundAcquireError> {
        let gate = shared_lock(&self.account_gates, wallet_id.to_string(), || {
            tokio::sync::RwLock::new(())
        })
        .map_err(ExclusiveRoundAcquireError::Failure)?;
        gate.try_write_owned()
            .map(|guard| ExclusiveAccountLease { _guard: guard })
            .map_err(|_| ExclusiveRoundAcquireError::Busy)
    }

    /// Attempts to admit delegation setup alongside unrelated bundles while
    /// excluding account cleanup, round deletion, and lifecycle work for the
    /// same bundle.
    ///
    /// The acquisition order matches [`SubmissionCoordination::acquire`].
    /// Setup is synchronous, so contention is reported to its retrying caller
    /// instead of waiting on Tokio locks.
    pub(crate) fn try_acquire_delegation_setup(
        &self,
        identity: &ChainSubmissionIdentity,
    ) -> Result<DelegationSetupLease, ExclusiveRoundAcquireError> {
        let account_gate = shared_lock(
            &self.account_gates,
            identity.wallet_id().to_string(),
            || tokio::sync::RwLock::new(()),
        )
        .map_err(ExclusiveRoundAcquireError::Failure)?;
        let account_guard = account_gate
            .try_read_owned()
            .map_err(|_| ExclusiveRoundAcquireError::Busy)?;

        let round_key = round_key(identity);
        let round_gate = shared_lock(
            &self.round_gates,
            round_key,
            || tokio::sync::RwLock::new(()),
        )
        .map_err(ExclusiveRoundAcquireError::Failure)?;
        let round_guard = round_gate
            .try_read_owned()
            .map_err(|_| ExclusiveRoundAcquireError::Busy)?;

        let bundle_key = BundleOperationKey::from_identity(identity);
        let bundle_lock = shared_lock(&self.bundle_locks, bundle_key, || {
            tokio::sync::Mutex::new(())
        })
        .map_err(ExclusiveRoundAcquireError::Failure)?;
        let bundle_guard = bundle_lock
            .try_lock_owned()
            .map_err(|_| ExclusiveRoundAcquireError::Busy)?;

        Ok(DelegationSetupLease {
            _account_guard: account_guard,
            _round_guard: round_guard,
            _bundle_guard: bundle_guard,
        })
    }

    pub(super) fn register_in_flight(
        &self,
        identity: &ChainSubmissionIdentity,
    ) -> Result<InFlightSubmission, ChainSubmissionFailure> {
        let key = SubmissionOperationKey::from_identity(identity);
        let mut in_flight = self.in_flight.lock().map_err(|_| {
            ChainSubmissionFailure::without_state(
                ChainSubmissionFailureKind::InvariantViolation,
                "chain submission in-flight registry is poisoned",
            )
        })?;
        *in_flight.entry(key.clone()).or_default() += 1;
        drop(in_flight);
        Ok(InFlightSubmission {
            registry: Arc::clone(&self.in_flight),
            key,
        })
    }

    /// Attempts to exclude lifecycle work for cleanup or deletion of a round.
    /// A busy result is authoritative and must not be bypassed by consulting
    /// the registry separately.
    pub(crate) fn try_acquire_round_exclusive(
        &self,
        identity: &ChainSubmissionIdentity,
    ) -> Result<ExclusiveRoundLease, ExclusiveRoundAcquireError> {
        let key = round_key(identity);
        let gate = shared_lock(&self.round_gates, key, || tokio::sync::RwLock::new(()))
            .map_err(ExclusiveRoundAcquireError::Failure)?;
        gate.try_write_owned()
            .map(|guard| ExclusiveRoundLease { _guard: guard })
            .map_err(|_| ExclusiveRoundAcquireError::Busy)
    }

    pub(super) fn has_in_flight_for_round(
        &self,
        identity: &ChainSubmissionIdentity,
    ) -> Result<bool, ChainSubmissionFailure> {
        let round = round_key(identity);
        let in_flight = self.in_flight.lock().map_err(|_| {
            ChainSubmissionFailure::without_state(
                ChainSubmissionFailureKind::InvariantViolation,
                "chain submission in-flight registry is poisoned",
            )
        })?;
        Ok(in_flight.keys().any(|key| key.bundle.round == round))
    }

    #[cfg(test)]
    pub(super) fn identity_locks_are_held(
        &self,
        identities: &[ChainSubmissionIdentity],
    ) -> Result<bool, ChainSubmissionFailure> {
        let registry = self.identity_locks.lock().map_err(|_| {
            ChainSubmissionFailure::without_state(
                ChainSubmissionFailureKind::InvariantViolation,
                "chain submission identity-lock registry is poisoned",
            )
        })?;
        Ok(identities.iter().all(|identity| {
            registry
                .get(&SubmissionOperationKey::from_identity(identity))
                .and_then(Weak::upgrade)
                .is_some_and(|lock| lock.try_lock().is_err())
        }))
    }
}

pub(crate) enum ExclusiveRoundAcquireError {
    Busy,
    Failure(ChainSubmissionFailure),
}

pub(crate) struct ExclusiveRoundLease {
    _guard: OwnedRwLockWriteGuard<()>,
}

pub(crate) struct ExclusiveAccountLease {
    _guard: OwnedRwLockWriteGuard<()>,
}

/// Continuously-held exclusion for one bundle's delegation setup.
pub(crate) struct DelegationSetupLease {
    _account_guard: OwnedRwLockReadGuard<()>,
    _round_guard: OwnedRwLockReadGuard<()>,
    _bundle_guard: OwnedMutexGuard<()>,
}

fn shared_lock<K, L>(
    registry: &Mutex<HashMap<K, Weak<L>>>,
    key: K,
    create: impl FnOnce() -> L,
) -> Result<Arc<L>, ChainSubmissionFailure>
where
    K: Eq + std::hash::Hash,
{
    let mut registry = registry.lock().map_err(|_| {
        ChainSubmissionFailure::without_state(
            ChainSubmissionFailureKind::InvariantViolation,
            "chain submission lock registry is poisoned",
        )
    })?;
    registry.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = registry.get(&key).and_then(Weak::upgrade) {
        return Ok(lock);
    }
    let lock = Arc::new(create());
    registry.insert(key, Arc::downgrade(&lock));
    Ok(lock)
}

/// Continuously-held lifecycle locks. Dropping this value ends the operation.
pub(super) struct SubmissionOperationLease {
    _account_guard: OwnedRwLockReadGuard<()>,
    _round_guard: OwnedRwLockReadGuard<()>,
    _bundle_guard: OwnedMutexGuard<()>,
    _identity_guards: Vec<OwnedMutexGuard<()>>,
    identity_keys: Vec<SubmissionOperationKey>,
}

impl SubmissionOperationLease {
    pub(super) fn identity_keys(&self) -> &[SubmissionOperationKey] {
        &self.identity_keys
    }
}

/// Covers one committed reservation until its network result is durable.
pub(super) struct InFlightSubmission {
    registry: Arc<Mutex<BTreeMap<SubmissionOperationKey, usize>>>,
    key: SubmissionOperationKey,
}

impl Drop for InFlightSubmission {
    fn drop(&mut self) {
        if let Ok(mut registry) = self.registry.lock() {
            if let Some(count) = registry.get_mut(&self.key) {
                *count -= 1;
                if *count == 0 {
                    registry.remove(&self.key);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain_submission::ChainSubmissionTarget;

    mod delegation_setup;

    fn identity(target: ChainSubmissionTarget, bundle_index: u32) -> ChainSubmissionIdentity {
        ChainSubmissionIdentity::new("wallet", Network::Testnet, [1; 32], bundle_index, target)
            .unwrap()
    }

    #[tokio::test]
    async fn identities_are_locked_once_in_canonical_order() {
        let native = identity(
            ChainSubmissionTarget::VoteBatch {
                ordered_batch_digest: [9; 32],
            },
            3,
        );
        let member_two = identity(ChainSubmissionTarget::Vote { proposal_id: 2 }, 3);
        let member_one = identity(ChainSubmissionTarget::Vote { proposal_id: 1 }, 3);
        let operation = CapturedSubmissionOperation::new(native, 4);
        let coordination = SubmissionCoordination::default();
        let lease = coordination
            .acquire(
                &operation,
                &[member_two.clone(), member_one.clone(), member_two],
            )
            .await
            .unwrap();

        assert_eq!(lease.identity_keys().len(), 3);
        assert!(lease
            .identity_keys()
            .windows(2)
            .all(|keys| keys[0] < keys[1]));
    }

    #[tokio::test]
    async fn exclusive_round_access_is_busy_until_lifecycle_work_finishes() {
        let identity = identity(ChainSubmissionTarget::Vote { proposal_id: 1 }, 3);
        let operation = CapturedSubmissionOperation::new(identity.clone(), 4);
        let coordination = SubmissionCoordination::default();
        let lease = coordination
            .acquire(&operation, std::slice::from_ref(&identity))
            .await
            .unwrap();
        let in_flight = coordination.register_in_flight(&identity).unwrap();

        assert!(matches!(
            coordination.try_acquire_round_exclusive(&identity),
            Err(ExclusiveRoundAcquireError::Busy)
        ));
        assert!(coordination.has_in_flight_for_round(&identity).unwrap());

        drop(in_flight);
        drop(lease);
        assert!(!coordination.has_in_flight_for_round(&identity).unwrap());
        assert!(coordination.try_acquire_round_exclusive(&identity).is_ok());
    }

    #[tokio::test]
    async fn exclusive_account_access_blocks_every_round_for_the_wallet() {
        let identity = identity(ChainSubmissionTarget::Vote { proposal_id: 1 }, 3);
        let operation = CapturedSubmissionOperation::new(identity.clone(), 4);
        let coordination = SubmissionCoordination::default();
        let lease = coordination
            .acquire(&operation, std::slice::from_ref(&identity))
            .await
            .unwrap();

        assert!(matches!(
            coordination.try_acquire_account_exclusive("wallet"),
            Err(ExclusiveRoundAcquireError::Busy)
        ));
        assert!(coordination
            .try_acquire_account_exclusive("different-wallet")
            .is_ok());
        drop(lease);
        assert!(coordination.try_acquire_account_exclusive("wallet").is_ok());
    }
}
