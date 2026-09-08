use crate::types::ShareDelegationRecord;

use super::{ShareTimingPolicy, ShareTrackingSummary};

/// Seconds to wait after helper submission time before polling share status.
pub const SHARE_STATUS_CHECK_GRACE_SECONDS: u64 = 10;
/// Minimum seconds before an unconfirmed share is considered overdue.
pub const SHARE_MIN_OVERDUE_THRESHOLD_SECONDS: u64 = 30;
/// Maximum seconds before an unconfirmed share is considered overdue.
pub const SHARE_MAX_OVERDUE_THRESHOLD_SECONDS: u64 = 60 * 60;
/// Seconds near the vote end when resubmission should stop.
pub const SHARE_RESUBMIT_CUTOFF_SECONDS: u64 = 10;
/// Seconds between polls when all remaining shares are ready but unconfirmed.
pub const SHARE_READY_POLL_INTERVAL_SECONDS: u64 = 15;
/// Maximum seconds to wait for a future share to become ready.
pub const SHARE_FUTURE_CHECK_MAX_DELAY_SECONDS: u64 = 30;
/// Minimum seconds to sleep before the next tracking poll.
pub const SHARE_MIN_TRACKING_DELAY_SECONDS: u64 = 3;
/// Numerator for the last-moment share window fraction.
pub const LAST_MOMENT_BUFFER_FRACTION_NUMERATOR: u64 = 2;
/// Denominator for the last-moment share window fraction.
pub const LAST_MOMENT_BUFFER_FRACTION_DENOMINATOR: u64 = 5;
/// Maximum last-moment share window, in seconds.
pub const LAST_MOMENT_BUFFER_MAX_SECONDS: u64 = 6 * 60 * 60;

/// Return the last-moment buffer for a voting round.
///
/// The buffer is 40% of the round duration from ceremony start to vote end,
/// capped at six hours. The calculation rounds up to whole seconds so callers
/// do not schedule delayed helper shares inside the intended last-moment
/// window. Invalid or zero-length timing returns `None`.
pub fn last_moment_buffer_seconds(
    ceremony_start_seconds: u64,
    vote_end_time_seconds: u64,
) -> Option<u64> {
    let duration = vote_end_time_seconds.checked_sub(ceremony_start_seconds)?;
    if duration == 0 {
        return None;
    }
    let numerator = u128::from(duration) * u128::from(LAST_MOMENT_BUFFER_FRACTION_NUMERATOR);
    let denominator = u128::from(LAST_MOMENT_BUFFER_FRACTION_DENOMINATOR);
    let buffer = (numerator + denominator - 1) / denominator;
    let capped = buffer.min(u128::from(LAST_MOMENT_BUFFER_MAX_SECONDS));
    Some(capped as u64)
}

/// Return the Unix-second boundary where last-moment mode starts.
///
/// Returns `None` when the round timing cannot produce a last-moment buffer.
pub fn last_moment_deadline_seconds(
    ceremony_start_seconds: u64,
    vote_end_time_seconds: u64,
) -> Option<u64> {
    let buffer = last_moment_buffer_seconds(ceremony_start_seconds, vote_end_time_seconds)?;
    Some(vote_end_time_seconds.saturating_sub(buffer))
}

/// Return true when `now_seconds` falls inside the active round's last-moment window.
///
/// Invalid or zero-length round timing is treated as not last moment.
pub fn is_last_moment(
    now_seconds: u64,
    ceremony_start_seconds: u64,
    vote_end_time_seconds: u64,
) -> bool {
    last_moment_deadline_seconds(ceremony_start_seconds, vote_end_time_seconds)
        .is_some_and(|deadline| now_seconds >= deadline && now_seconds < vote_end_time_seconds)
}

/// Return the time recovery should use as the share's base time.
///
/// Delayed shares use `submit_at`; immediate shares use `created_at`.
pub fn share_recovery_base_time(share: &ShareDelegationRecord) -> u64 {
    if share.submit_at > 0 {
        share.submit_at
    } else {
        share.created_at
    }
}

/// Return true once a helper has had enough time to process this share.
pub fn is_share_ready_for_status_check(
    share: &ShareDelegationRecord,
    now_seconds: u64,
    policy: ShareTimingPolicy,
) -> bool {
    if share.confirmed {
        return false;
    }
    now_seconds >= share_recovery_base_time(share).saturating_add(policy.status_check_grace_seconds)
}

