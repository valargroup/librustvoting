use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::{
    types::{ShareDelegationRecord, VotingError},
    wire::BoundedU32,
};

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
/// Random bytes needed to sample an initial delayed share submission time.
pub const SHARE_SUBMIT_AT_RANDOM_BYTES: usize = 8;
/// Maximum randomized delay before an initial helper share submission.
pub const SHARE_SUBMIT_AT_MAX_DELAY_SECONDS: u64 = 100 * 60 * 60;
/// Number of encrypted shares in one complete vote commitment.
pub const VOTE_COMMITMENT_SHARE_COUNT: usize = 16;
/// Normal maximum number of one commitment's shares sent to one helper.
pub const SHARE_HELPER_MAX_SHARES_PER_SERVER: usize = VOTE_COMMITMENT_SHARE_COUNT / 2;
/// Initial helper readiness window before slower transports keep racing.
pub const SHARE_HELPER_PREFLIGHT_SOFT_TIMEOUT_MILLISECONDS: u64 = 2_000;
/// Absolute deadline for the helper readiness race.
pub const SHARE_HELPER_PREFLIGHT_HARD_TIMEOUT_MILLISECONDS: u64 = 30_000;
/// Numerator for the last-moment share window fraction.
pub const LAST_MOMENT_BUFFER_FRACTION_NUMERATOR: u64 = 2;
/// Denominator for the last-moment share window fraction.
pub const LAST_MOMENT_BUFFER_FRACTION_DENOMINATOR: u64 = 5;
/// Maximum last-moment share window, in seconds.
pub const LAST_MOMENT_BUFFER_MAX_SECONDS: u64 = 6 * 60 * 60;

/// Pure timing knobs for helper-share scheduling and recovery.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareTimingPolicy {
    pub status_check_grace_seconds: u64,
    pub min_overdue_threshold_seconds: u64,
    pub max_overdue_threshold_seconds: u64,
    pub resubmit_cutoff_seconds: u64,
    pub ready_poll_interval_seconds: u64,
    pub future_check_max_delay_seconds: u64,
    pub min_tracking_delay_seconds: u64,
}

impl Default for ShareTimingPolicy {
    fn default() -> Self {
        Self {
            status_check_grace_seconds: SHARE_STATUS_CHECK_GRACE_SECONDS,
            min_overdue_threshold_seconds: SHARE_MIN_OVERDUE_THRESHOLD_SECONDS,
            max_overdue_threshold_seconds: SHARE_MAX_OVERDUE_THRESHOLD_SECONDS,
            resubmit_cutoff_seconds: SHARE_RESUBMIT_CUTOFF_SECONDS,
            ready_poll_interval_seconds: SHARE_READY_POLL_INTERVAL_SECONDS,
            future_check_max_delay_seconds: SHARE_FUTURE_CHECK_MAX_DELAY_SECONDS,
            min_tracking_delay_seconds: SHARE_MIN_TRACKING_DELAY_SECONDS,
        }
    }
}

/// Planned helper-share submission values that SDKs can apply to payloads.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareSubmissionPlan {
    /// Unix seconds when helpers should submit the share, or 0 for immediate.
    pub submit_at: u64,
    /// Number of helpers each share should reach.
    pub target_count: u32,
    /// Helper targets selected for initial share submission.
    pub target_servers: Vec<String>,
}

/// Shared helper selection values for one complete vote commitment.
///
/// Clients own the HTTP requests, but should use these values to keep probe
/// timing and privacy limits consistent. Start every readiness probe at once,
/// inspect the responses available after `preflight_soft_timeout_milliseconds`,
/// and keep waiting until `target_count` helpers are ready or the hard timeout
/// expires. Pass ready helpers to
/// [`ranked_share_submission_server_candidates`] in response order, followed by
/// the remaining configured helpers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareServerSelectionPolicy {
    /// Number of helpers each share should reach.
    pub target_count: u32,
    /// Normal maximum number of one commitment's shares sent to one helper.
    pub max_shares_per_server: u32,
    /// Time to collect the first group of fast helper responses.
    pub preflight_soft_timeout_milliseconds: u64,
    /// Absolute deadline for collecting enough ready helper responses.
    pub preflight_hard_timeout_milliseconds: u64,
}

/// Random byte counts needed to plan one or more share submissions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareSubmissionRandomBytesRequired {
    /// Bytes needed for independent delayed `submit_at` samples.
    pub submit_at_random_bytes: usize,
    /// Bytes needed for independent helper-order shuffles.
    pub server_random_bytes: usize,
}

/// Counts shares by their recovery status.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareTrackingSummary {
    pub total: u64,
    pub confirmed: u64,
    pub waiting: u64,
    pub ready: u64,
    pub overdue: u64,
}

impl ShareTrackingSummary {
    pub fn has_shares(&self) -> bool {
        self.total > 0
    }
}

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
        && vote_end_time_seconds > now_seconds.saturating_add(policy.resubmit_cutoff_seconds)
}

