use std::{
    collections::{HashSet, VecDeque},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use tokio::task::JoinSet;

use crate::helper::client::{HelperClient, HelperError, ShareStatus};

use super::{
    SHARE_STATUS_CANCEL_CHECK_MILLISECONDS, SHARE_STATUS_MAX_CONCURRENT_POLLS,
    SHARE_STATUS_POLL_BUDGET_MILLISECONDS,
};

/// Configured-fleet evidence observed while polling one share.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ShareStatusOutcome {
    /// The required distinct configured helpers reported confirmation.
    ConfiguredHelperQuorumObserved,
    /// Polling ended without the required configured-helper quorum.
    ConfiguredHelperQuorumNotObserved,
    /// The caller stopped polling before further evidence could be collected.
    Cancelled,
}

/// Polls helpers for the share's global on-chain confirmation state.
///
/// At most [`SHARE_STATUS_MAX_CONCURRENT_POLLS`] requests run concurrently and
/// the complete quorum search is capped by
/// [`SHARE_STATUS_POLL_BUDGET_MILLISECONDS`]. Reaching the quorum, caller
/// cancellation, or budget expiry aborts every outstanding request. Helpers
/// still in flight at budget expiry are degraded so later passes prefer other
/// configured endpoints.
pub(super) async fn poll_share_helpers(
    client: &HelperClient,
    round_id: &str,
    share_id: &str,
    server_urls: &[String],
    now_seconds: u64,
    cancel: &(dyn Fn() -> bool + Send + Sync),
) -> ShareStatusOutcome {
    const REQUIRED_CONFIRMATIONS: usize = 2;
    let required_confirmations = REQUIRED_CONFIRMATIONS.min(server_urls.len());
    if required_confirmations == 0 {
        return ShareStatusOutcome::ConfiguredHelperQuorumNotObserved;
    }

    let mut remaining = VecDeque::from(client.health().candidate_servers(server_urls, now_seconds));
    let mut polls = JoinSet::new();
    let mut in_flight = HashSet::new();
    let task_cancelled = Arc::new(AtomicBool::new(false));
    let deadline =
        tokio::time::Instant::now() + Duration::from_millis(SHARE_STATUS_POLL_BUDGET_MILLISECONDS);
    let mut confirmations = 0usize;

    loop {
        // A cancellation observed after the final completed request must not
        // replace that request's definite result.
        if polls.is_empty() && remaining.is_empty() {
            return ShareStatusOutcome::ConfiguredHelperQuorumNotObserved;
        }
        if cancel() {
            task_cancelled.store(true, Ordering::Relaxed);
            polls.abort_all();
            return ShareStatusOutcome::Cancelled;
        }

        while polls.len() < SHARE_STATUS_MAX_CONCURRENT_POLLS {
            let Some(server_url) = remaining.pop_front() else {
                break;
            };
            in_flight.insert(server_url.clone());
            let client = client.clone();
            let round_id = round_id.to_string();
            let share_id = share_id.to_string();
            let task_cancelled = Arc::clone(&task_cancelled);
            polls.spawn(async move {
                let outcome = client
                    .share_status(&server_url, &round_id, &share_id, now_seconds, &|| {
                        task_cancelled.load(Ordering::Relaxed)
                    })
                    .await;
                (server_url, outcome)
            });
        }

        if polls.is_empty() {
            return ShareStatusOutcome::ConfiguredHelperQuorumNotObserved;
        }

        let now = tokio::time::Instant::now();
        if now >= deadline {
            task_cancelled.store(true, Ordering::Relaxed);
            polls.abort_all();
            let failure_time =
                now_seconds.saturating_add(SHARE_STATUS_POLL_BUDGET_MILLISECONDS.div_ceil(1_000));
            for server_url in in_flight {
                client.health().record_failure(&server_url, failure_time);
            }
            return ShareStatusOutcome::ConfiguredHelperQuorumNotObserved;
        }
        let cancel_check = now + Duration::from_millis(SHARE_STATUS_CANCEL_CHECK_MILLISECONDS);
        let wait_deadline = deadline.min(cancel_check);
        let joined = match tokio::time::timeout_at(wait_deadline, polls.join_next()).await {
            Ok(joined) => joined,
            Err(_) => continue,
        };
        let Some(joined) = joined else {
            continue;
        };
        let Ok((server_url, outcome)) = joined else {
            continue;
        };
        in_flight.remove(&server_url);
        match outcome {
            Ok(ShareStatus::Confirmed) => {
                confirmations += 1;
                if confirmations == required_confirmations {
                    task_cancelled.store(true, Ordering::Relaxed);
                    polls.abort_all();
                    return ShareStatusOutcome::ConfiguredHelperQuorumObserved;
                }
            }
            // The helper is alive but has not revealed yet. Keep walking.
            Ok(ShareStatus::Pending) => {}
            Err(HelperError::Cancelled) if cancel() => {
                task_cancelled.store(true, Ordering::Relaxed);
                polls.abort_all();
                return ShareStatusOutcome::Cancelled;
            }
            Err(HelperError::Cancelled) => {}
            // Any transport, HTTP, or out-of-protocol failure was already
            // scored by the client.
            Err(_) => continue,
        }
    }
}
