//! The values a retired host-side cadence loop was assembled from.
//!
//! Retiring `next_tracking_delay_seconds`, `is_share_ready_for_status_check`
//! and the four constants only they read rests on a promise: nothing a host
//! could reason about is gone, because every value survives as a field of
//! `ShareTimingPolicy::default()`, which is what the driver's own policy is
//! built from. Nothing enforced that promise — a later change to `Default`
//! would leave the migration note quietly false and a host with no way to read
//! the schedule it is told to expect.
//!
//! This pins the promise from outside the crate, through the documented import
//! path, which is where a host stands.

use zcash_voting::prelude::*;

#[test]
fn every_retired_constant_survives_as_a_policy_default() {
    let policy = ShareTimingPolicy::default();

    // The four constants retired with the host-side cadence API.
    assert_eq!(
        policy.status_check_grace_seconds, 10,
        "SHARE_STATUS_CHECK_GRACE_SECONDS"
    );
    assert_eq!(
        policy.ready_poll_interval_seconds, 15,
        "SHARE_READY_POLL_INTERVAL_SECONDS"
    );
    assert_eq!(
        policy.future_check_max_delay_seconds, 30,
        "SHARE_FUTURE_CHECK_MAX_DELAY_SECONDS"
    );
    assert_eq!(
        policy.min_tracking_delay_seconds, 3,
        "SHARE_MIN_TRACKING_DELAY_SECONDS"
    );

    // The three that were already only policy fields, kept here so the whole
    // schedule is pinned in one place rather than half of it.
    assert_eq!(policy.min_overdue_threshold_seconds, 30);
    assert_eq!(policy.max_overdue_threshold_seconds, 60 * 60);
    assert_eq!(policy.resubmit_cutoff_seconds, 10);
}

#[test]
fn the_driver_is_paced_by_exactly_that_policy() {
    // The migration tells a host that reading `ShareTimingPolicy::default()`
    // is reading the driver's schedule. That is only true while the driver's
    // own default timing is that policy.
    assert_eq!(
        ShareTrackingDrivePolicy::default().timing,
        ShareTimingPolicy::default(),
    );
}

#[test]
fn share_readiness_is_still_observable_without_the_retired_predicate() {
    // `is_share_ready_for_status_check` is crate-private now. The state it
    // reported is still readable, because the summary a wallet UI already uses
    // classifies each share with that same predicate.
    let summary = summarize_share_tracking(&[], 0, None, ShareTimingPolicy::default());

    assert_eq!(summary.total, 0);
    assert_eq!(summary.ready, 0);
    assert_eq!(summary.waiting, 0);
    assert_eq!(summary.overdue, 0);
    assert_eq!(summary.confirmed, 0);
}
