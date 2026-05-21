use serde::{Deserialize, Serialize};

use crate::{
    share_policy::{
        is_share_ready_for_status_check, next_tracking_delay_seconds,
        scheduled_share_submit_at_from_entropy, share_submit_at_random_bytes_required,
        should_resubmit_share, summarize_share_tracking, ShareTimingPolicy, ShareTrackingSummary,
        SHARE_SUBMIT_AT_RANDOM_BYTES,
    },
    types::{ShareDelegationRecord, VotingError},
};

/// Fraction of the voting window reserved for immediate share submission.
pub const SHARE_LAST_MOMENT_BUFFER_NUMERATOR: u64 = 2;
/// Denominator for `SHARE_LAST_MOMENT_BUFFER_NUMERATOR`.
pub const SHARE_LAST_MOMENT_BUFFER_DENOMINATOR: u64 = 5;
/// Maximum seconds reserved for the immediate share-submission window.
pub const SHARE_LAST_MOMENT_BUFFER_MAX_SECONDS: u64 = 21_600;

/// Crate-owned share submission mode for a voting round at a point in time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareModePlan {
    /// True when wallets should build only the single immediate share.
    pub single_share: bool,
    /// Last-moment buffer derived from the round timing, when timing is valid.
    pub last_moment_buffer_seconds: Option<u64>,
    /// Seconds remaining in the delayed-share submission window.
    pub submit_at_delay_seconds: Option<u64>,
}

/// Stable identity for a tracked share delegation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareWorkflowKey {
    pub bundle_index: u32,
    pub proposal_id: u32,
    pub share_index: u32,
}

/// Crate-owned share tracking decision for the wallet polling loop.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareTrackingPlan {
    pub summary: ShareTrackingSummary,
    /// Unconfirmed shares ready for status checks. This includes overdue shares.
    pub ready_share_keys: Vec<ShareWorkflowKey>,
    /// Unconfirmed shares that should be resubmitted if status checks fail.
    pub overdue_share_keys: Vec<ShareWorkflowKey>,
    /// Delay before the next tracking pass, or `None` when tracking is complete.
    pub next_delay_seconds: Option<u64>,
}

/// Return the share mode for the current round timing.
///
/// Invalid round timing is not treated as last moment. Callers then fall back to
/// immediate `submit_at = 0` for every share.
pub fn plan_share_mode(
    now_seconds: u64,
    ceremony_start_seconds: u64,
    vote_end_time_seconds: u64,
) -> ShareModePlan {
    let Some(last_moment_buffer_seconds) =
        last_moment_buffer_seconds(ceremony_start_seconds, vote_end_time_seconds)
    else {
        return ShareModePlan {
            single_share: false,
            last_moment_buffer_seconds: None,
            submit_at_delay_seconds: None,
        };
    };

    let deadline = vote_end_time_seconds.saturating_sub(last_moment_buffer_seconds);
    if now_seconds >= deadline {
        return ShareModePlan {
            single_share: true,
            last_moment_buffer_seconds: Some(last_moment_buffer_seconds),
            submit_at_delay_seconds: None,
        };
    }

    ShareModePlan {
        single_share: false,
        last_moment_buffer_seconds: Some(last_moment_buffer_seconds),
        submit_at_delay_seconds: Some(deadline - now_seconds),
    }
}

/// Plan per-share helper submission times using caller-provided entropy.
///
/// This intentionally does not select helper targets. Wallets keep their
/// current HTTP fanout, while the crate owns delayed `submit_at` policy.
pub fn plan_share_submit_times(
    share_count: usize,
    now_seconds: u64,
    vote_end_time_seconds: u64,
    mode: ShareModePlan,
    submit_at_random_bytes: &[u8],
) -> Result<Vec<u64>, VotingError> {
    if share_count == 0 {
        return Ok(Vec::new());
    }

    let bytes_per_share = share_submit_at_random_bytes_required(
        now_seconds,
        vote_end_time_seconds,
        mode.last_moment_buffer_seconds,
        mode.single_share,
    );
    let bytes_needed =
        bytes_per_share
            .checked_mul(share_count)
            .ok_or_else(|| VotingError::InvalidInput {
                message: "submit_at_random_bytes requirement overflows usize".to_string(),
            })?;
    if submit_at_random_bytes.len() < bytes_needed {
        return Err(VotingError::InvalidInput {
            message: format!("submit_at_random_bytes must contain at least {bytes_needed} bytes"),
        });
    }

    let mut submit_times = Vec::with_capacity(share_count);
    for share_index in 0..share_count {
        let start = share_index * bytes_per_share;
        let end = start + bytes_per_share;
        submit_times.push(scheduled_share_submit_at_from_entropy(
            now_seconds,
            vote_end_time_seconds,
            mode.last_moment_buffer_seconds,
            mode.single_share,
            &submit_at_random_bytes[start..end],
        )?);
    }
    Ok(submit_times)
}

/// Return how many random bytes the workflow consumes per share at most.
///
/// SDKs may draw this many bytes per share unconditionally and pass the result
/// into `plan_share_submit_times`; immediate paths ignore the bytes.
pub fn share_submit_time_entropy_bytes_per_share() -> usize {
    SHARE_SUBMIT_AT_RANDOM_BYTES
}

