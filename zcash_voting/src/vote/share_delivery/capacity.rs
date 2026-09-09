//! Admission shared by every singleton and multi-proposal initial delivery.

use crate::VotingError;
use std::{sync::LazyLock, time::Duration};
use tokio::sync::{Semaphore, SemaphorePermit};

/// Active share workflows, independently of the lower-level 128-POST ceiling.
pub(super) const MAX_CONCURRENT_SHARE_DELIVERIES: usize = 32;
static DELIVERY_PERMITS: LazyLock<Semaphore> =
    LazyLock::new(|| Semaphore::new(MAX_CONCURRENT_SHARE_DELIVERIES));

/// Waits for capacity without creating durable reservations. Cancellation is
/// observed within 50 ms even while all permits remain occupied. The caller
/// holds the returned permit until its share's outcomes have been journaled.
pub(super) async fn acquire(
    cancel: &(dyn Fn() -> bool + Send + Sync),
) -> Result<Option<SemaphorePermit<'static>>, VotingError> {
    let admission = DELIVERY_PERMITS.acquire();
    tokio::pin!(admission);
    loop {
        if cancel() {
            return Ok(None);
        }
        tokio::select! {
            biased;
            permit = &mut admission => {
                let permit = permit.map_err(|_| VotingError::Internal { message: "helper-share delivery semaphore closed".to_string() })?;
                return Ok((!cancel()).then_some(permit));
            }
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
    }
}