/// Return the next delay after a share-status polling pass completes.
///
/// This mirrors the current wallet polling cadence. If any unconfirmed share is
/// still before its status-check grace time, the delay is the soonest future
/// check time capped by `future_check_max_delay_seconds`. If every unconfirmed
/// share is already ready, the delay is `ready_poll_interval_seconds` so callers
/// do not tight-loop on past check times. The returned delay is always at least
/// `min_tracking_delay_seconds`.
pub fn next_tracking_delay_seconds(
    shares: &[ShareDelegationRecord],
    now_seconds: u64,
    policy: ShareTimingPolicy,
) -> Option<u64> {
    let mut next_second: Option<u64> = None;
    let mut has_unconfirmed = false;

    for share in shares.iter().filter(|share| !share.confirmed) {
        has_unconfirmed = true;
        let base_time = share_recovery_base_time(share);
        let check_at = base_time.saturating_add(policy.status_check_grace_seconds);
        if check_at > now_seconds {
            next_second = min_second(next_second, check_at);
        }
    }

    if !has_unconfirmed {
        return None;
    }

    let delay_seconds = match next_second {
        Some(next) => next
            .saturating_sub(now_seconds)
            .min(policy.future_check_max_delay_seconds),
        None => policy.ready_poll_interval_seconds,
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

/// Return the random bytes needed to sample a delayed share submission time.
///
/// `vote_end_time_seconds` is required because callers should only schedule
/// share submission for an active voting session. `last_moment_buffer_seconds`
/// is optional because some round timing data cannot produce a delayed-share
/// window. When the buffer is missing or zero, or when `single_share` is true,
/// no random bytes are needed because the share should be submitted
/// immediately.
pub fn share_submit_at_random_bytes_required(
    now_seconds: u64,
    vote_end_time_seconds: u64,
    last_moment_buffer_seconds: Option<u64>,
    single_share: bool,
) -> usize {
    if delayed_share_window_seconds(
        now_seconds,
        vote_end_time_seconds,
        last_moment_buffer_seconds,
        single_share,
    )
    .is_some()
    {
        SHARE_SUBMIT_AT_RANDOM_BYTES
    } else {
        0
    }
}

/// Plan the delayed helper submission time from a caller-provided random unit.
///
/// The sampled delay is capped at 100 hours and still ends before the round's
/// last-moment window.
///
/// This is useful for deterministic tests and FFI callers that already expose a
/// random sample in the `[0, 1)` range. Production submission paths should use
/// `scheduled_share_submit_at_from_entropy` to keep sampling policy inside the
/// crate.
pub fn scheduled_share_submit_at_from_random_unit(
    now_seconds: u64,
    vote_end_time_seconds: u64,
    last_moment_buffer_seconds: Option<u64>,
    single_share: bool,
    random_unit: f64,
) -> Result<u64, VotingError> {
    let Some(window_seconds) = delayed_share_window_seconds(
        now_seconds,
        vote_end_time_seconds,
        last_moment_buffer_seconds,
        single_share,
    ) else {
        return Ok(0);
    };
    if !random_unit.is_finite() || !(0.0..1.0).contains(&random_unit) {
        return Err(VotingError::InvalidInput {
            message: "random_unit must be finite and in [0, 1)".to_string(),
        });
    }

    let delay_seconds = (random_unit * window_seconds as f64).floor() as u64;
    Ok(now_seconds.saturating_add(delay_seconds))
}

/// Plan the delayed helper submission time using caller-provided entropy.
///
/// The sampled delay is capped at 100 hours and still ends before the round's
/// last-moment window.
///
/// `submit_at_random_bytes` must contain at least
/// `share_submit_at_random_bytes_required(...)` bytes from a cryptographically
/// secure RNG. No bytes are needed when the share should be submitted
/// immediately, and callers may pass 8 bytes unconditionally for simplicity.
pub fn scheduled_share_submit_at_from_entropy(
    now_seconds: u64,
    vote_end_time_seconds: u64,
    last_moment_buffer_seconds: Option<u64>,
    single_share: bool,
    submit_at_random_bytes: &[u8],
) -> Result<u64, VotingError> {
    let Some(window_seconds) = delayed_share_window_seconds(
        now_seconds,
        vote_end_time_seconds,
        last_moment_buffer_seconds,
        single_share,
    ) else {
        return Ok(0);
    };
    if submit_at_random_bytes.len() < SHARE_SUBMIT_AT_RANDOM_BYTES {
        return Err(VotingError::InvalidInput {
            message: format!(
                "submit_at_random_bytes must contain at least {SHARE_SUBMIT_AT_RANDOM_BYTES} bytes"
            ),
        });
    }

    let mut sample_bytes = [0u8; 8];
    sample_bytes.copy_from_slice(&submit_at_random_bytes[..8]);
    let sample = u64::from_le_bytes(sample_bytes);
    let delay_seconds = ((sample as u128 * window_seconds as u128) >> 64) as u64;
    Ok(now_seconds.saturating_add(delay_seconds))
}

/// Return the nonempty randomized delay window for an initial helper share.
///
/// The window ends no later than the round's last-moment boundary and is capped
/// at 100 hours from `now_seconds`.
fn delayed_share_window_seconds(
    now_seconds: u64,
    vote_end_time_seconds: u64,
    last_moment_buffer_seconds: Option<u64>,
    single_share: bool,
) -> Option<u64> {
    if single_share {
        return None;
    }

    let last_moment_buffer_seconds = last_moment_buffer_seconds?;
    if last_moment_buffer_seconds == 0 {
        return None;
    }

    let deadline = vote_end_time_seconds.saturating_sub(last_moment_buffer_seconds);
    if deadline <= now_seconds {
        return None;
    }

    Some((deadline - now_seconds).min(SHARE_SUBMIT_AT_MAX_DELAY_SECONDS))
}

/// Return how many helpers should receive each initial share.
///
/// This is half of the available helpers, rounded up, and 0 when there are no
/// helpers.
pub fn share_submission_target_count(server_count: usize) -> usize {
    if server_count == 0 {
        0
    } else {
        server_count / 2 + server_count % 2
    }
}

/// Return the shared helper probe and privacy policy for a complete commitment.
pub fn share_server_selection_policy(
    server_count: usize,
) -> Result<ShareServerSelectionPolicy, VotingError> {
    let target_count = share_submission_target_count(server_count);
    let max_shares_per_server = SHARE_HELPER_MAX_SHARES_PER_SERVER;

    Ok(ShareServerSelectionPolicy {
        target_count: bounded_u32(target_count, "target_count")?,
        max_shares_per_server: bounded_u32(max_shares_per_server, "max_shares_per_server")?,
        preflight_soft_timeout_milliseconds: SHARE_HELPER_PREFLIGHT_SOFT_TIMEOUT_MILLISECONDS,
        preflight_hard_timeout_milliseconds: SHARE_HELPER_PREFLIGHT_HARD_TIMEOUT_MILLISECONDS,
    })
}

/// Plan each share's complete helper candidate order from fastest to slowest.
///
/// `ranked_server_urls` should contain every configured helper exactly once.
/// Helpers that passed the progressive readiness probe come first in response
/// order; helpers that did not respond before the deadline follow in stable
/// configured order.
///
/// The first `share_submission_target_count(ranked_server_urls.len())` entries
/// of each returned row are the planned targets. For a normal 16-share vote and
/// ten helpers, no helper is a planned target for more than eight shares. Once
/// every remaining helper has reached that cap, the least-used helpers are
/// selected again so liveness wins when the configured or healthy helper set is
/// too small to satisfy the cap. The rest of each row is the failover order;
/// callers should keep trying it until the target count accepts the share.
pub fn ranked_share_submission_server_candidates(
    share_count: usize,
    ranked_server_urls: &[String],
) -> Result<Vec<Vec<String>>, VotingError> {
    ranked_share_submission_server_candidates_with_usage(share_count, ranked_server_urls, &[])
}

/// Plan helper candidates while preserving usage from an interrupted attempt.
///
/// `previously_selected_server_urls` contains one entry for every share a
/// helper already accepted for this same commitment. Repeated URLs are
/// intentional. They seed the per-helper counts so a resumed submission does
/// not forget earlier assignments and exceed the normal privacy cap while
/// uncapped configured helpers remain.
pub fn ranked_share_submission_server_candidates_with_usage(
    share_count: usize,
    ranked_server_urls: &[String],
    previously_selected_server_urls: &[String],
) -> Result<Vec<Vec<String>>, VotingError> {
    if share_count == 0 {
        return Ok(Vec::new());
    }
    require_share_servers(ranked_server_urls)?;

    let target_count = share_submission_target_count(ranked_server_urls.len());
    let max_shares_per_server = SHARE_HELPER_MAX_SHARES_PER_SERVER;
    let mut usage = std::collections::HashMap::<String, usize>::new();
    let configured_servers: HashSet<&str> = ranked_server_urls.iter().map(String::as_str).collect();
    for server_url in previously_selected_server_urls {
        if !configured_servers.contains(server_url.as_str()) {
            return Err(VotingError::InvalidInput {
                message: "previously selected server URLs must be configured".to_string(),
            });
        }
        *usage.entry(server_url.clone()).or_default() += 1;
    }
    let mut candidates = Vec::with_capacity(share_count);

    for _ in 0..share_count {
        let targets = select_targets_with_usage_cap(
            ranked_server_urls,
            target_count,
            max_shares_per_server,
            &mut usage,
        );
        let target_set: HashSet<String> = targets.iter().cloned().collect();
        let mut server_candidates = targets;
        server_candidates.extend(
            ranked_server_urls
                .iter()
                .filter(|server| !target_set.contains(server.as_str()))
                .cloned(),
        );
        candidates.push(server_candidates);
    }

    Ok(candidates)
}

/// Return the number of random bytes needed to shuffle a server list.
///
/// The bytes should come from a cryptographically secure RNG. The crate owns the
/// shuffle policy, while SDKs only provide entropy from their platform RNG.
pub fn share_server_order_random_bytes_required(server_count: usize) -> usize {
    server_count
        .saturating_sub(1)
        .saturating_mul(std::mem::size_of::<u64>())
}

/// Return the random bytes needed to plan `share_count` independent shares.
///
/// Use these totals with `plan_share_submissions`. The returned counts are split
/// by purpose so callers can draw each byte slice from their platform CSPRNG and
/// pass the slices without having to understand the sampling layout.
pub fn share_submission_random_bytes_required(
    share_count: usize,
    server_count: usize,
    now_seconds: u64,
    vote_end_time_seconds: u64,
    last_moment_buffer_seconds: Option<u64>,
    single_share: bool,
) -> ShareSubmissionRandomBytesRequired {
    let submit_at_per_share = share_submit_at_random_bytes_required(
        now_seconds,
        vote_end_time_seconds,
        last_moment_buffer_seconds,
        single_share,
    );
    let server_per_share = share_server_order_random_bytes_required(server_count);

    ShareSubmissionRandomBytesRequired {
        submit_at_random_bytes: submit_at_per_share.saturating_mul(share_count),
        server_random_bytes: server_per_share.saturating_mul(share_count),
    }
}

/// Return the random bytes needed for `resubmission_server_order`.
pub fn resubmission_server_order_random_bytes_required(
    configured_server_urls: &[String],
    sent_to_urls: &[String],
) -> usize {
    let sent: HashSet<&str> = sent_to_urls.iter().map(String::as_str).collect();
    let untried_count = configured_server_urls
        .iter()
        .filter(|server| !sent.contains(server.as_str()))
        .count();
    let already_sent_count = configured_server_urls
        .iter()
        .filter(|server| sent.contains(server.as_str()))
        .count();
    share_server_order_random_bytes_required(untried_count)
        .saturating_add(share_server_order_random_bytes_required(already_sent_count))
}

/// Return a randomized helper-server order using caller-provided entropy.
///
/// `server_urls` must not contain duplicates. Duplicate helpers are treated as
/// configuration errors because target counts are based on distinct endpoints.
///
/// `random_bytes` must contain at least
/// `share_server_order_random_bytes_required(server_urls.len())` bytes from a
/// cryptographically secure RNG. Extra bytes are ignored.
pub fn shuffled_share_server_order(
    server_urls: &[String],
    random_bytes: &[u8],
) -> Result<Vec<String>, VotingError> {
    require_unique_share_servers(server_urls)?;
    let needed = share_server_order_random_bytes_required(server_urls.len());
    if random_bytes.len() < needed {
        return Err(VotingError::InvalidInput {
            message: format!(
                "server_random_bytes must contain at least {needed} bytes for {} servers",
                server_urls.len()
            ),
        });
    }

    let mut ordered = server_urls.to_vec();
    let mut offset = 0usize;
    for index in (1..ordered.len()).rev() {
        let mut sample_bytes = [0u8; 8];
        sample_bytes.copy_from_slice(&random_bytes[offset..offset + 8]);
        offset += 8;

        let sample = u64::from_le_bytes(sample_bytes);
        let swap_index = (sample % ((index + 1) as u64)) as usize;
        ordered.swap(index, swap_index);
    }
    Ok(ordered)
}

/// Select initial helper targets from a randomized server order.
///
/// Prefer this helper for production submission paths. It owns the privacy
/// sensitive randomization policy and only asks callers to provide CSPRNG bytes.
pub fn select_share_submission_targets(
    server_urls: &[String],
    target_count: usize,
    server_random_bytes: &[u8],
) -> Result<Vec<String>, VotingError> {
    let target_count = target_count.min(server_urls.len());
    if target_count == 0 {
        return Ok(Vec::new());
    }

    let randomized_order = shuffled_share_server_order(server_urls, server_random_bytes)?;
    Ok(select_share_submission_targets_from_order(
        &randomized_order,
        target_count,
    ))
}

/// Select initial helper targets from a caller-provided server order.
///
/// This is deterministic for tests and callers that have already made an
/// explicit ordering decision. Production submission paths should prefer
/// `select_share_submission_targets`.
pub fn select_share_submission_targets_from_order(
    server_urls: &[String],
    target_count: usize,
) -> Vec<String> {
    server_urls
        .iter()
        .take(target_count.min(server_urls.len()))
        .cloned()
        .collect()
}

/// Plan the timing and initial helper targets for a share delegation.
///
/// `server_urls` must not be empty. Missing helpers are a configuration error
/// for initial delegation, not a successful zero-target plan.
/// `server_urls` must also not contain duplicates because target counts are
/// based on distinct endpoints.
///
/// Missing or zero `last_moment_buffer_seconds` means there is no delayed-share
/// window, so the returned plan uses `submit_at = 0`. Helper targets are chosen
/// from a randomized server order using `server_random_bytes`. Callers can use
/// `share_submit_at_random_bytes_required` and
/// `share_server_order_random_bytes_required` to size the entropy inputs.
pub fn plan_share_submission(
    server_urls: &[String],
    now_seconds: u64,
    vote_end_time_seconds: u64,
    last_moment_buffer_seconds: Option<u64>,
    single_share: bool,
    submit_at_random_bytes: &[u8],
    server_random_bytes: &[u8],
) -> Result<ShareSubmissionPlan, VotingError> {
    require_share_servers(server_urls)?;
    let target_count = share_submission_target_count(server_urls.len());
    let target_servers =
        select_share_submission_targets(server_urls, target_count, server_random_bytes)?;
    let submit_at = scheduled_share_submit_at_from_entropy(
        now_seconds,
        vote_end_time_seconds,
        last_moment_buffer_seconds,
        single_share,
        submit_at_random_bytes,
    )?;

    build_share_submission_plan(target_count, target_servers, submit_at)
}

/// Plan independent timing and initial helper targets for multiple shares.
///
/// This is the preferred production helper when a wallet has multiple share
/// payloads. It consumes separate entropy for each returned plan so callers
/// cannot accidentally reuse one `submit_at` or helper target order for every
/// share. Use `share_submission_random_bytes_required` to size the two entropy
/// inputs.
///
/// For a complete normal 16-share commitment, planned targets stop using a
/// helper after eight shares while uncapped helpers remain. When the helper set
/// is too small to fill every target slot under that cap, the least-used helper
/// is selected again so liveness wins. Each returned plan already contains its
/// final initial target list. Fallback and recovery may exceed the planned cap
/// when needed to preserve liveness.
///
/// `server_urls` must contain distinct, valid helper endpoint URLs. This
/// function checks only that the list is non-empty and has no duplicates; it
/// does not parse URLs or probe helper health. Callers are responsible for
/// validating endpoints and assessing health before planning, because
/// unavailable helpers can prevent reaching the returned `target_count`.
pub fn plan_share_submissions(
    share_count: usize,
    server_urls: &[String],
    now_seconds: u64,
    vote_end_time_seconds: u64,
    last_moment_buffer_seconds: Option<u64>,
    single_share: bool,
    submit_at_random_bytes: &[u8],
    server_random_bytes: &[u8],
) -> Result<Vec<ShareSubmissionPlan>, VotingError> {
    if share_count == 0 {
        return Ok(Vec::new());
    }

    require_share_servers(server_urls)?;
    let submit_at_bytes_per_share = share_submit_at_random_bytes_required(
        now_seconds,
        vote_end_time_seconds,
        last_moment_buffer_seconds,
        single_share,
    );
    let server_bytes_per_share = share_server_order_random_bytes_required(server_urls.len());
    let submit_at_bytes_needed = checked_random_bytes_required(
        submit_at_bytes_per_share,
        share_count,
        "submit_at_random_bytes",
    )?;
    let server_bytes_needed =
        checked_random_bytes_required(server_bytes_per_share, share_count, "server_random_bytes")?;
    if submit_at_random_bytes.len() < submit_at_bytes_needed {
        return Err(VotingError::InvalidInput {
            message: format!(
                "submit_at_random_bytes must contain at least {submit_at_bytes_needed} bytes"
            ),
        });
    }
    if server_random_bytes.len() < server_bytes_needed {
        return Err(VotingError::InvalidInput {
            message: format!(
                "server_random_bytes must contain at least {server_bytes_needed} bytes"
            ),
        });
    }

    let target_count = share_submission_target_count(server_urls.len());
    let max_shares_per_server = if !single_share && share_count == VOTE_COMMITMENT_SHARE_COUNT {
        SHARE_HELPER_MAX_SHARES_PER_SERVER
    } else {
        usize::MAX
    };
    let mut server_usage = std::collections::HashMap::<String, usize>::new();

    let mut plans = Vec::with_capacity(share_count);
    for share_index in 0..share_count {
        let submit_at_start = share_index * submit_at_bytes_per_share;
        let submit_at_end = submit_at_start + submit_at_bytes_per_share;
        let server_start = share_index * server_bytes_per_share;
        let server_end = server_start + server_bytes_per_share;
        let target_servers = select_batch_share_submission_targets(
            server_urls,
            target_count,
            max_shares_per_server,
            &mut server_usage,
            &server_random_bytes[server_start..server_end],
        )?;
        let submit_at = scheduled_share_submit_at_from_entropy(
            now_seconds,
            vote_end_time_seconds,
            last_moment_buffer_seconds,
            single_share,
            &submit_at_random_bytes[submit_at_start..submit_at_end],
        )?;
        plans.push(build_share_submission_plan(
            target_count,
            target_servers,
            submit_at,
        )?);
    }

    Ok(plans)
}

/// Plan share submission using a caller-provided helper order.
///
/// `server_urls` must not be empty. Missing helpers are a configuration error
/// for initial delegation, not a successful zero-target plan.
/// `server_urls` must also not contain duplicates because target counts are
/// based on distinct endpoints.
///
/// This is deterministic for tests and callers that have already made an
/// explicit ordering decision. Production submission paths should prefer
/// `plan_share_submission`.
pub fn plan_share_submission_from_order(
    server_urls: &[String],
    now_seconds: u64,
    vote_end_time_seconds: u64,
    last_moment_buffer_seconds: Option<u64>,
    single_share: bool,
    random_unit: f64,
) -> Result<ShareSubmissionPlan, VotingError> {
    require_share_servers(server_urls)?;
    let target_count = share_submission_target_count(server_urls.len());
    let target_servers = select_share_submission_targets_from_order(server_urls, target_count);
    plan_share_submission_with_targets(
        target_count,
        target_servers,
        now_seconds,
        vote_end_time_seconds,
        last_moment_buffer_seconds,
        single_share,
        random_unit,
    )
}

fn plan_share_submission_with_targets(
    target_count: usize,
    target_servers: Vec<String>,
    now_seconds: u64,
    vote_end_time_seconds: u64,
    last_moment_buffer_seconds: Option<u64>,
    single_share: bool,
    random_unit: f64,
) -> Result<ShareSubmissionPlan, VotingError> {
    let submit_at = scheduled_share_submit_at_from_random_unit(
        now_seconds,
        vote_end_time_seconds,
        last_moment_buffer_seconds,
        single_share,
        random_unit,
    )?;

    build_share_submission_plan(target_count, target_servers, submit_at)
}

fn build_share_submission_plan(
    target_count: usize,
    target_servers: Vec<String>,
    submit_at: u64,
) -> Result<ShareSubmissionPlan, VotingError> {
    Ok(ShareSubmissionPlan {
        submit_at,
        target_count: bounded_u32(target_count, "target_count")?,
        target_servers,
    })
}

fn bounded_u32(value: usize, name: &str) -> Result<u32, VotingError> {
    BoundedU32::try_from(value)
        .map(|value| value.0)
        .map_err(|_| VotingError::InvalidInput {
            message: format!("{name} {value} does not fit u32"),
        })
}

fn require_share_servers(server_urls: &[String]) -> Result<(), VotingError> {
    if server_urls.is_empty() {
        return Err(VotingError::InvalidInput {
            message: "server_urls must not be empty".to_string(),
        });
    }

    require_unique_share_servers(server_urls)
}

fn require_unique_share_servers(server_urls: &[String]) -> Result<(), VotingError> {
    let mut seen = HashSet::new();
    for server_url in server_urls {
        if !seen.insert(server_url.as_str()) {
            return Err(VotingError::InvalidInput {
                message: "server_urls must not contain duplicates".to_string(),
            });
        }
    }

    Ok(())
}

/// Select the final targets for one share before its batch plan is constructed.
fn select_batch_share_submission_targets(
    server_urls: &[String],
    target_count: usize,
    max_shares_per_server: usize,
    server_usage: &mut std::collections::HashMap<String, usize>,
    server_random_bytes: &[u8],
) -> Result<Vec<String>, VotingError> {
    let randomized_order = shuffled_share_server_order(server_urls, server_random_bytes)?;
    Ok(select_targets_with_usage_cap(
        &randomized_order,
        target_count,
        max_shares_per_server,
        server_usage,
    ))
}

fn select_targets_with_usage_cap(
    server_order: &[String],
    target_count: usize,
    max_shares_per_server: usize,
    server_usage: &mut std::collections::HashMap<String, usize>,
) -> Vec<String> {
    let target_count = target_count.min(server_order.len());
    let mut selected = Vec::with_capacity(target_count);

    for server in server_order {
        if server_usage.get(server).copied().unwrap_or(0) < max_shares_per_server {
            selected.push(server.clone());
            if selected.len() == target_count {
                break;
            }
        }
    }

    if selected.len() < target_count {
        let selected_set: HashSet<&str> = selected.iter().map(String::as_str).collect();
        let mut remaining: Vec<(usize, &String)> = server_order
            .iter()
            .enumerate()
            .filter(|(_, server)| !selected_set.contains(server.as_str()))
            .collect();
        remaining
            .sort_by_key(|(rank, server)| (server_usage.get(*server).copied().unwrap_or(0), *rank));
        selected.extend(
            remaining
                .into_iter()
                .take(target_count - selected.len())
                .map(|(_, server)| server.clone()),
        );
    }

    for server in &selected {
        *server_usage.entry(server.clone()).or_default() += 1;
    }
    selected
}

fn checked_random_bytes_required(
    per_share: usize,
    share_count: usize,
    name: &str,
) -> Result<usize, VotingError> {
    per_share
        .checked_mul(share_count)
        .ok_or_else(|| VotingError::InvalidInput {
            message: format!("{name} requirement overflows usize"),
        })
}

/// Return resubmission order from separately ordered helper groups.
///
/// The returned order always tries untried helpers before helpers that already
/// received the share. This is deterministic for tests and callers that have
/// already made an explicit ordering decision. Production resubmission paths
/// should prefer `resubmission_server_order`.
pub fn resubmission_server_order_from_groups(
    untried_server_urls: &[String],
    already_sent_server_urls: &[String],
) -> Vec<String> {
    untried_server_urls
        .iter()
        .chain(already_sent_server_urls.iter())
        .cloned()
        .collect()
}

/// Return randomized resubmission order with untried helpers first.
///
/// The configured server list is split into untried and already-sent groups.
/// `configured_server_urls` must not contain duplicates because retry order is
/// based on distinct helper endpoints.
/// Each group is shuffled separately using `server_random_bytes`, then the
/// shuffled untried group is followed by the shuffled already-sent group.
/// Callers can use `resubmission_server_order_random_bytes_required` to size
/// the entropy input.
pub fn resubmission_server_order(
    configured_server_urls: &[String],
    sent_to_urls: &[String],
    server_random_bytes: &[u8],
) -> Result<Vec<String>, VotingError> {
    require_unique_share_servers(configured_server_urls)?;
    let sent: HashSet<&str> = sent_to_urls.iter().map(String::as_str).collect();
    let untried: Vec<String> = configured_server_urls
        .iter()
        .filter(|server| !sent.contains(server.as_str()))
        .cloned()
        .collect();
    let already_sent: Vec<String> = configured_server_urls
        .iter()
        .filter(|server| sent.contains(server.as_str()))
        .cloned()
        .collect();

    let untried_bytes = share_server_order_random_bytes_required(untried.len());
    let already_sent_bytes = share_server_order_random_bytes_required(already_sent.len());
    let needed =
        resubmission_server_order_random_bytes_required(configured_server_urls, sent_to_urls);
    if server_random_bytes.len() < needed {
        return Err(VotingError::InvalidInput {
            message: format!(
                "server_random_bytes must contain at least {needed} bytes for resubmission order"
            ),
        });
    }

    let randomized_untried =
        shuffled_share_server_order(&untried, &server_random_bytes[..untried_bytes])?;
    let randomized_already_sent = shuffled_share_server_order(
        &already_sent,
        &server_random_bytes[untried_bytes..untried_bytes + already_sent_bytes],
    )?;
    Ok(resubmission_server_order_from_groups(
        &randomized_untried,
        &randomized_already_sent,
    ))
}

/// Return resubmission order from configured helper order and already-sent set.
///
/// This preserves the configured order within each group. It is useful for
/// deterministic tests and callers that have already made an explicit ordering
/// decision. Production resubmission paths should prefer
/// `resubmission_server_order`.
pub fn resubmission_server_order_from_configured_order(
    configured_server_urls: &[String],
    sent_to_urls: &[String],
) -> Vec<String> {
    let sent: HashSet<&str> = sent_to_urls.iter().map(String::as_str).collect();
    let untried: Vec<String> = configured_server_urls
        .iter()
        .filter(|server| !sent.contains(server.as_str()))
        .cloned()
        .collect();
    let already_sent: Vec<String> = configured_server_urls
        .iter()
        .filter(|server| sent.contains(server.as_str()))
        .cloned()
        .collect();
    resubmission_server_order_from_groups(&untried, &already_sent)
}

fn min_second(current: Option<u64>, candidate: u64) -> Option<u64> {
    match current {
        Some(current) if current <= candidate => Some(current),
        _ => Some(candidate),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn share(submit_at: u64, created_at: u64) -> ShareDelegationRecord {
        ShareDelegationRecord {
            round_id: "round".to_string(),
            bundle_index: 0,
            proposal_id: 1,
            share_index: 0,
            sent_to_urls: vec!["https://helper.example.com".to_string()],
            nullifier: vec![7; 32],
            confirmed: false,
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
    fn last_moment_buffer_uses_two_fifths_of_round_duration() {
        assert_eq!(last_moment_buffer_seconds(1_000, 1_600), Some(240));
    }

    #[test]
    fn last_moment_buffer_caps_at_six_hours() {
        assert_eq!(
            last_moment_buffer_seconds(1_000, 1_000 + 24 * 60 * 60),
            Some(LAST_MOMENT_BUFFER_MAX_SECONDS)
        );
    }

    #[test]
    fn last_moment_buffer_caps_without_u64_overflow() {
        assert_eq!(
            last_moment_buffer_seconds(0, u64::MAX),
            Some(LAST_MOMENT_BUFFER_MAX_SECONDS)
        );
    }

    #[test]
    fn last_moment_buffer_rejects_invalid_round_timing() {
        assert_eq!(last_moment_buffer_seconds(1_000, 1_000), None);
        assert_eq!(last_moment_buffer_seconds(1_001, 1_000), None);
    }

    #[test]
    fn last_moment_buffer_rounds_up_to_whole_seconds() {
        assert_eq!(last_moment_buffer_seconds(1_000, 1_001), Some(1));
        assert_eq!(last_moment_buffer_seconds(1_000, 1_002), Some(1));
        assert_eq!(last_moment_buffer_seconds(1_000, 1_003), Some(2));
    }

    #[test]
    fn last_moment_deadline_subtracts_buffer_from_vote_end() {
        assert_eq!(last_moment_deadline_seconds(1_000, 1_600), Some(1_360));
        assert_eq!(
            last_moment_deadline_seconds(1_000, 1_000 + 24 * 60 * 60),
            Some(1_000 + 18 * 60 * 60)
        );
    }

    #[test]
    fn last_moment_predicate_uses_deadline_boundary() {
        assert!(!is_last_moment(1_359, 1_000, 1_600));
        assert!(is_last_moment(1_360, 1_000, 1_600));
        assert!(is_last_moment(1_599, 1_000, 1_600));
        assert!(!is_last_moment(1_600, 1_000, 1_600));
        assert!(!is_last_moment(1_000, 1_000, 1_000));
    }

    #[test]
    fn scheduled_submit_at_from_random_unit_samples_before_deadline() {
        let submit_at =
            scheduled_share_submit_at_from_random_unit(1_000, 2_000, Some(100), false, 0.5)
                .unwrap();
        assert_eq!(submit_at, 1_450);
    }

    #[test]
    fn delayed_share_window_caps_long_round_at_100_hours() {
        let now = 1_000_000;
        let vote_end = now + 30 * 24 * 60 * 60;

        assert_eq!(
            delayed_share_window_seconds(
                now,
                vote_end,
                Some(LAST_MOMENT_BUFFER_MAX_SECONDS),
                false,
            ),
            Some(SHARE_SUBMIT_AT_MAX_DELAY_SECONDS)
        );
        assert_eq!(
            scheduled_share_submit_at_from_random_unit(
                now,
                vote_end,
                Some(LAST_MOMENT_BUFFER_MAX_SECONDS),
                false,
                0.5,
            )
            .unwrap(),
            now + 50 * 60 * 60
        );
    }

    #[test]
    fn delayed_share_window_preserves_shorter_round_deadline() {
        let now = 1_000_000;
        let vote_end = now + 36 * 60 * 60;

        assert_eq!(
            delayed_share_window_seconds(
                now,
                vote_end,
                Some(LAST_MOMENT_BUFFER_MAX_SECONDS),
                false,
            ),
            Some(30 * 60 * 60)
        );
    }

    #[test]
    fn delayed_share_window_is_immediate_inside_last_moment_buffer() {
        let now = 1_000_000;
        let buffer = LAST_MOMENT_BUFFER_MAX_SECONDS;

        assert_eq!(
            delayed_share_window_seconds(now, now + buffer, Some(buffer), false),
            None
        );
        assert_eq!(
            delayed_share_window_seconds(now, now + buffer - 1, Some(buffer), false),
            None
        );
    }

    #[test]
    fn delayed_share_window_handles_clock_skew_without_underflow() {
        assert_eq!(
            delayed_share_window_seconds(u64::MAX, u64::MAX - 1, Some(1), false),
            None
        );
        assert_eq!(delayed_share_window_seconds(1, 5, Some(10), false), None);
        assert_eq!(
            scheduled_share_submit_at_from_entropy(u64::MAX, u64::MAX - 1, Some(1), false, &[])
                .unwrap(),
            0
        );
    }

    #[test]
    fn capped_submit_at_samples_remain_randomized_within_window() {
        let now = 1_000_000;
        let vote_end = now + 30 * 24 * 60 * 60;
        let samples = [0, 1u64 << 62, 1u64 << 63, 3u64 << 62, u64::MAX];
        let window_end = now + SHARE_SUBMIT_AT_MAX_DELAY_SECONDS;
        let mut submit_times = Vec::new();

        for sample in samples {
            let submit_at = scheduled_share_submit_at_from_entropy(
                now,
                vote_end,
                Some(LAST_MOMENT_BUFFER_MAX_SECONDS),
                false,
                &random_bytes(&[sample]),
            )
            .unwrap();
            assert!(submit_at >= now);
            assert!(submit_at < window_end);
            submit_times.push(submit_at);
        }

        assert_eq!(submit_times[0], now);
        assert_eq!(submit_times[4], window_end - 1);
        assert!(submit_times.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn retry_uses_capped_submit_at_without_adding_another_window() {
        let now = 1_000_000;
        let vote_end = now + 30 * 24 * 60 * 60;
        let submit_at = scheduled_share_submit_at_from_random_unit(
            now,
            vote_end,
            Some(LAST_MOMENT_BUFFER_MAX_SECONDS),
            false,
            0.5,
        )
        .unwrap();
        let share = share(submit_at, now);
        let policy = ShareTimingPolicy::default();

        assert_eq!(share_recovery_base_time(&share), now + 50 * 60 * 60);
        assert!(!should_resubmit_share(
            &share,
            submit_at + SHARE_MAX_OVERDUE_THRESHOLD_SECONDS - 1,
            vote_end,
            policy,
        ));
        assert!(should_resubmit_share(
            &share,
            submit_at + SHARE_MAX_OVERDUE_THRESHOLD_SECONDS,
            vote_end,
            policy,
        ));
    }

    #[test]
    fn scheduled_submit_at_entropy_requirement_matches_delay_window() {
        assert_eq!(
            share_submit_at_random_bytes_required(1_000, 2_000, Some(100), false),
            SHARE_SUBMIT_AT_RANDOM_BYTES
        );
        assert_eq!(
            share_submit_at_random_bytes_required(1_000, 2_000, Some(100), true),
            0
        );
        assert_eq!(
            share_submit_at_random_bytes_required(1_000, 2_000, None, false),
            0
        );
        assert_eq!(
            share_submit_at_random_bytes_required(1_950, 2_000, Some(100), false),
            0
        );
    }

    #[test]
    fn scheduled_submit_at_from_entropy_samples_before_deadline() {
        let submit_at = scheduled_share_submit_at_from_entropy(
            1_000,
            2_000,
            Some(100),
            false,
            &random_bytes(&[1u64 << 63]),
        )
        .unwrap();
        assert_eq!(submit_at, 1_450);
    }

    #[test]
    fn scheduled_submit_at_is_immediate_without_a_delay_window() {
        assert_eq!(
            scheduled_share_submit_at_from_random_unit(1_000, 2_000, Some(100), true, f64::NAN)
                .unwrap(),
            0
        );
        assert_eq!(
            scheduled_share_submit_at_from_random_unit(1_000, 2_000, None, false, f64::NAN)
                .unwrap(),
            0
        );
        assert_eq!(
            scheduled_share_submit_at_from_random_unit(1_000, 2_000, Some(0), false, f64::NAN)
                .unwrap(),
            0
        );
        assert_eq!(
            scheduled_share_submit_at_from_random_unit(1_950, 2_000, Some(100), false, f64::NAN)
                .unwrap(),
            0
        );
        assert_eq!(
            scheduled_share_submit_at_from_entropy(1_000, 2_000, Some(100), true, &[]).unwrap(),
            0
        );
    }

    #[test]
    fn scheduled_submit_at_rejects_non_finite_random_unit_for_delay_window() {
        assert!(matches!(
            scheduled_share_submit_at_from_random_unit(1_000, 2_000, Some(100), false, f64::NAN),
            Err(VotingError::InvalidInput { .. })
        ));
    }

    #[test]
    fn scheduled_submit_at_from_entropy_rejects_missing_entropy_for_delay_window() {
        assert!(matches!(
            scheduled_share_submit_at_from_entropy(1_000, 2_000, Some(100), false, &[]),
            Err(VotingError::InvalidInput { .. })
        ));
    }

    #[test]
    fn scheduled_submit_at_from_random_unit_rejects_out_of_range_samples() {
        assert!(matches!(
            scheduled_share_submit_at_from_random_unit(1_000, 2_000, Some(100), false, 1.0),
            Err(VotingError::InvalidInput { .. })
        ));
        assert!(matches!(
            scheduled_share_submit_at_from_random_unit(1_000, 2_000, Some(100), false, -1.0),
            Err(VotingError::InvalidInput { .. })
        ));
    }

    #[test]
    fn immediate_shares_use_created_at_for_status_and_retry() {
        let share = share(0, 100);
        let policy = ShareTimingPolicy::default();

        assert_eq!(share_recovery_base_time(&share), 100);
        assert!(!is_share_ready_for_status_check(&share, 109, policy));
        assert!(is_share_ready_for_status_check(&share, 110, policy));
        assert!(!should_resubmit_share(&share, 129, 200, policy));
        assert!(should_resubmit_share(&share, 130, 200, policy));
    }

    #[test]
    fn delayed_shares_use_submit_at_for_status_and_retry() {
        let share = share(200, 100);
        let policy = ShareTimingPolicy::default();

        assert_eq!(share_recovery_base_time(&share), 200);
        assert!(!is_share_ready_for_status_check(&share, 209, policy));
        assert!(is_share_ready_for_status_check(&share, 210, policy));
        assert!(!should_resubmit_share(&share, 229, 320, policy));
        assert!(should_resubmit_share(&share, 230, 320, policy));
    }

    #[test]
    fn overdue_threshold_is_quarter_window_with_bounds() {
        let share = share(0, 100);
        let policy = ShareTimingPolicy::default();

        assert_eq!(overdue_threshold_seconds(&share, 500, policy), 100);
        assert_eq!(overdue_threshold_seconds(&share, 120, policy), 30);
        assert_eq!(overdue_threshold_seconds(&share, 20_000, policy), 3_600);
    }

    #[test]
    fn should_resubmit_respects_vote_end_cutoff() {
        let share = share(0, 100);
        let policy = ShareTimingPolicy::default();

        assert!(should_resubmit_share(&share, 130, 200, policy));
        assert!(!should_resubmit_share(&share, 190, 200, policy));
    }

    #[test]
    fn next_tracking_delay_uses_future_check_times() {
        let shares = vec![share(0, 100), share(200, 100)];
        let policy = ShareTimingPolicy::default();

        assert_eq!(next_tracking_delay_seconds(&shares, 105, policy), Some(5));
    }

    #[test]
    fn next_tracking_delay_applies_minimum_and_future_cap() {
        let shares = vec![share(0, 100), share(200, 100)];
        let policy = ShareTimingPolicy::default();

        assert_eq!(next_tracking_delay_seconds(&shares, 109, policy), Some(3));
        assert_eq!(next_tracking_delay_seconds(&shares, 111, policy), Some(30));
    }

    #[test]
    fn next_tracking_delay_uses_ready_poll_interval_for_ready_pending_shares() {
        let shares = vec![share(0, 100)];
        let policy = ShareTimingPolicy::default();

        assert_eq!(next_tracking_delay_seconds(&shares, 130, policy), Some(15));
        assert_eq!(next_tracking_delay_seconds(&shares, 131, policy), Some(15));
    }

    #[test]
    fn next_tracking_delay_stops_when_all_shares_are_confirmed() {
        let mut confirmed = share(0, 100);
        confirmed.confirmed = true;

        assert_eq!(
            next_tracking_delay_seconds(&[confirmed], 130, ShareTimingPolicy::default()),
            None
        );
    }

    #[test]
    fn tracking_summary_uses_confirmed_overdue_ready_waiting_order() {
        let mut confirmed = share(0, 100);
        confirmed.confirmed = true;
        let overdue = share(0, 100);
        let ready = share(120, 100);
        let waiting = share(300, 100);
        let shares = vec![confirmed, overdue, ready, waiting];

        let summary =
            summarize_share_tracking(&shares, 130, Some(200), ShareTimingPolicy::default());

        assert_eq!(
            summary,
            ShareTrackingSummary {
                total: 4,
                confirmed: 1,
                waiting: 1,
                ready: 1,
                overdue: 1,
            }
        );
        assert!(summary.has_shares());
    }

    #[test]
    fn helper_target_count_is_half_rounded_up() {
        assert_eq!(share_submission_target_count(0), 0);
        assert_eq!(share_submission_target_count(1), 1);
        assert_eq!(share_submission_target_count(2), 1);
        assert_eq!(share_submission_target_count(3), 2);
        assert_eq!(share_submission_target_count(5), 3);
    }

    #[test]
    fn complete_vote_helper_policy_uses_progressive_timeouts_and_half_share_cap() {
        assert_eq!(
            share_server_selection_policy(10).unwrap(),
            ShareServerSelectionPolicy {
                target_count: 5,
                max_shares_per_server: 8,
                preflight_soft_timeout_milliseconds: 2_000,
                preflight_hard_timeout_milliseconds: 30_000,
            }
        );
    }

    #[test]
    fn ranked_candidates_reselect_after_fast_helpers_reach_the_share_cap() {
        let servers: Vec<String> = (0..10)
            .map(|index| format!("https://helper-{index}.example.com"))
            .collect();

        let candidates =
            ranked_share_submission_server_candidates(VOTE_COMMITMENT_SHARE_COUNT, &servers)
                .unwrap();

        assert_eq!(candidates.len(), VOTE_COMMITMENT_SHARE_COUNT);
        assert!(candidates.iter().all(|row| row.len() == servers.len()));
        assert!(candidates[..8].iter().all(|row| row[..5] == servers[..5]));
        assert!(candidates[8..].iter().all(|row| row[..5] == servers[5..]));

        let mut planned_counts = std::collections::HashMap::<&str, usize>::new();
        for row in &candidates {
            let unique: HashSet<&str> = row.iter().map(String::as_str).collect();
            assert_eq!(unique.len(), servers.len());
            for server in &row[..5] {
                *planned_counts.entry(server.as_str()).or_default() += 1;
            }
        }
        assert!(servers
            .iter()
            .all(|server| planned_counts[server.as_str()] == 8));
    }

    #[test]
    fn ranked_candidates_reject_duplicate_helpers() {
        let servers = vec![
            "https://helper.example.com".to_string(),
            "https://helper.example.com".to_string(),
        ];

        assert!(matches!(
            ranked_share_submission_server_candidates(16, &servers),
            Err(VotingError::InvalidInput { .. })
        ));
    }

    #[test]
    fn ranked_candidates_preserve_usage_across_resume() {
        let servers: Vec<String> = (0..10)
            .map(|index| format!("https://helper-{index}.example.com"))
            .collect();
        let previously_selected: Vec<String> = servers[..5]
            .iter()
            .flat_map(|server| std::iter::repeat_n(server.clone(), 8))
            .collect();

        let candidates =
            ranked_share_submission_server_candidates_with_usage(8, &servers, &previously_selected)
                .unwrap();

        assert!(candidates.iter().all(|row| row[..5] == servers[5..]));
    }

    #[test]
    fn ranked_candidates_reject_usage_for_an_unknown_helper() {
        let servers = vec!["https://helper.example.com".to_string()];

        assert!(matches!(
            ranked_share_submission_server_candidates_with_usage(
                1,
                &servers,
                &["https://unknown.example.com".to_string()],
            ),
            Err(VotingError::InvalidInput { .. })
        ));
    }

    #[test]
    fn helper_order_random_bytes_required_matches_shuffle_steps() {
        assert_eq!(share_server_order_random_bytes_required(0), 0);
        assert_eq!(share_server_order_random_bytes_required(1), 0);
        assert_eq!(share_server_order_random_bytes_required(3), 16);
    }

    #[test]
    fn resubmission_order_random_bytes_required_matches_group_shuffles() {
        let configured = vec![
            "https://already-one.example.com".to_string(),
            "https://untried-one.example.com".to_string(),
            "https://untried-two.example.com".to_string(),
            "https://already-two.example.com".to_string(),
        ];
        let sent = vec![
            "https://already-one.example.com".to_string(),
            "https://already-two.example.com".to_string(),
        ];

        assert_eq!(
            resubmission_server_order_random_bytes_required(&configured, &sent),
            16
        );
    }

    #[test]
    fn randomized_helper_order_uses_entropy() {
        let servers = vec![
            "https://one.example.com".to_string(),
            "https://two.example.com".to_string(),
            "https://three.example.com".to_string(),
        ];

        let ordered = shuffled_share_server_order(&servers, &random_bytes(&[1, 0])).unwrap();

        assert_eq!(
            ordered,
            vec![
                "https://three.example.com".to_string(),
                "https://one.example.com".to_string(),
                "https://two.example.com".to_string()
            ]
        );
    }

    #[test]
    fn randomized_helper_order_rejects_missing_entropy() {
        let servers = vec![
            "https://one.example.com".to_string(),
            "https://two.example.com".to_string(),
        ];

        assert!(matches!(
            shuffled_share_server_order(&servers, &[]),
            Err(VotingError::InvalidInput { .. })
        ));
    }

    #[test]
    fn randomized_helper_order_rejects_duplicate_server_urls() {
        let servers = vec![
            "https://one.example.com".to_string(),
            "https://one.example.com".to_string(),
        ];

        assert!(matches!(
            shuffled_share_server_order(&servers, &random_bytes(&[0])),
            Err(VotingError::InvalidInput { .. })
        ));
    }

    #[test]
    fn share_submission_plan_randomizes_target_servers() {
        let servers = vec![
            "https://one.example.com".to_string(),
            "https://two.example.com".to_string(),
            "https://three.example.com".to_string(),
        ];

        let plan = plan_share_submission(
            &servers,
            1_000,
            2_000,
            Some(100),
            false,
            &random_bytes(&[1u64 << 63]),
            &random_bytes(&[1, 0]),
        )
        .unwrap();

        assert_eq!(plan.submit_at, 1_450);
        assert_eq!(plan.target_count, 2);
        assert_eq!(
            plan.target_servers,
            vec![
                "https://three.example.com".to_string(),
                "https://one.example.com".to_string()
            ]
        );
    }

    #[test]
    fn share_submission_plan_rejects_empty_server_list() {
        assert!(matches!(
            plan_share_submission(
                &[],
                1_000,
                2_000,
                Some(100),
                false,
                &random_bytes(&[1u64 << 63]),
                &[]
            ),
            Err(VotingError::InvalidInput { .. })
        ));
        assert!(matches!(
            plan_share_submission_from_order(&[], 1_000, 2_000, Some(100), false, 0.0),
            Err(VotingError::InvalidInput { .. })
        ));
    }

    #[test]
    fn share_submission_plan_rejects_duplicate_server_urls() {
        let servers = vec![
            "https://one.example.com".to_string(),
            "https://one.example.com".to_string(),
        ];

        assert!(matches!(
            plan_share_submission(
                &servers,
                1_000,
                2_000,
                Some(100),
                false,
                &random_bytes(&[1u64 << 63]),
                &random_bytes(&[0])
            ),
            Err(VotingError::InvalidInput { .. })
        ));
        assert!(matches!(
            plan_share_submission_from_order(&servers, 1_000, 2_000, Some(100), false, 0.0),
            Err(VotingError::InvalidInput { .. })
        ));
    }

    #[test]
    fn share_submission_random_bytes_required_counts_independent_share_plans() {
        assert_eq!(
            share_submission_random_bytes_required(2, 3, 1_000, 2_000, Some(100), false),
            ShareSubmissionRandomBytesRequired {
                submit_at_random_bytes: 16,
                server_random_bytes: 32,
            }
        );
        assert_eq!(
            share_submission_random_bytes_required(2, 3, 1_000, 2_000, Some(100), true),
            ShareSubmissionRandomBytesRequired {
                submit_at_random_bytes: 0,
                server_random_bytes: 32,
            }
        );
    }

    #[test]
    fn share_submission_batch_plan_uses_independent_entropy_per_share() {
        let servers = vec![
            "https://one.example.com".to_string(),
            "https://two.example.com".to_string(),
            "https://three.example.com".to_string(),
        ];

        let plans = plan_share_submissions(
            2,
            &servers,
            1_000,
            2_000,
            Some(100),
            false,
            &random_bytes(&[0, 1u64 << 63]),
            &random_bytes(&[1, 0, 0, 1]),
        )
        .unwrap();

        assert_eq!(plans.len(), 2);
        assert_eq!(plans[0].submit_at, 1_000);
        assert_eq!(
            plans[0].target_servers,
            vec![
                "https://three.example.com".to_string(),
                "https://one.example.com".to_string()
            ]
        );
        assert_eq!(plans[1].submit_at, 1_450);
        assert_eq!(
            plans[1].target_servers,
            vec![
                "https://three.example.com".to_string(),
                "https://two.example.com".to_string()
            ]
        );
    }

    #[test]
    fn share_submission_batch_plan_reselects_after_repeated_targets_reach_the_cap() {
        let servers = vec![
            "https://one.example.com".to_string(),
            "https://two.example.com".to_string(),
            "https://three.example.com".to_string(),
        ];
        let repeated_shuffle = vec![0u64; 16 * 2];

        let plans = plan_share_submissions(
            16,
            &servers,
            1_000,
            2_000,
            None,
            false,
            &[],
            &random_bytes(&repeated_shuffle),
        )
        .unwrap();

        assert!(plans.iter().all(|plan| plan.target_servers.len() == 2));
        assert_eq!(
            plans[0].target_servers,
            vec![servers[1].clone(), servers[2].clone()]
        );
        assert!(plans[..8]
            .iter()
            .all(|plan| plan.target_servers == vec![servers[1].clone(), servers[2].clone()]));
        assert!(plans[8..]
            .iter()
            .all(|plan| plan.target_servers.contains(&servers[0])));
        for server in &servers {
            assert!(
                plans
                    .iter()
                    .any(|plan| !plan.target_servers.contains(server)),
                "{server} must be omitted from at least one share"
            );
        }
    }

    #[test]
    fn complete_vote_batch_plan_caps_ten_helpers_at_eight_shares_each() {
        let servers: Vec<String> = (0..10)
            .map(|index| format!("https://helper-{index}.example.com"))
            .collect();
        let server_random_bytes = vec![
            0;
            VOTE_COMMITMENT_SHARE_COUNT
                * share_server_order_random_bytes_required(servers.len())
        ];

        let plans = plan_share_submissions(
            VOTE_COMMITMENT_SHARE_COUNT,
            &servers,
            1_000,
            2_000,
            None,
            false,
            &[],
            &server_random_bytes,
        )
        .unwrap();

        let mut counts = std::collections::HashMap::<&str, usize>::new();
        for plan in &plans {
            assert_eq!(plan.target_servers.len(), 5);
            for server in &plan.target_servers {
                *counts.entry(server.as_str()).or_default() += 1;
            }
        }
        assert!(servers.iter().all(|server| counts[server.as_str()] == 8));
    }

    #[test]
    fn share_submission_batch_plan_spreads_more_helpers_than_shares() {
        let servers: Vec<String> = (0..33)
            .map(|index| format!("https://helper-{index}.example.com"))
            .collect();
        let server_random_bytes = vec![
            0;
            VOTE_COMMITMENT_SHARE_COUNT
                * share_server_order_random_bytes_required(servers.len())
        ];

        let plans = plan_share_submissions(
            VOTE_COMMITMENT_SHARE_COUNT,
            &servers,
            1_000,
            2_000,
            None,
            false,
            &[],
            &server_random_bytes,
        )
        .unwrap();

        assert!(plans.iter().all(|plan| plan.target_servers.len() == 17));
        for server in &servers {
            assert!(
                plans
                    .iter()
                    .any(|plan| !plan.target_servers.contains(server)),
                "{server} must be omitted from at least one share"
            );
        }
    }

    #[test]
    fn share_submission_batch_plan_spreads_two_helpers() {
        let servers = vec![
            "https://one.example.com".to_string(),
            "https://two.example.com".to_string(),
        ];
        let repeated_shuffle = vec![0u64; 16];

        let plans = plan_share_submissions(
            16,
            &servers,
            1_000,
            2_000,
            None,
            false,
            &[],
            &random_bytes(&repeated_shuffle),
        )
        .unwrap();

        assert!(plans.iter().all(|plan| plan.target_servers.len() == 1));
        for server in &servers {
            assert!(
                plans
                    .iter()
                    .any(|plan| !plan.target_servers.contains(server)),
                "{server} must be omitted from at least one share"
            );
        }
    }

    #[test]
    fn share_submission_batch_plan_allows_one_helper_when_no_alternative_exists() {
        let servers = vec!["https://only.example.com".to_string()];

        let plans =
            plan_share_submissions(16, &servers, 1_000, 2_000, None, false, &[], &[]).unwrap();

        assert!(plans.iter().all(|plan| plan.target_servers == servers));
    }

    #[test]
    fn share_submission_batch_plan_rejects_missing_entropy() {
        let servers = vec![
            "https://one.example.com".to_string(),
            "https://two.example.com".to_string(),
            "https://three.example.com".to_string(),
        ];

        assert!(matches!(
            plan_share_submissions(
                2,
                &servers,
                1_000,
                2_000,
                Some(100),
                false,
                &random_bytes(&[0]),
                &random_bytes(&[1, 0, 0, 1]),
            ),
            Err(VotingError::InvalidInput { .. })
        ));
        assert!(matches!(
            plan_share_submissions(
                2,
                &servers,
                1_000,
                2_000,
                Some(100),
                false,
                &random_bytes(&[0, 1u64 << 63]),
                &random_bytes(&[1, 0, 0]),
            ),
            Err(VotingError::InvalidInput { .. })
        ));
    }

    #[test]
    fn share_submission_plan_from_order_uses_caller_server_order() {
        let servers = vec![
            "https://one.example.com".to_string(),
            "https://two.example.com".to_string(),
            "https://three.example.com".to_string(),
        ];

        let plan = plan_share_submission_from_order(&servers, 1_000, 2_000, Some(100), false, 0.0)
            .unwrap();

        assert_eq!(plan.submit_at, 1_000);
        assert_eq!(plan.target_count, 2);
        assert_eq!(
            plan.target_servers,
            vec![
                "https://one.example.com".to_string(),
                "https://two.example.com".to_string()
            ]
        );
    }

    #[test]
    fn share_submission_target_selection_uses_caller_server_order() {
        let servers = vec![
            "https://three.example.com".to_string(),
            "https://one.example.com".to_string(),
            "https://two.example.com".to_string(),
        ];

        assert_eq!(
            select_share_submission_targets_from_order(&servers, 2),
            vec![
                "https://three.example.com".to_string(),
                "https://one.example.com".to_string()
            ]
        );
    }

    #[test]
    fn randomized_share_submission_target_selection_uses_entropy() {
        let servers = vec![
            "https://one.example.com".to_string(),
            "https://two.example.com".to_string(),
            "https://three.example.com".to_string(),
        ];

        assert_eq!(
            select_share_submission_targets(&servers, 2, &random_bytes(&[1, 0])).unwrap(),
            vec![
                "https://three.example.com".to_string(),
                "https://one.example.com".to_string()
            ]
        );
    }

    #[test]
    fn resubmission_order_tries_ordered_untried_helpers_first() {
        let untried = vec![
            "https://untried-two.example.com".to_string(),
            "https://untried-one.example.com".to_string(),
        ];
        let already_sent = vec!["https://already.example.com".to_string()];

        assert_eq!(
            resubmission_server_order_from_groups(&untried, &already_sent),
            vec![
                "https://untried-two.example.com".to_string(),
                "https://untried-one.example.com".to_string(),
                "https://already.example.com".to_string()
            ]
        );
    }

    #[test]
    fn randomized_resubmission_order_shuffles_groups_separately() {
        let configured = vec![
            "https://already-one.example.com".to_string(),
            "https://untried-one.example.com".to_string(),
            "https://untried-two.example.com".to_string(),
            "https://already-two.example.com".to_string(),
        ];
        let sent = vec![
            "https://already-one.example.com".to_string(),
            "https://already-two.example.com".to_string(),
        ];

        assert_eq!(
            resubmission_server_order(&configured, &sent, &random_bytes(&[0, 0])).unwrap(),
            vec![
                "https://untried-two.example.com".to_string(),
                "https://untried-one.example.com".to_string(),
                "https://already-two.example.com".to_string(),
                "https://already-one.example.com".to_string()
            ]
        );
    }

    #[test]
    fn randomized_resubmission_order_rejects_missing_entropy() {
        let configured = vec![
            "https://untried-one.example.com".to_string(),
            "https://untried-two.example.com".to_string(),
        ];

        assert!(matches!(
            resubmission_server_order(&configured, &[], &[]),
            Err(VotingError::InvalidInput { .. })
        ));
    }

    #[test]
    fn randomized_resubmission_order_rejects_duplicate_configured_urls() {
        let configured = vec![
            "https://untried-one.example.com".to_string(),
            "https://untried-one.example.com".to_string(),
        ];

        assert!(matches!(
            resubmission_server_order(&configured, &[], &random_bytes(&[0])),
            Err(VotingError::InvalidInput { .. })
        ));
    }

    #[test]
    fn resubmission_order_from_configured_order_preserves_group_order() {
        let configured = vec![
            "https://already.example.com".to_string(),
            "https://untried.example.com".to_string(),
        ];
        let sent = vec!["https://already.example.com".to_string()];

        assert_eq!(
            resubmission_server_order_from_configured_order(&configured, &sent),
            vec![
                "https://untried.example.com".to_string(),
                "https://already.example.com".to_string()
            ]
        );
    }
}
