//! The values a retired host-side cadence loop was assembled from.
//!
//! Retiring `next_tracking_delay_seconds`, `is_share_ready_for_status_check`
//! and the four constants only they read rests on a promise: nothing a host
//! could reason about is gone. Each retired value survives as a field of
//! `ShareTimingPolicy::default()`, which is what the driver's own policy is
//! built from, and the retired predicate survives as
//! `share_tracking_flags(..).ready_for_status_check`.
//!
//! Nothing enforced that promise. A later change to `Default` would leave the
//! migration note quietly false, and — as review found — a replacement that
//! merely looks equivalent is worse than a missing one: `summarize_share_tracking`
//! classifies by precedence, so it calls an overdue share `overdue` and leaves
//! `ready` at zero for exactly the shares the retired predicate called ready.
//!
//! This pins the promise from outside the crate, through the documented import
//! path, which is where a host stands.

use zcash_voting::prelude::*;
use zcash_voting::share_policy::summarize_share_tracking;
use zcash_voting::ShareDelegationRecord;

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

/// An unconfirmed share whose helper submission happened `age` seconds ago.
fn share_submitted(age_seconds: u64, now_seconds: u64) -> ShareDelegationRecord {
    ShareDelegationRecord {
        round_id: "round".to_string(),
        bundle_index: 0,
        proposal_id: 1,
        share_index: 0,
        sent_to_urls: Vec::new(),
        ambiguous_urls: Vec::new(),
        attempting_urls: Vec::new(),
        target_count: 1,
        nullifier: vec![0u8; 32],
        confirmed: false,
        submit_at: 0,
        created_at: now_seconds.saturating_sub(age_seconds),
    }
}

#[test]
fn share_readiness_is_still_observable_without_the_retired_predicate() {
    // `is_share_ready_for_status_check` is crate-private now. The state it
    // reported is still readable through `share_tracking_flags`, whose
    // `ready_for_status_check` is that predicate exactly: unconfirmed, and far
    // enough past its base time for a helper to have processed it.
    let policy = ShareTimingPolicy::default();
    let now = 100_000;
    let grace = policy.status_check_grace_seconds;

    let too_young = share_submitted(grace - 1, now);
    assert!(
        !share_tracking_flags(&too_young, now, None, policy).ready_for_status_check,
        "inside the status-check grace",
    );

    let just_ready = share_submitted(grace, now);
    assert!(
        share_tracking_flags(&just_ready, now, None, policy).ready_for_status_check,
        "the grace boundary is inclusive, as the retired predicate was",
    );

    let mut confirmed = share_submitted(grace * 10, now);
    confirmed.confirmed = true;
    assert!(
        !share_tracking_flags(&confirmed, now, None, policy).ready_for_status_check,
        "a confirmed share is never ready for a status check",
    );
}

#[test]
fn readiness_survives_a_share_also_being_overdue() {
    // The distinction that makes `share_tracking_flags` the right migration
    // target rather than `summarize_share_tracking`: that summary classifies
    // by precedence, counting an overdue share as `overdue` and leaving
    // `ready` at zero, while the retired predicate called the same share
    // ready. Pointing a host at the summary would have quietly changed the
    // answer for exactly the shares it most needs to poll.
    let policy = ShareTimingPolicy::default();
    let now = 100_000;
    let vote_end = now + 60;
    let long_overdue = share_submitted(policy.max_overdue_threshold_seconds * 2, now);

    let flags = share_tracking_flags(&long_overdue, now, Some(vote_end), policy);
    assert!(flags.overdue_for_retry, "the share is overdue");
    assert!(
        flags.ready_for_status_check,
        "and still ready for a status check, which is what the retired \
         predicate reported and the summary's `ready` count does not",
    );

    let summary = summarize_share_tracking(&[long_overdue], now, Some(vote_end), policy);
    assert_eq!(summary.overdue, 1);
    assert_eq!(
        summary.ready, 0,
        "the summary is a precedence classification, not the predicate",
    );
}
