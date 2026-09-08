//! Process-wide admission for initial helper POST operations.

use std::{sync::LazyLock, time::Duration};

use tokio::{
    sync::{Semaphore, SemaphorePermit},
    time::Instant,
};

use crate::{
    helper::client::HelperError,
    share_policy::{
        SHARE_DELIVERY_MIN_ATTEMPT_BUDGET_MILLISECONDS, SHARE_HELPER_MAX_CONCURRENT_POSTS,
    },
};

static POST_PERMITS: LazyLock<Semaphore> =
    LazyLock::new(|| Semaphore::new(SHARE_HELPER_MAX_CONCURRENT_POSTS));

/// Reserves one initial POST slot, including any same-helper retry sequence.
///
/// Cancellation is checked every 50 milliseconds while queued. Admission
/// requires at least the minimum attempt budget before the delivery deadline;
/// rejection is definitely unsent, so callers can clear the durable reservation.
/// Dropping the returned permit releases capacity, including on cancellation
/// or timeout of the owning delivery future.
pub(super) async fn acquire(
    deadline: Instant,
    cancel: &(dyn Fn() -> bool + Send + Sync),
) -> Result<SemaphorePermit<'static>, HelperError> {
    let minimum_budget = Duration::from_millis(SHARE_DELIVERY_MIN_ATTEMPT_BUDGET_MILLISECONDS);
    let admission = POST_PERMITS.acquire();
    tokio::pin!(admission);
    loop {
        if cancel() {
            return Err(HelperError::Cancelled);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining <= minimum_budget {
            return Err(HelperError::DeadlineExceeded);
        }
        tokio::select! {
            biased;
            permit = &mut admission => {
                return permit.map_err(|_| HelperError::InvalidRequest {
                    message: "helper POST capacity semaphore closed".to_string(),
                });
            }
            _ = tokio::time::sleep((remaining - minimum_budget).min(Duration::from_millis(50))) => {}
        }
    }
}
