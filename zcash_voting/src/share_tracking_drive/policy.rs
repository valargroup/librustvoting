//! Pacing and failure policy for one share-tracking run.

use std::time::Duration;

use crate::share_policy::ShareTimingPolicy;

/// How the tracking driver paces passes and reacts to a failing one.
///
/// The driver never invents a cadence. Between two successful passes it waits
/// exactly the delay the pass itself computed
/// ([`ShareTrackingReport::next_delay_seconds`](crate::share_tracking::ShareTrackingReport::next_delay_seconds)),
/// derived from the durable share rows under `timing`. This policy governs
/// only what the pass cannot decide for itself: what to do when one fails, and
/// when to give up.
///
/// Every wait, whichever of the two produced it, is shortened to the time left
/// before the round's vote end. The pass computes its delay from share rows
/// alone and does not know that boundary, so without the cap a wait could span
/// it and a run would sit on a share it can no longer act on.
#[derive(Clone, Debug)]
pub struct ShareTrackingDrivePolicy {
    /// Thresholds the pass uses for polling, retry, and cutoff decisions.
    pub timing: ShareTimingPolicy,

    /// Wait before retrying after a pass returned an error.
    ///
    /// A failed pass computes no next delay, so this is the only cadence the
    /// driver supplies itself. Helper trouble is usually transient; the wait
    /// ends early on cancellation or an operation-epoch change.
    pub failure_retry: Duration,

    /// Consecutive failed passes before the run stops with
    /// [`ShareTrackingQuiescence::Failing`](super::ShareTrackingQuiescence).
    ///
    /// A successful pass resets the count.
    ///
    /// This is a pathology guard, not the normal stop: **vote end is what ends
    /// a healthy run**, and a share that misses it is a share that did not
    /// count. Helper outages are the ordinary reason a pass fails, and they
    /// outlast a handful of retries, so a small budget here would abandon a
    /// round's shares over a transient fault and leave nothing to restart it —
    /// the host starts runs on lifecycle events, not on a timer. The default
    /// keeps retrying for about an hour at `failure_retry`, long enough to
    /// ride out an outage and short enough that a permanently misconfigured
    /// fleet is eventually reported rather than polled for the round's life.
    pub max_consecutive_failures: u32,

    /// Passes before the run stops with
    /// [`ShareTrackingQuiescence::PassBudgetExhausted`](super::ShareTrackingQuiescence),
    /// or `None` for no pass-count bound.
    ///
    /// `None` by default, because **vote end is the boundary of a healthy
    /// run** and a pass count is not a duration. Passes are paced by the share
    /// rows: a round whose shares are all ready but unconfirmed produces one
    /// pass per `ready_poll_interval_seconds`, so any budget a host might
    /// think generous — a thousand passes is under five hours at the default
    /// interval — expires deep inside a multi-day voting window, and the run
    /// would stop confirming and recovering shares that still had days to
    /// settle. Nothing restarts it: a host starts runs on lifecycle events,
    /// not on a timer.
    ///
    /// What actually bounds a run is vote end, confirmation, cancellation, and
    /// [`max_consecutive_failures`](Self::max_consecutive_failures). Set a
    /// budget when none of those apply — a round whose host reports no vote
    /// end has no time boundary at all — or to bound a run for a test.
    pub max_passes: Option<u32>,
}

impl Default for ShareTrackingDrivePolicy {
    fn default() -> Self {
        Self {
            timing: ShareTimingPolicy::default(),
            failure_retry: Duration::from_secs(15),
            max_consecutive_failures: 240,
            max_passes: None,
        }
    }
}