/// Return the bounded overdue threshold for a share.
///
/// The threshold is one quarter of the remaining vote window from the share's
/// base time, bounded by the policy's minimum and maximum seconds.
pub fn overdue_threshold_seconds(
    share: &ShareDelegationRecord,
    vote_end_time_seconds: u64,
    policy: ShareTimingPolicy,
) -> u64 {
    let base_time = share_recovery_base_time(share);
    let remaining_window = vote_end_time_seconds.saturating_sub(base_time);
    let threshold = remaining_window / 4;
    let max_threshold = policy
        .max_overdue_threshold_seconds
        .max(policy.min_overdue_threshold_seconds);
    threshold
        .max(policy.min_overdue_threshold_seconds)
        .min(max_threshold)
}

/// Return true when a pending share should be retried immediately.
pub fn should_resubmit_share(
    share: &ShareDelegationRecord,
    now_seconds: u64,
    vote_end_time_seconds: u64,
    policy: ShareTimingPolicy,
) -> bool {
    if share.confirmed {
        return false;
    }
    let base_time = share_recovery_base_time(share);
    let retry_at = base_time.saturating_add(overdue_threshold_seconds(
        share,
        vote_end_time_seconds,
        policy,
    ));
    now_seconds >= retry_at
        && is_share_resubmission_window_open(now_seconds, vote_end_time_seconds, policy)
}

/// Return true while recovery POSTs are allowed before the vote-end cutoff.
pub(crate) fn is_share_resubmission_window_open(
    now_seconds: u64,
    vote_end_time_seconds: u64,
    policy: ShareTimingPolicy,
) -> bool {
    vote_end_time_seconds > now_seconds.saturating_add(policy.resubmit_cutoff_seconds)
}

/// Return the last Unix second at which recovery POSTs are still permitted.
///
/// This is the inverse of [`is_share_resubmission_window_open`]: the returned
/// second is open and the one after it is not. `None` when the cutoff already
/// covers the whole round, so no second in it is open at all.
pub(crate) fn last_open_resubmission_second(
    vote_end_time_seconds: u64,
    policy: ShareTimingPolicy,
) -> Option<u64> {
    vote_end_time_seconds
        .checked_sub(policy.resubmit_cutoff_seconds)?
        .checked_sub(1)
}

/// What a tracking pass can still accomplish for one round, and until when.
///
/// A pass does two different things and they stop being possible at two
/// different times: recovery POSTs shut a `resubmit_cutoff_seconds` before the
/// vote end, and a confirmation is only worth seeking until the vote end
/// itself. Four separate questions used to be derived from those two facts at
/// four call sites — whether this share may be resubmitted, whether it is
/// beyond help, when to wake next, and whether a pass has reached its cutoff —
/// each from its own reading of the clock. They must agree, so they are
/// answered here.
///
/// A round with no vote-end time has no boundary: nothing ever shuts.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RoundWindow {
    vote_end_time_seconds: Option<u64>,
    policy: ShareTimingPolicy,
}

impl RoundWindow {
    pub(crate) fn new(vote_end_time_seconds: Option<u64>, policy: ShareTimingPolicy) -> Self {
        Self {
            vote_end_time_seconds,
            policy,
        }
    }

    /// Whether a recovery POST issued at `now_seconds` is permitted.
    pub(crate) fn can_resubmit_at(&self, now_seconds: u64) -> bool {
        self.vote_end_time_seconds.is_none_or(|vote_end| {
            is_share_resubmission_window_open(now_seconds, vote_end, self.policy)
        })
    }

    /// Whether the round is over, so no POST on any path can still produce a
    /// confirmation that counts.
    ///
    /// Deliberately the vote end and not the resubmission cutoff. The cutoff
    /// governs recovery only: initial delivery does not consult it, so between
    /// the cutoff and the vote end a share nothing holds may still be placed by
    /// an outstanding initial fan-out that resumes. Treating the cutoff as the
    /// end of all hope would tell a caller to stop tracking a share that was
    /// about to be delivered.
    pub(crate) fn closed_at(&self, now_seconds: u64) -> bool {
        self.vote_end_time_seconds
            .is_some_and(|vote_end| now_seconds >= vote_end)
    }