/// Plan the wallet's next share tracking pass.
///
/// Wallets still own helper status HTTP calls and resubmission. This only
/// identifies which shares are ready, which are overdue, and when to poll next.
pub fn plan_share_tracking(
    shares: &[ShareDelegationRecord],
    now_seconds: u64,
    vote_end_time_seconds: u64,
) -> ShareTrackingPlan {
    let policy = ShareTimingPolicy::default();
    let summary =
        summarize_share_tracking(shares, now_seconds, Some(vote_end_time_seconds), policy);
    let mut ready_share_keys = Vec::new();
    let mut overdue_share_keys = Vec::new();

    for share in shares {
        if is_share_ready_for_status_check(share, now_seconds, policy) {
            ready_share_keys.push(share_workflow_key(share));
        }
        if should_resubmit_share(share, now_seconds, vote_end_time_seconds, policy) {
            overdue_share_keys.push(share_workflow_key(share));
        }
    }

    ShareTrackingPlan {
        summary,
        ready_share_keys,
        overdue_share_keys,
        next_delay_seconds: next_tracking_delay_seconds(shares, now_seconds, policy),
    }
}

fn last_moment_buffer_seconds(
    ceremony_start_seconds: u64,
    vote_end_time_seconds: u64,
) -> Option<u64> {
    if vote_end_time_seconds <= ceremony_start_seconds {
        return None;
    }

    let duration = vote_end_time_seconds - ceremony_start_seconds;
    Some(
        duration
            .saturating_mul(SHARE_LAST_MOMENT_BUFFER_NUMERATOR)
            .checked_div(SHARE_LAST_MOMENT_BUFFER_DENOMINATOR)
            .unwrap_or(0)
            .min(SHARE_LAST_MOMENT_BUFFER_MAX_SECONDS),
    )
}

fn share_workflow_key(share: &ShareDelegationRecord) -> ShareWorkflowKey {
    ShareWorkflowKey {
        bundle_index: share.bundle_index,
        proposal_id: share.proposal_id,
        share_index: share.share_index,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn share(submit_at: u64, created_at: u64, confirmed: bool) -> ShareDelegationRecord {
        ShareDelegationRecord {
            round_id: "round".to_string(),
            bundle_index: 7,
            proposal_id: 42,
            share_index: 3,
            sent_to_urls: vec!["https://helper.example.com".to_string()],
            nullifier: vec![9; 32],
            confirmed,
            submit_at,
            created_at,
        }
    }

    fn random_bytes(samples: &[u64]) -> Vec<u8> {
        samples
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect()
    }

    #[test]
    fn share_mode_plans_delayed_window_before_last_moment() {
        let plan = plan_share_mode(1_000, 0, 2_000);

        assert_eq!(
            plan,
            ShareModePlan {
                single_share: false,
                last_moment_buffer_seconds: Some(800),
                submit_at_delay_seconds: Some(200),
            }
        );
    }

    #[test]
    fn share_mode_uses_single_share_inside_last_moment_window() {
        let plan = plan_share_mode(1_201, 0, 2_000);

        assert_eq!(
            plan,
            ShareModePlan {
                single_share: true,
                last_moment_buffer_seconds: Some(800),
                submit_at_delay_seconds: None,
            }
        );
    }

    #[test]
    fn invalid_round_timing_falls_back_to_immediate_multi_share() {
        let mode = plan_share_mode(1_000, 2_000, 2_000);
        let submit_times = plan_share_submit_times(2, 1_000, 2_000, mode, &[]).unwrap();

        assert!(!mode.single_share);
        assert_eq!(mode.last_moment_buffer_seconds, None);
        assert_eq!(submit_times, vec![0, 0]);
    }

    #[test]
    fn submit_times_use_independent_entropy_before_last_moment() {
        let mode = plan_share_mode(1_000, 0, 2_000);
        let submit_times =
            plan_share_submit_times(2, 1_000, 2_000, mode, &random_bytes(&[0, u64::MAX])).unwrap();

        assert_eq!(submit_times, vec![1_000, 1_199]);
    }

    #[test]
    fn submit_times_are_immediate_for_single_share_mode() {
        let mode = plan_share_mode(1_300, 0, 2_000);
        let submit_times = plan_share_submit_times(2, 1_300, 2_000, mode, &[]).unwrap();

        assert_eq!(submit_times, vec![0, 0]);
    }

    #[test]
    fn tracking_plan_returns_ready_overdue_and_next_delay() {
        let shares = vec![
            share(1_000, 900, false),
            ShareDelegationRecord {
                share_index: 4,
                submit_at: 1_300,
                ..share(0, 900, false)
            },
            ShareDelegationRecord {
                share_index: 5,
                confirmed: true,
                ..share(0, 900, false)
            },
        ];
        let plan = plan_share_tracking(&shares, 1_100, 1_400);

        assert_eq!(plan.summary.total, 3);
        assert_eq!(plan.summary.confirmed, 1);
        assert_eq!(plan.summary.overdue, 1);
        assert_eq!(
            plan.ready_share_keys,
            vec![ShareWorkflowKey {
                bundle_index: 7,
                proposal_id: 42,
                share_index: 3,
            }]
        );
        assert_eq!(plan.ready_share_keys, plan.overdue_share_keys);
        assert_eq!(plan.next_delay_seconds, Some(30));
    }

    #[test]
    fn tracking_plan_stops_when_all_confirmed() {
        let plan = plan_share_tracking(&[share(0, 900, true)], 1_100, 2_000);

        assert_eq!(plan.next_delay_seconds, None);
        assert!(plan.ready_share_keys.is_empty());
        assert!(plan.overdue_share_keys.is_empty());
    }
}
