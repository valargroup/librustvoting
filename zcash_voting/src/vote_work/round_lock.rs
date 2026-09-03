use std::{
    collections::HashMap,
    sync::{Arc, LazyLock, Mutex, Weak},
    time::Duration,
};

use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

use crate::ChainSubmissionControl;

const ROUND_LOCK_CANCEL_CHECK_MILLISECONDS: u64 = 50;

/// `(wallet_id, round_id, bundle)`. `None` is the round-wide scope used by
/// chain and share steps; `Some(bundle)` scopes delegation work so bundles
/// prove and sign concurrently.
type RoundLockKey = (String, String, Option<u32>);

static ROUND_LOCKS: LazyLock<Mutex<HashMap<RoundLockKey, Weak<AsyncMutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(super) async fn acquire(
    wallet_id: String,
    round_id: &str,
    bundle_index: Option<u32>,
    control: &ChainSubmissionControl,
) -> Result<Option<OwnedMutexGuard<()>>, String> {
    let key = (wallet_id, round_id.to_string(), bundle_index);
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
        if control.is_cancelled() {
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