    /// The latest second a pass may **begin** and still reach its recovery
    /// phase with the window open.
    ///
    /// A pass walks helper status before it decides anything about recovery,
    /// and re-reads the clock in between, so waking at the last open second
    /// loses the retry: the walk spends it. `reserve_seconds` is what the
    /// caller spends before that decision, and it is subtracted here.
    ///
    /// `None` when no second is open at all, or when the reserve does not fit
    /// inside the open window — a pass cannot then be scheduled to recover,
    /// however early it starts.
    ///
    /// The reserve is one share's status budget. A pass over many shares
    /// spends that budget per share, so a later share can still find the
    /// window shut by the time its own recovery is considered. No wake time
    /// prevents that; it is bounded by how many shares one pass carries, and
    /// each share's own window check still refuses the POST rather than making
    /// a late one.
    pub(crate) fn latest_start_that_can_resubmit(&self, reserve_seconds: u64) -> Option<u64> {
        last_open_resubmission_second(self.vote_end_time_seconds?, self.policy)?
            .checked_sub(reserve_seconds)
    }

    /// The next second after `now_seconds` at which what a pass can do
    /// changes, given a `reserve_seconds` status budget.
    ///
    /// The recovery boundary while it is still ahead, and the vote end once it
    /// is behind. `None` when the round has no vote-end time and so no
    /// boundary to respect.
    pub(crate) fn next_boundary_after(
        &self,
        now_seconds: u64,
        reserve_seconds: u64,
    ) -> Option<u64> {
        let vote_end = self.vote_end_time_seconds?;
        Some(
            self.latest_start_that_can_resubmit(reserve_seconds)
                .filter(|second| *second > now_seconds)
                .unwrap_or(vote_end),
        )
    }
}

/// Return the next delay after a share-status polling pass completes.
///
/// Two clocks compete, and the delay is whichever comes first:
///
/// - a share already past its status-check grace time is polled again after
///   `ready_poll_interval_seconds`, so callers neither tight-loop on it nor
///   leave it unpolled;
/// - a share still before its grace time is waited for until it arrives,
///   capped by `future_check_max_delay_seconds`.
///
/// Taking the sooner of the two matters when the two kinds coexist: a ready
/// share must not queue behind an unrelated share whose check is further out.
/// The returned delay is always at least `min_tracking_delay_seconds`, and
/// `None` means no unconfirmed share remains — the signal to stop tracking.
pub fn next_tracking_delay_seconds(
    shares: &[ShareDelegationRecord],
    now_seconds: u64,
    policy: ShareTimingPolicy,
) -> Option<u64> {
    let mut next_second: Option<u64> = None;
    let mut has_unconfirmed = false;
    let mut has_ready = false;

    for share in shares.iter().filter(|share| !share.confirmed) {
        has_unconfirmed = true;
        let base_time = share_recovery_base_time(share);
        let check_at = base_time.saturating_add(policy.status_check_grace_seconds);
        if check_at > now_seconds {
            next_second = min_second(next_second, check_at);
        } else {
            has_ready = true;
        }
    }

    if !has_unconfirmed {
        return None;
    }

    let future_delay = next_second.map(|next| {
        next.saturating_sub(now_seconds)
            .min(policy.future_check_max_delay_seconds)
    });
    let ready_delay = has_ready.then_some(policy.ready_poll_interval_seconds);
    let delay_seconds = match (ready_delay, future_delay) {
        (Some(ready), Some(future)) => ready.min(future),
        (Some(ready), None) => ready,
        (None, Some(future)) => future,
        // Every unconfirmed share was counted as ready or future, so one of
        // the two is always set once `has_unconfirmed` holds.
        (None, None) => policy.ready_poll_interval_seconds,
    };

    Some(delay_seconds.max(policy.min_tracking_delay_seconds))
}

/// Summarize share tracking state using the same precedence as wallet UIs.
pub fn summarize_share_tracking(
    shares: &[ShareDelegationRecord],
    now_seconds: u64,
    vote_end_time_seconds: Option<u64>,
    policy: ShareTimingPolicy,
) -> ShareTrackingSummary {
    let mut summary = ShareTrackingSummary {
        total: shares.len() as u64,
        confirmed: 0,
        waiting: 0,
        ready: 0,
        overdue: 0,
    };

    for share in shares {
        if share.confirmed {
            summary.confirmed += 1;
        } else if match vote_end_time_seconds {
            Some(vote_end_time_seconds) => {
                should_resubmit_share(share, now_seconds, vote_end_time_seconds, policy)
            }
            None => false,
        } {
            summary.overdue += 1;
        } else if is_share_ready_for_status_check(share, now_seconds, policy) {
            summary.ready += 1;
        } else {
            summary.waiting += 1;
        }
    }

    summary
}

fn min_second(current: Option<u64>, candidate: u64) -> Option<u64> {
    match current {
        Some(current) if current <= candidate => Some(current),
        _ => Some(candidate),
    }
}
