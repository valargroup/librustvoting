use std::{
    collections::HashMap,
    sync::{Arc, LazyLock, Mutex, Weak},
    time::Duration,
};

use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

use crate::{session::NextStep, ChainSubmissionControl};

const ROUND_LOCK_CANCEL_CHECK_MILLISECONDS: u64 = 50;

/// The lock `step` takes: `Some(bundle_index)` for work scoped to one bundle,
/// `None` for the round-wide scope.
///
/// This is the **only** place that decision is made. The executor locks with
/// it and the round driver schedules with it, because a scheduler that
/// believed two round-locked steps were independent would admit them together
/// and leave them queueing on one lock, each holding a proving worker open.
///
/// The match is exhaustive rather than a wildcard so that a new [`NextStep`]
/// variant is a compile error here, where the choice belongs, instead of
/// silently defaulting to the round scope.
pub(crate) fn bundle_scope(step: &NextStep) -> Option<u32> {
    match step {
        NextStep::Delegate { bundle_index } | NextStep::AdvanceDelegation { bundle_index } => {
            Some(*bundle_index)
        }
        // Imported delegation advancement is chain work on the round's
        // submission rows, not proving; it takes the round scope like every
        // other chain and share step.
        NextStep::AdvanceImportedDelegation { .. }
        | NextStep::CastVote { .. }
        | NextStep::AdvanceVote { .. }
        | NextStep::AdvanceVoteBatch { .. }
        | NextStep::SubmitShares { .. }
        | NextStep::ConfirmShare { .. } => None,
    }
}

/// `(sidecar connection, wallet_id, round_id, bundle)`. `None` is the
/// round-wide scope used by chain and share steps; `Some(bundle)` scopes
/// delegation work so bundles prove and sign concurrently. The connection id
/// keeps two independently opened sidecars that share a wallet id from
/// serializing against each other.
type RoundLockKey = (u64, String, String, Option<u32>);

static ROUND_LOCKS: LazyLock<Mutex<HashMap<RoundLockKey, Weak<AsyncMutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// A held round or bundle lock shared between a step future and a proving
/// thread it detaches, so the lock outlives a dropped future for as long as
/// the thread keeps working.
pub(super) type HeldRoundLock = Arc<OwnedMutexGuard<()>>;

/// Acquires the lock for `(wallet, round, bundle)`, returning `None` if the
/// host cancels or moves to another operation epoch than `entry_epoch` while
/// the caller is queued. A stale caller therefore stops waiting instead of
/// holding its place behind a long-running proof.
pub(super) async fn acquire(
    sidecar_id: u64,
    wallet_id: String,
    round_id: &str,
    bundle_index: Option<u32>,
    control: &ChainSubmissionControl,
    entry_epoch: u64,
) -> Result<Option<OwnedMutexGuard<()>>, String> {
    let key = (sidecar_id, wallet_id, round_id.to_string(), bundle_index);
    let lock = {
        let mut locks = ROUND_LOCKS
            .lock()
            .map_err(|_| "persisted vote round lock registry is poisoned".to_string())?;
        locks.retain(|_, lock| lock.strong_count() > 0);
        match locks.get(&key).and_then(Weak::upgrade) {
            Some(lock) => lock,
            None => {
                let lock = Arc::new(AsyncMutex::new(()));
                locks.insert(key, Arc::downgrade(&lock));
                lock
            }
        }
    };
    let pending = lock.lock_owned();
    tokio::pin!(pending);
    loop {
        if control.is_cancelled() || control.operation_epoch() != entry_epoch {
            return Ok(None);
        }
        tokio::select! {
            biased;
            guard = &mut pending => return Ok(Some(guard)),
            _ = tokio::time::sleep(Duration::from_millis(
                ROUND_LOCK_CANCEL_CHECK_MILLISECONDS,
            )) => {}
        }
    }
}
