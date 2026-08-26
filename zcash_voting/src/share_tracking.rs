//! Helper-share confirmation and recovery loop.
//!
//! [`share_policy`](crate::share_policy) decides *when* a share should be
//! checked or retried, and [`share`](crate::share) stores the result. This
//! module is the part in between: it asks helpers what they know, decides what
//! their answers mean, and drives the durable state forward.
//!
//! Wallets should call [`track_pending_shares`] on a timer and keep only the
//! lifecycle concerns — the timer itself, app lock, and round expiry — on their
//! side, surfaced through the `cancel` callback.
//!
//! # Trust model
//!
//! Helper responses are authenticated only by the host transport's connection
//! to a configured endpoint. They are not chain proofs. In particular, one
//! configured helper reporting `confirmed` is trusted as an oracle for the
//! share nullifier's global on-chain state. A dishonest configured helper can
//! therefore suppress local recovery by falsely reporting confirmation. Hosts
//! that do not trust every configured helper for this assertion must verify the
//! nullifier through their own trusted chain source before calling
//! [`share::confirm`], rather than using [`track_pending_shares`] to persist
//! confirmation.

use std::{
    collections::HashSet,
    time::{Duration, Instant},
};

use crate::{
    helper::{
        client::{HelperClient, HelperError, ShareStatus},
        url::canonicalize_helper_base_url,
    },
    recovery,
    round::VotingDb,
    share,
    share_policy::{
        is_share_ready_for_status_check, is_share_resubmission_window_open,
        next_tracking_delay_seconds, resubmission_server_order,
        resubmission_server_order_random_bytes_required, share_submission_target_count,
        should_resubmit_share, ShareTimingPolicy, SHARE_INITIAL_DELIVERY_TIMEOUT_MILLISECONDS,
    },
    types::{ShareDelegationRecord, VotingError},
};

/// Identifies one helper share within a round.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ShareKey {
    pub bundle_index: u32,
    pub proposal_id: u32,
    pub share_index: u32,
}

impl ShareKey {
    fn of(share: &ShareDelegationRecord) -> Self {
        Self {
            bundle_index: share.bundle_index,
            proposal_id: share.proposal_id,
            share_index: share.share_index,
        }
    }
}

/// What the timing policy says should happen to a share right now.
///
/// This replaces the historical bitmask, which forced every wallet to hardcode
/// `flags & 1` / `flags & 2` against constants it could not see.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct ShareTrackingFlags {
    /// Enough time has passed since submission to ask helpers about the share.
    pub ready_for_status_check: bool,
    /// The share has gone unconfirmed long enough to warrant another helper.
    pub overdue_for_retry: bool,
}

impl ShareTrackingFlags {
    /// Returns true when this share needs no work in the current pass.
    pub fn is_idle(&self) -> bool {
        !self.ready_for_status_check && !self.overdue_for_retry
    }
}

/// Returns the tracking flags for one share.
///
/// `vote_end_time_seconds` is optional because some rounds have no usable end
/// time yet. Without it a share can still be status-checked but is never
/// treated as overdue: the overdue threshold is a fraction of the remaining
/// vote window, so with no window there is nothing to measure against, and
/// guessing would resubmit shares that are merely young.
pub fn share_tracking_flags(
    share: &ShareDelegationRecord,
    now_seconds: u64,
    vote_end_time_seconds: Option<u64>,
    policy: ShareTimingPolicy,
) -> ShareTrackingFlags {
    ShareTrackingFlags {
        ready_for_status_check: is_share_ready_for_status_check(share, now_seconds, policy),
        overdue_for_retry: vote_end_time_seconds
            .is_some_and(|vote_end| should_resubmit_share(share, now_seconds, vote_end, policy)),
    }
}

/// Result of polling helpers about one share.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShareStatusOutcome {
    /// At least one helper reported the share confirmed.
    Confirmed,
    /// Every helper answered without confirming, or could not answer.
    NotConfirmed,
    /// The caller asked to stop mid-poll. Nothing was decided.
    Cancelled,
}

/// One helper that accepted a resubmitted share.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResubmittedShare {
    pub share: ShareKey,
    pub server_url: String,
}

/// Results of an initial fan-out across helper servers.
///
/// Pass this report through [`share::ShareDeliveryRecordParams`] to
/// [`share::record_delivery`]. Ambiguous attempts remain poll-only and do not
/// count toward `target_count`: the current status endpoint reports on-chain
/// confirmation, not whether one helper possesses the share.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShareSubmissionReport {
    /// Helpers that definitively accepted the share.
    pub accepted_urls: Vec<String>,
    /// Helpers that may have accepted the share before the response failed.
    pub ambiguous_urls: Vec<String>,
    /// Desired number of definite helper placements.
    pub target_count: usize,
}

/// A freshly built share plus the durable identity used to journal each POST.
#[derive(Clone, Copy, Debug)]
pub struct InitialShareSubmissionParams<'a> {
    pub round_id: &'a str,
    pub bundle_index: u32,
    pub proposal_id: u32,
    pub share_index: u32,
    pub share_wire_json: &'a str,
    pub candidate_servers: &'a [String],
    pub target_count: usize,
    pub submit_at: u64,
    pub now_seconds: u64,
}

/// What one tracking pass did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShareTrackingReport {
    /// Shares marked confirmed during this pass.
    pub confirmed: Vec<ShareKey>,
    /// Shares that reached a new helper during this pass.
    pub resubmitted: Vec<ResubmittedShare>,
    /// Outcome-unknown attempts durably retained during this pass.
    pub ambiguous: Vec<ResubmittedShare>,
    /// Shares skipped because their recovery material is missing.
    ///
    /// These cannot be repaired by retrying; a wallet should surface them
    /// rather than spin on them.
    pub unrecoverable: Vec<ShareKey>,
    /// True when the pass stopped early because `cancel` fired.
    pub cancelled: bool,
    /// Seconds to wait before the next pass, or `None` when nothing is pending.
    pub next_delay_seconds: Option<u64>,
}

/// Inputs for one tracking pass.
pub struct ShareTrackingParams<'a> {
    /// Round whose unconfirmed shares should be tracked.
    pub round_id: &'a str,
    /// Helper URLs currently configured for this wallet.
    ///
    /// A share's persisted `sent_to_urls` is intersected with this list, so a
    /// helper dropped from config is neither polled nor counted.
    pub configured_server_urls: &'a [String],
    pub now_seconds: u64,
    pub vote_end_time_seconds: Option<u64>,
    pub policy: ShareTimingPolicy,
    /// Source of CSPRNG bytes for randomized resubmission order.
    ///
    /// Callers supply this so tests can be deterministic; production wallets
    /// pass [`os_random_bytes`].
    pub random_bytes: &'a (dyn Fn(usize) -> Vec<u8> + Send + Sync),
}

/// Fills `len` bytes from the operating system CSPRNG.
pub fn os_random_bytes(len: usize) -> Vec<u8> {
    use rand::RngCore as _;

    let mut bytes = vec![0u8; len];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    bytes
}

/// Asks helpers whether a share is confirmed, stopping at the first `yes`.
///
/// # Policy
///
/// The endpoint reports only whether the nullifier is confirmed on-chain; it
/// does not inspect a helper's local queue. One valid `confirmed` response is
/// therefore authoritative for the local workflow and stops the walk, while
/// `pending` provides no possession evidence and only keeps polling alive.
/// This API assumes every `server_urls` entry is trusted to report the global
/// chain state honestly; see the module-level trust model.
pub async fn confirm_share_by_any_helper(
    client: &HelperClient,
    round_id: &str,
    share_id: &str,
    server_urls: &[String],
    now_seconds: u64,
    cancel: &(dyn Fn() -> bool + Send + Sync),
) -> ShareStatusOutcome {
    poll_share_helpers(client, round_id, share_id, server_urls, now_seconds, cancel).await
}

/// Polls helpers for the share's global on-chain confirmation state.
async fn poll_share_helpers(
    client: &HelperClient,
    round_id: &str,
    share_id: &str,
    server_urls: &[String],
    now_seconds: u64,
    cancel: &(dyn Fn() -> bool + Send + Sync),
) -> ShareStatusOutcome {
    for server_url in client.health().candidate_servers(server_urls, now_seconds) {
        if cancel() {
            return ShareStatusOutcome::Cancelled;
        }
        match client
            .share_status(&server_url, round_id, share_id, now_seconds, cancel)
            .await
        {
            Ok(ShareStatus::Confirmed) => {
                return ShareStatusOutcome::Confirmed;
            }
            // The helper is alive but has not revealed yet. Keep walking.
            Ok(ShareStatus::Pending) => {}
            Err(HelperError::Cancelled) => {
                return ShareStatusOutcome::Cancelled;
            }
            // Any transport, HTTP, or out-of-protocol failure was already
            // scored by the client.
            Err(_) => continue,
        }
    }
    ShareStatusOutcome::NotConfirmed
}

/// Fans one freshly built share out to helpers until enough accept it.
///
/// Helpers are drawn in health order, re-evaluated before **every** attempt so
/// a failure observed during this fan-out immediately demotes that helper for
/// the remaining picks. Each helper is tried at most once: a share is spread
/// across distinct helpers, never doubled up on one that already refused.
///
/// Every helper is first persisted as `attempting`; only then can its POST be
/// dispatched. Definite acknowledgements and unknown outcomes are promoted to
/// their final state, while definite pre-dispatch failures clear the marker.
/// A process death or failed outcome write therefore leaves a poll-only marker
/// and can never cause the same non-idempotent POST to be replayed.
pub async fn submit_share_to_helpers(
    db: &VotingDb,
    client: &HelperClient,
    params: &InitialShareSubmissionParams<'_>,
    cancel: &(dyn Fn() -> bool + Send + Sync),
) -> Result<ShareSubmissionReport, VotingError> {
    serde_json::from_str::<serde_json::Value>(params.share_wire_json).map_err(|e| {
        VotingError::InvalidInput {
            message: format!("invalid helper share JSON: {e}"),
        }
    })?;
    let empty = ShareSubmissionReport {
        target_count: params.target_count,
        ..ShareSubmissionReport::default()
    };
    share::record_delivery(
        db,
        &share::ShareDeliveryRecordParams {
            round_id: params.round_id,
            bundle_index: params.bundle_index,
            proposal_id: params.proposal_id,
            share_index: params.share_index,
            submission: &empty,
            submit_at: params.submit_at,
        },
    )?;
    let stored = share::list(db, params.round_id)?
        .into_iter()
        .find(|share| {
            share.bundle_index == params.bundle_index
                && share.proposal_id == params.proposal_id
                && share.share_index == params.share_index
        })
        .ok_or_else(|| VotingError::Internal {
            message: "newly journaled helper share was not found".to_string(),
        })?;
    let candidates: Vec<String> = dedupe_preserving_order(
        params
            .candidate_servers
            .iter()
            .filter_map(|url| canonicalize_helper_base_url(url).ok()),
    );
    let mut accepted = dedupe_preserving_order(
        stored
            .sent_to_urls
            .into_iter()
            .filter(|url| candidates.contains(url)),
    );
    let mut ambiguous = dedupe_preserving_order(
        stored
            .ambiguous_urls
            .into_iter()
            .chain(stored.attempting_urls.into_iter())
            .filter(|url| candidates.contains(url)),
    );
    let mut remaining = candidates;
    remaining.retain(|url| !accepted.contains(url) && !ambiguous.contains(url));
    let attempt_target = params
        .target_count
        .min(accepted.len().saturating_add(remaining.len()));
    let deadline = tokio::time::Instant::now()
        + Duration::from_millis(SHARE_INITIAL_DELIVERY_TIMEOUT_MILLISECONDS);

    while !remaining.is_empty() && accepted.len() < attempt_target {
        if cancel() {
            break;
        }
        let ordered = client
            .health()
            .candidate_servers(&remaining, params.now_seconds);
        let Some(server_url) = ordered.into_iter().next() else {
            break;
        };
        remaining.retain(|url| url != &server_url);

        let remaining_time = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining_time.is_zero() {
            break;
        }

        let attempt = share::ShareDeliveryAttemptParams {
            round_id: params.round_id,
            bundle_index: params.bundle_index,
            proposal_id: params.proposal_id,
            share_index: params.share_index,
            server_url: &server_url,
            target_count: params.target_count,
            submit_at: params.submit_at,
        };
        if !share::begin_delivery_attempt(db, &attempt)? {
            continue;
        }

        let submission = tokio::time::timeout_at(
            deadline,
            client.submit_share_with_timeout(
                &server_url,
                params.share_wire_json,
                params.now_seconds,
                cancel,
                remaining_time,
            ),
        )
        .await;
        match submission {
            Err(_) => {
                // Cancelling the POST future at the overall deadline leaves
                // its transport outcome unknown, so retain it for polling.
                share::resolve_delivery_attempt(
                    db,
                    &attempt,
                    share::ShareDeliveryAttemptOutcome::Ambiguous,
                    false,
                )?;
                ambiguous.push(server_url);
                break;
            }
            Ok(Ok(_)) => {
                share::resolve_delivery_attempt(
                    db,
                    &attempt,
                    share::ShareDeliveryAttemptOutcome::Accepted,
                    false,
                )?;
                accepted.push(server_url);
            }
            Ok(Err(error)) if error.is_ambiguous() => {
                share::resolve_delivery_attempt(
                    db,
                    &attempt,
                    share::ShareDeliveryAttemptOutcome::Ambiguous,
                    false,
                )?;
                ambiguous.push(server_url);
            }
            Ok(Err(_)) => share::resolve_delivery_attempt(
                db,
                &attempt,
                share::ShareDeliveryAttemptOutcome::DefiniteFailure,
                false,
            )?,
        }
    }
    Ok(ShareSubmissionReport {
        accepted_urls: accepted,
        ambiguous_urls: ambiguous,
        target_count: params.target_count,
    })
}

#[cfg(test)]
async fn submit_share_to_helpers_unrecorded(
    client: &HelperClient,
    share_wire_json: &str,
    candidate_servers: &[String],
    target_count: usize,
    now_seconds: u64,
    cancel: &(dyn Fn() -> bool + Send + Sync),
) -> ShareSubmissionReport {
    let mut accepted = Vec::new();
    let mut ambiguous = Vec::new();
    let mut remaining = dedupe_preserving_order(
        candidate_servers
            .iter()
            .filter_map(|url| canonicalize_helper_base_url(url).ok()),
    );
    let attempt_target = target_count.min(remaining.len());
    let deadline = tokio::time::Instant::now()
        + Duration::from_millis(SHARE_INITIAL_DELIVERY_TIMEOUT_MILLISECONDS);
    while !remaining.is_empty() && accepted.len() < attempt_target {
        if cancel() {
            break;
        }
        let ordered = client.health().candidate_servers(&remaining, now_seconds);
        let Some(server_url) = ordered.into_iter().next() else {
            break;
        };
        remaining.retain(|url| url != &server_url);
        let remaining_time = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining_time.is_zero() {
            break;
        }
        match tokio::time::timeout_at(
            deadline,
            client.submit_share_with_timeout(
                &server_url,
                share_wire_json,
                now_seconds,
                cancel,
                remaining_time,
            ),
        )
        .await
        {
            Err(_) => {
                ambiguous.push(server_url);
                break;
            }
            Ok(Ok(_)) => accepted.push(server_url),
            Ok(Err(error)) if error.is_ambiguous() => ambiguous.push(server_url),
            Ok(Err(_)) => {}
        }
    }
    ShareSubmissionReport {
        accepted_urls: accepted,
        ambiguous_urls: ambiguous,
        target_count,
    }
}

/// Runs one confirm-or-retry pass over a round's unconfirmed shares.
///
/// For each unconfirmed share, in persisted order:
///
/// 1. Compute [`ShareTrackingFlags`] and the configured definite placement.
/// 2. When ready, poll definite and ambiguous helpers for on-chain
///    confirmation. `pending` never proves helper possession, so ambiguous
///    attempts remain ambiguous.
/// 3. On confirmation, persist with [`share::confirm`] and move on.
/// 4. Before the vote-end cutoff, when overdue or below the desired placement,
///    walk a health-aware randomized resubmission order and durably retain each
///    attempt before contacting another helper. Early replenishment preserves
///    the persisted `submit_at`; overdue recovery uses zero so the replacement
///    helper acts immediately. Ambiguous helpers remain poll-only.
///
/// `cancel` is polled between every helper and every share. When it fires the
/// pass returns what it has already durably recorded with
/// [`ShareTrackingReport::cancelled`] set; nothing is rolled back, because
/// every effect recorded so far actually happened.
///
/// # Trust
///
/// A `confirmed` response from any configured helper is persisted without an
/// independent chain proof. Callers must use only helpers they trust as global
/// chain-state oracles; see the module-level trust model.
pub async fn track_pending_shares(
    db: &VotingDb,
    params: &ShareTrackingParams<'_>,
    client: &HelperClient,
    cancel: &(dyn Fn() -> bool + Send + Sync),
) -> Result<ShareTrackingReport, VotingError> {
    let started_at = Instant::now();
    track_pending_shares_with_elapsed(db, params, client, cancel, &|| {
        started_at.elapsed().as_secs()
    })
    .await
}

async fn track_pending_shares_with_elapsed(
    db: &VotingDb,
    params: &ShareTrackingParams<'_>,
    client: &HelperClient,
    cancel: &(dyn Fn() -> bool + Send + Sync),
    elapsed_seconds: &(dyn Fn() -> u64 + Send + Sync),
) -> Result<ShareTrackingReport, VotingError> {
    let mut report = ShareTrackingReport::default();
    let configured_urls = dedupe_preserving_order(
        params
            .configured_server_urls
            .iter()
            .filter_map(|url| canonicalize_helper_base_url(url).ok()),
    );
    let configured: HashSet<&str> = configured_urls.iter().map(String::as_str).collect();

    for share in share::unconfirmed(db, params.round_id)? {
        if cancel() {
            report.cancelled = true;
            break;
        }

        // Only configured helpers count toward current placement or polling.
        let mut accepted = dedupe_preserving_order(
            share
                .sent_to_urls
                .iter()
                .filter(|url| configured.contains(url.as_str()))
                .cloned(),
        );
        // An `attempting` marker left by an interrupted process is an unknown
        // POST outcome. Poll and exclude it exactly like explicit ambiguity.
        let mut ambiguous = dedupe_preserving_order(
            share
                .ambiguous_urls
                .iter()
                .chain(share.attempting_urls.iter())
                .filter(|url| configured.contains(url.as_str()))
                .filter(|url| !accepted.contains(url))
                .cloned(),
        );
        // Network failures that are definitely known not to have placed a
        // share are not durable state, but they must still be remembered for
        // this pass so filling a multi-helper deficit never contacts the same
        // failing endpoint again.
        let mut attempted_this_pass = Vec::new();
        let target_count = if share.target_count == 0 {
            share_submission_target_count(configured_urls.len())
        } else {
            usize::try_from(share.target_count).unwrap_or(usize::MAX)
        }
        .min(configured_urls.len());
        let flags = share_tracking_flags(
            &share,
            params.now_seconds,
            params.vote_end_time_seconds,
            params.policy,
        );
        if flags.is_idle() && accepted.len() >= target_count {
            continue;
        }

        let polled_urls = dedupe_preserving_order(accepted.iter().chain(ambiguous.iter()).cloned());
        if flags.ready_for_status_check && !polled_urls.is_empty() {
            let share_id = hex::encode(&share.nullifier);
            let poll = poll_share_helpers(
                client,
                params.round_id,
                &share_id,
                &polled_urls,
                params.now_seconds,
                cancel,
            )
            .await;
            match poll {
                ShareStatusOutcome::Cancelled => {
                    report.cancelled = true;
                    break;
                }
                ShareStatusOutcome::Confirmed => {
                    share::confirm(
                        db,
                        params.round_id,
                        share.bundle_index,
                        share.proposal_id,
                        share.share_index,
                    )?;
                    report.confirmed.push(ShareKey::of(&share));
                    continue;
                }
                ShareStatusOutcome::NotConfirmed => {}
            }
        }

        let resubmission_window_open = params.vote_end_time_seconds.is_none_or(|vote_end| {
            is_share_resubmission_window_open(params.now_seconds, vote_end, params.policy)
        });
        if resubmission_window_open && (flags.overdue_for_retry || accepted.len() < target_count) {
            let schedule = if flags.overdue_for_retry {
                ResubmissionSchedule::Immediate
            } else {
                ResubmissionSchedule::Preserve(share.submit_at)
            };
            loop {
                let resubmission = resubmit_to_next_helper(
                    db,
                    params,
                    client,
                    &ResubmitRequest {
                        share: &share,
                        accepted_urls: &accepted,
                        ambiguous_urls: &ambiguous,
                        schedule,
                    },
                    &mut attempted_this_pass,
                    cancel,
                    elapsed_seconds,
                )
                .await?;
                for server_url in resubmission.ambiguous_urls {
                    if !ambiguous.contains(&server_url) {
                        ambiguous.push(server_url.clone());
                    }
                    report.ambiguous.push(ResubmittedShare {
                        share: ShareKey::of(&share),
                        server_url,
                    });
                }
                match resubmission.outcome {
                    ResubmitOutcome::Accepted(server_url) => {
                        let is_new_placement = !accepted.contains(&server_url);
                        if is_new_placement {
                            accepted.push(server_url.clone());
                        }
                        report.resubmitted.push(ResubmittedShare {
                            share: ShareKey::of(&share),
                            server_url,
                        });
                        if accepted.len() >= target_count || !is_new_placement {
                            break;
                        }
                    }
                    ResubmitOutcome::Unrecoverable => {
                        report.unrecoverable.push(ShareKey::of(&share));
                        break;
                    }
                    ResubmitOutcome::AwaitingVcPosition
                    | ResubmitOutcome::NoHelperAccepted
                    | ResubmitOutcome::CutoffReached => break,
                    ResubmitOutcome::Cancelled => {
                        report.cancelled = true;
                        break;
                    }
                }
            }
            if report.cancelled {
                break;
            }
        }
    }

    // Recompute from storage rather than from the loop: confirmations recorded
    // above must not be counted as still pending.
    report.next_delay_seconds = next_tracking_delay_seconds(
        &share::unconfirmed(db, params.round_id)?,
        params.now_seconds,
        params.policy,
    );
    Ok(report)
}

enum ResubmitOutcome {
    Accepted(String),
    /// No helper in the order accepted the share this pass.
    NoHelperAccepted,
    /// The share's recovery material is missing, so no body can be built.
    Unrecoverable,
    /// Recovery exists but confirmation has not recorded the real VC position.
    AwaitingVcPosition,
    /// The vote-end recovery window closed during this tracking pass.
    CutoffReached,
    Cancelled,
}

struct ResubmitReport {
    outcome: ResubmitOutcome,
    ambiguous_urls: Vec<String>,
}

struct ResubmitRequest<'a> {
    share: &'a ShareDelegationRecord,
    accepted_urls: &'a [String],
    ambiguous_urls: &'a [String],
    schedule: ResubmissionSchedule,
}

#[derive(Clone, Copy)]
enum ResubmissionSchedule {
    Preserve(u64),
    Immediate,
}

impl ResubmissionSchedule {
    fn submit_at(self) -> u64 {
        match self {
            Self::Preserve(submit_at) => submit_at,
            Self::Immediate => 0,
        }
    }

    fn reset_submit_at(self) -> bool {
        matches!(self, Self::Immediate)
    }
}

/// Walks the randomized resubmission order until one helper accepts.
///
/// Untried helpers come before definitely accepted ones. Early replenishment
/// uses only untried helpers because another POST to an accepted helper cannot
/// add a placement; genuinely overdue recovery may fall back to accepted
/// helpers after exhausting untried ones. Ambiguous helpers are excluded
/// entirely: their prior POST remains poll-only until resolved.
///
/// Randomization is preserved within the untried and previously attempted
/// groups, while degraded helpers move behind healthy peers in their group.
/// Every POST is journaled before dispatch, and every accepted or ambiguous
/// outcome is persisted before returning or advancing to another helper.
async fn resubmit_to_next_helper(
    db: &VotingDb,
    params: &ShareTrackingParams<'_>,
    client: &HelperClient,
    request: &ResubmitRequest<'_>,
    attempted_urls: &mut Vec<String>,
    cancel: &(dyn Fn() -> bool + Send + Sync),
    elapsed_seconds: &(dyn Fn() -> u64 + Send + Sync),
) -> Result<ResubmitReport, VotingError> {
    let share = request.share;
    let bundle = match recovery::helper_recovery_material(
        db,
        params.round_id,
        share.bundle_index,
        share.proposal_id,
    )? {
        recovery::HelperRecoveryMaterial::Ready(bundle) => bundle,
        recovery::HelperRecoveryMaterial::AwaitingVcPosition => {
            return Ok(ResubmitReport {
                outcome: ResubmitOutcome::AwaitingVcPosition,
                ambiguous_urls: Vec::new(),
            });
        }
        recovery::HelperRecoveryMaterial::Missing => {
            return Ok(ResubmitReport {
                outcome: ResubmitOutcome::Unrecoverable,
                ambiguous_urls: Vec::new(),
            });
        }
    };

    let body = match share::recover_wire_json(
        &bundle.commitment_bundle_json,
        share.proposal_id,
        share.share_index,
        bundle.vc_tree_position,
        request.schedule.submit_at(),
    ) {
        Ok(body) => body,
        // Corrupt recovery material cannot be fixed by trying another helper.
        Err(_) => {
            return Ok(ResubmitReport {
                outcome: ResubmitOutcome::Unrecoverable,
                ambiguous_urls: Vec::new(),
            });
        }
    };

    let ambiguous: HashSet<&str> = request.ambiguous_urls.iter().map(String::as_str).collect();
    let accepted: HashSet<&str> = request.accepted_urls.iter().map(String::as_str).collect();
    let eligible_servers: Vec<String> = {
        let attempted: HashSet<&str> = attempted_urls.iter().map(String::as_str).collect();
        dedupe_preserving_order(
            params
                .configured_server_urls
                .iter()
                .filter_map(|url| canonicalize_helper_base_url(url).ok())
                .filter(|url| !ambiguous.contains(url.as_str()))
                .filter(|url| !attempted.contains(url.as_str()))
                .filter(|url| {
                    request.schedule.reset_submit_at() || !accepted.contains(url.as_str())
                }),
        )
    };
    let needed =
        resubmission_server_order_random_bytes_required(&eligible_servers, request.accepted_urls);
    let randomized = resubmission_server_order(
        &eligible_servers,
        request.accepted_urls,
        &(params.random_bytes)(needed),
    )?;
    let untried_count = randomized
        .iter()
        .take_while(|url| !accepted.contains(url.as_str()))
        .count();
    let (untried, previously_attempted) = randomized.split_at(untried_count);
    let mut order = client
        .health()
        .candidate_servers(untried, params.now_seconds);
    if request.schedule.reset_submit_at() {
        order.extend(
            client
                .health()
                .candidate_servers(previously_attempted, params.now_seconds),
        );
    }
    let mut ambiguous_urls = Vec::new();
    for server_url in order {
        if cancel() {
            return Ok(ResubmitReport {
                outcome: ResubmitOutcome::Cancelled,
                ambiguous_urls,
            });
        }
        let current_time = params.now_seconds.saturating_add(elapsed_seconds());
        if params.vote_end_time_seconds.is_some_and(|vote_end| {
            !is_share_resubmission_window_open(current_time, vote_end, params.policy)
        }) {
            return Ok(ResubmitReport {
                outcome: ResubmitOutcome::CutoffReached,
                ambiguous_urls,
            });
        }
        attempted_urls.push(server_url.clone());
        let attempt = share::ShareDeliveryAttemptParams {
            round_id: params.round_id,
            bundle_index: share.bundle_index,
            proposal_id: share.proposal_id,
            share_index: share.share_index,
            server_url: &server_url,
            target_count: usize::try_from(share.target_count).unwrap_or(usize::MAX),
            submit_at: request.schedule.submit_at(),
        };
        if !share::begin_existing_delivery_attempt(db, &attempt)? {
            continue;
        }
        match client
            .resubmit_share(&server_url, &body, current_time, cancel)
            .await
        {
            Ok(_) => {
                share::resolve_delivery_attempt(
                    db,
                    &attempt,
                    share::ShareDeliveryAttemptOutcome::Accepted,
                    request.schedule.reset_submit_at(),
                )?;
                return Ok(ResubmitReport {
                    outcome: ResubmitOutcome::Accepted(server_url),
                    ambiguous_urls,
                });
            }
            Err(error) if error.is_ambiguous() => {
                share::resolve_delivery_attempt(
                    db,
                    &attempt,
                    share::ShareDeliveryAttemptOutcome::Ambiguous,
                    request.schedule.reset_submit_at(),
                )?;
                ambiguous_urls.push(server_url);
            }
            Err(_) => share::resolve_delivery_attempt(
                db,
                &attempt,
                share::ShareDeliveryAttemptOutcome::DefiniteFailure,
                request.schedule.reset_submit_at(),
            )?,
        }
    }
    Ok(ResubmitReport {
        outcome: ResubmitOutcome::NoHelperAccepted,
        ambiguous_urls,
    })
}

fn dedupe_preserving_order(urls: impl Iterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut ordered = Vec::new();
    for url in urls {
        if seen.insert(url.clone()) {
            ordered.push(url);
        }
    }
    ordered
}

#[cfg(test)]
mod tests {
    use super::*;

    fn share_record(confirmed: bool, submit_at: u64) -> ShareDelegationRecord {
        ShareDelegationRecord {
            round_id: "ab".repeat(32),
            bundle_index: 0,
            proposal_id: 1,
            share_index: 0,
            sent_to_urls: Vec::new(),
            ambiguous_urls: Vec::new(),
            attempting_urls: Vec::new(),
            target_count: 0,
            nullifier: vec![0u8; 32],
            confirmed,
            submit_at,
            created_at: submit_at,
        }
    }

    #[test]
    fn confirmed_shares_are_never_ready_or_overdue() {
        let share = share_record(true, 100);
        let flags =
            share_tracking_flags(&share, 100_000, Some(200_000), ShareTimingPolicy::default());
        assert!(flags.is_idle());
    }

    #[test]
    fn missing_vote_end_suppresses_overdue_but_not_status_checks() {
        let share = share_record(false, 100);
        let flags = share_tracking_flags(&share, 100_000, None, ShareTimingPolicy::default());
        assert!(flags.ready_for_status_check);
        assert!(!flags.overdue_for_retry);
    }

    #[test]
    fn young_share_is_idle_until_the_status_grace_passes() {
        let share = share_record(false, 1_000);
        let policy = ShareTimingPolicy::default();
        let just_before = 1_000 + policy.status_check_grace_seconds - 1;
        assert!(share_tracking_flags(&share, just_before, Some(500_000), policy).is_idle());

        let at_grace = 1_000 + policy.status_check_grace_seconds;
        assert!(
            share_tracking_flags(&share, at_grace, Some(500_000), policy).ready_for_status_check
        );
    }

    // ---- Mock transport -------------------------------------------------

    use std::collections::{HashMap, VecDeque};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use crate::helper::{
        client::{HelperClient, HelperClientConfig},
        health::HelperHealth,
        transport::{
            HelperFuture, HelperResponse, HelperTransport, HelperTransportError,
            MAX_HELPER_RESPONSE_BYTES,
        },
    };

    type Reply = Result<HelperResponse, HelperTransportError>;

    /// Canned per-URL responses plus a call log.
    ///
    /// Missing canned responses fail rather than defaulting, so a test that
    /// contacts an unexpected helper fails loudly instead of passing silently.
    #[derive(Default)]
    struct MockTransport {
        gets: Mutex<HashMap<String, VecDeque<Reply>>>,
        posts: Mutex<HashMap<String, VecDeque<Reply>>>,
        calls: Mutex<Vec<String>>,
        timeouts: Mutex<Vec<(String, Duration)>>,
        post_bodies: Mutex<Vec<(String, Vec<u8>)>>,
        post_delays: Mutex<HashMap<String, VecDeque<Duration>>>,
        post_observer: Mutex<Option<Arc<dyn Fn(&str) + Send + Sync>>>,
    }

    impl MockTransport {
        fn queue_get(&self, url: &str, reply: Reply) {
            self.gets
                .lock()
                .unwrap()
                .entry(url.to_string())
                .or_default()
                .push_back(reply);
        }

        fn queue_post(&self, url: &str, reply: Reply) {
            self.posts
                .lock()
                .unwrap()
                .entry(url.to_string())
                .or_default()
                .push_back(reply);
        }

        fn queue_post_after(&self, url: &str, delay: Duration, reply: Reply) {
            self.queue_post(url, reply);
            self.post_delays
                .lock()
                .unwrap()
                .entry(url.to_string())
                .or_default()
                .push_back(delay);
        }

        fn observe_posts(&self, observer: impl Fn(&str) + Send + Sync + 'static) {
            *self.post_observer.lock().unwrap() = Some(Arc::new(observer));
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        fn call_count(&self, needle: &str) -> usize {
            self.calls()
                .iter()
                .filter(|call| call.contains(needle))
                .count()
        }

        fn posted_submit_at(&self, url: &str) -> u64 {
            let bodies = self.post_bodies.lock().unwrap();
            let (_, body) = bodies
                .iter()
                .find(|(posted_url, _)| posted_url == url)
                .unwrap_or_else(|| panic!("no POST body recorded for {url}"));
            serde_json::from_slice::<serde_json::Value>(body).unwrap()["submit_at"]
                .as_u64()
                .unwrap()
        }

        fn timeout_for(&self, url: &str) -> Duration {
            self.timeouts
                .lock()
                .unwrap()
                .iter()
                .find(|(requested_url, _)| requested_url == url)
                .map(|(_, timeout)| *timeout)
                .unwrap_or_else(|| panic!("no request recorded for {url}"))
        }

        fn take(
            &self,
            table: &Mutex<HashMap<String, VecDeque<Reply>>>,
            method: &str,
            url: &str,
        ) -> Reply {
            self.calls.lock().unwrap().push(format!("{method} {url}"));
            table
                .lock()
                .unwrap()
                .get_mut(url)
                .and_then(VecDeque::pop_front)
                .unwrap_or_else(|| {
                    Err(HelperTransportError::Transport(format!(
                        "no canned {method} response for {url}"
                    )))
                })
        }
    }

    impl HelperTransport for MockTransport {
        fn get<'a>(&'a self, url: &'a str, timeout: Duration) -> HelperFuture<'a> {
            self.timeouts
                .lock()
                .unwrap()
                .push((url.to_string(), timeout));
            let reply = self.take(&self.gets, "GET", url);
            Box::pin(async move { reply })
        }

        fn post_json<'a>(
            &'a self,
            url: &'a str,
            body: Vec<u8>,
            timeout: Duration,
        ) -> HelperFuture<'a> {
            self.timeouts
                .lock()
                .unwrap()
                .push((url.to_string(), timeout));
            self.post_bodies
                .lock()
                .unwrap()
                .push((url.to_string(), body));
            if let Some(observer) = self.post_observer.lock().unwrap().as_ref() {
                observer(url);
            }
            let delay = self
                .post_delays
                .lock()
                .unwrap()
                .get_mut(url)
                .and_then(VecDeque::pop_front)
                .unwrap_or_default();
            let reply = self.take(&self.posts, "POST", url);
            Box::pin(async move {
                tokio::time::sleep(delay).await;
                reply
            })
        }
    }

    fn json_status(status: &str) -> Reply {
        Ok(HelperResponse {
            status: 200,
            body: format!(r#"{{"status":"{status}"}}"#).into_bytes(),
        })
    }

    fn http_status(status: u16) -> Reply {
        Ok(HelperResponse {
            status,
            body: b"{}".to_vec(),
        })
    }

    fn helper(index: usize) -> String {
        format!("https://helper-{index}.example")
    }

    fn helpers(count: usize) -> Vec<String> {
        (1..=count).map(helper).collect()
    }

    fn client_with(transport: Arc<MockTransport>) -> HelperClient {
        HelperClient::new(transport, HelperHealth::default())
    }

    fn never_cancel() -> impl Fn() -> bool {
        || false
    }

    // ---- Confirmation policy --------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn first_confirmed_trusted_helper_stops_status_checks() {
        let round_id = "ab".repeat(32);
        let share_id = "cd".repeat(32);
        let transport = Arc::new(MockTransport::default());
        let status_url = |index: usize| {
            format!(
                "{}/shielded-vote/v1/share-status/{round_id}/{share_id}",
                helper(index)
            )
        };

        transport.queue_get(&status_url(1), json_status("pending"));
        transport.queue_get(&status_url(2), json_status("confirmed"));

        let client = client_with(transport.clone());
        let outcome = confirm_share_by_any_helper(
            &client,
            &round_id,
            &share_id,
            &helpers(5),
            1_000,
            &never_cancel(),
        )
        .await;

        assert_eq!(outcome, ShareStatusOutcome::Confirmed);
        // Helpers 3-5 are never contacted once helper 2 confirms.
        assert_eq!(transport.calls().len(), 2);
        for index in 3..=5 {
            assert_eq!(transport.call_count(&helper(index)), 0);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn every_helper_pending_reports_not_confirmed() {
        let round_id = "ab".repeat(32);
        let share_id = "cd".repeat(32);
        let transport = Arc::new(MockTransport::default());
        for index in 1..=3 {
            transport.queue_get(
                &format!(
                    "{}/shielded-vote/v1/share-status/{round_id}/{share_id}",
                    helper(index)
                ),
                json_status("pending"),
            );
        }

        let client = client_with(transport.clone());
        let outcome = confirm_share_by_any_helper(
            &client,
            &round_id,
            &share_id,
            &helpers(3),
            1_000,
            &never_cancel(),
        )
        .await;

        assert_eq!(outcome, ShareStatusOutcome::NotConfirmed);
        assert_eq!(transport.calls().len(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_between_helpers_stops_without_confirming() {
        let round_id = "ab".repeat(32);
        let share_id = "cd".repeat(32);
        let transport = Arc::new(MockTransport::default());
        transport.queue_get(
            &format!(
                "{}/shielded-vote/v1/share-status/{round_id}/{share_id}",
                helper(1)
            ),
            json_status("pending"),
        );
        transport.queue_get(
            &format!(
                "{}/shielded-vote/v1/share-status/{round_id}/{share_id}",
                helper(2)
            ),
            json_status("confirmed"),
        );

        let calls = Arc::new(Mutex::new(0usize));
        let cancel_after_first = {
            let calls = calls.clone();
            move || {
                let mut calls = calls.lock().unwrap();
                *calls += 1;
                // False for the first helper's pre-check, true afterwards.
                *calls > 2
            }
        };

        let client = client_with(transport.clone());
        let outcome = confirm_share_by_any_helper(
            &client,
            &round_id,
            &share_id,
            &helpers(2),
            1_000,
            &cancel_after_first,
        )
        .await;

        assert_eq!(outcome, ShareStatusOutcome::Cancelled);
        assert_eq!(transport.call_count(&helper(2)), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_is_not_scored_against_a_helper() {
        let transport = Arc::new(MockTransport::default());
        let client = client_with(transport.clone());
        let always_cancel = || true;

        let result = client
            .share_status(
                &helper(1),
                &"ab".repeat(32),
                &"cd".repeat(32),
                10,
                &always_cancel,
            )
            .await;

        assert!(matches!(result, Err(HelperError::Cancelled)));
        assert_eq!(client.health().failure_count(&helper(1)), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn helper_defaults_use_distinct_status_and_post_deadlines() {
        let transport = Arc::new(MockTransport::default());
        let status_url = format!(
            "{}/shielded-vote/v1/share-status/{}/{}",
            helper(1),
            "ab".repeat(32),
            "cd".repeat(32)
        );
        let post_url = format!("{}/shielded-vote/v1/shares", helper(1));
        transport.queue_get(&status_url, json_status("pending"));
        transport.queue_post(&post_url, json_status("queued"));

        let client = client_with(transport.clone());
        client
            .share_status(
                &helper(1),
                &"ab".repeat(32),
                &"cd".repeat(32),
                10,
                &never_cancel(),
            )
            .await
            .unwrap();
        client
            .submit_share(&helper(1), r#"{"share_index":0}"#, 10, &never_cancel())
            .await
            .unwrap();

        assert_eq!(transport.timeout_for(&status_url), Duration::from_secs(5));
        assert_eq!(transport.timeout_for(&post_url), Duration::from_secs(30));
    }

    #[tokio::test(start_paused = true)]
    async fn explicit_post_deadline_is_honored() {
        let transport = Arc::new(MockTransport::default());
        let post_url = format!("{}/shielded-vote/v1/shares", helper(1));
        transport.queue_post(&post_url, json_status("queued"));
        let config = HelperClientConfig {
            post_timeout: Duration::from_secs(47),
            ..HelperClientConfig::default()
        };
        let client = HelperClient::with_config(transport.clone(), HelperHealth::default(), config);

        client
            .resubmit_share(&helper(1), r#"{"share_index":0}"#, 10, &never_cancel())
            .await
            .unwrap();

        assert_eq!(transport.timeout_for(&post_url), Duration::from_secs(47));
    }

    // ---- Retry rules -----------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn ambiguous_timeout_is_not_retried_on_the_same_helper() {
        let transport = Arc::new(MockTransport::default());
        let url = format!("{}/shielded-vote/v1/shares", helper(1));
        transport.queue_post(&url, Err(HelperTransportError::Timeout));
        transport.queue_post(&url, json_status("queued"));

        let client = client_with(transport.clone());
        let result = client
            .submit_share(&helper(1), r#"{"share_index":0}"#, 10, &never_cancel())
            .await;

        assert!(result.is_err());
        // One attempt only: the share may already be queued on this helper.
        assert_eq!(transport.call_count(&url), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn response_body_failure_is_not_retried_on_the_same_helper() {
        let transport = Arc::new(MockTransport::default());
        let url = format!("{}/shielded-vote/v1/shares", helper(1));
        transport.queue_post(
            &url,
            Err(HelperTransportError::Response(
                "response body ended early".to_string(),
            )),
        );
        transport.queue_post(&url, json_status("queued"));

        let client = client_with(transport.clone());
        let result = client
            .submit_share(&helper(1), r#"{"share_index":0}"#, 10, &never_cancel())
            .await;

        assert!(matches!(
            result,
            Err(HelperError::Transport(HelperTransportError::Response(_)))
        ));
        // Headers prove the helper received the POST, so another attempt could
        // duplicate a share that was accepted before the body read failed.
        assert_eq!(transport.call_count(&url), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn unusable_successful_post_response_is_ambiguous_and_not_retried() {
        let cases = [
            b"not json".to_vec(),
            br#"{}"#.to_vec(),
            br#"{"status":"accepted"}"#.to_vec(),
            vec![b' '; MAX_HELPER_RESPONSE_BYTES + 1],
        ];

        for (index, body) in cases.into_iter().enumerate() {
            let transport = Arc::new(MockTransport::default());
            let server_url = helper(index + 1);
            let url = format!("{server_url}/shielded-vote/v1/shares");
            transport.queue_post(&url, Ok(HelperResponse { status: 200, body }));
            transport.queue_post(&url, json_status("queued"));

            let client = client_with(transport.clone());
            let error = client
                .submit_share(&server_url, r#"{"share_index":0}"#, 10, &never_cancel())
                .await
                .unwrap_err();

            assert!(matches!(
                &error,
                HelperError::AmbiguousSubmissionResponse { .. }
            ));
            assert!(error.is_ambiguous());
            assert_eq!(transport.call_count(&url), 1);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn server_error_submission_is_ambiguous_and_not_retried() {
        let transport = Arc::new(MockTransport::default());
        let url = format!("{}/shielded-vote/v1/shares", helper(1));
        transport.queue_post(&url, http_status(503));
        transport.queue_post(&url, json_status("queued"));

        let client = client_with(transport.clone());
        let error = client
            .submit_share(&helper(1), r#"{"share_index":0}"#, 10, &never_cancel())
            .await
            .unwrap_err();

        assert!(matches!(error, HelperError::Status { status: 503, .. }));
        assert!(error.is_ambiguous());
        assert_eq!(transport.call_count(&url), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn resubmit_makes_exactly_one_attempt() {
        let transport = Arc::new(MockTransport::default());
        let url = format!("{}/shielded-vote/v1/shares", helper(1));
        transport.queue_post(&url, http_status(503));
        transport.queue_post(&url, json_status("queued"));

        let client = client_with(transport.clone());
        let result = client
            .resubmit_share(&helper(1), r#"{"share_index":0}"#, 10, &never_cancel())
            .await;

        assert!(result.is_err());
        assert_eq!(transport.call_count(&url), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn malformed_share_body_is_rejected_before_any_request() {
        let transport = Arc::new(MockTransport::default());
        let client = client_with(transport.clone());

        let error = client
            .submit_share(&helper(1), "not json", 10, &never_cancel())
            .await
            .unwrap_err();

        assert!(matches!(&error, HelperError::Decode { .. }));
        assert!(!error.is_ambiguous());
        assert!(transport.calls().is_empty());
    }

    // ---- Initial fan-out -------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn fan_out_stops_at_the_target_count() {
        let transport = Arc::new(MockTransport::default());
        for index in 1..=5 {
            transport.queue_post(
                &format!("{}/shielded-vote/v1/shares", helper(index)),
                json_status("queued"),
            );
        }

        let client = client_with(transport.clone());
        let report = submit_share_to_helpers_unrecorded(
            &client,
            r#"{"share_index":0}"#,
            &helpers(5),
            3,
            10,
            &never_cancel(),
        )
        .await;

        assert_eq!(report.accepted_urls, vec![helper(1), helper(2), helper(3)]);
        assert!(report.ambiguous_urls.is_empty());
        assert_eq!(report.target_count, 3);
        assert_eq!(transport.calls().len(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn fan_out_moves_past_a_refusing_helper() {
        let transport = Arc::new(MockTransport::default());
        transport.queue_post(
            &format!("{}/shielded-vote/v1/shares", helper(1)),
            http_status(400),
        );
        transport.queue_post(
            &format!("{}/shielded-vote/v1/shares", helper(2)),
            json_status("queued"),
        );

        let client = client_with(transport.clone());
        let report = submit_share_to_helpers_unrecorded(
            &client,
            r#"{"share_index":0}"#,
            &helpers(3),
            1,
            10,
            &never_cancel(),
        )
        .await;

        assert_eq!(report.accepted_urls, vec![helper(2)]);
        assert_eq!(client.health().failure_count(&helper(1)), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn fan_out_never_retries_the_same_helper() {
        let transport = Arc::new(MockTransport::default());
        transport.queue_post(
            &format!("{}/shielded-vote/v1/shares", helper(1)),
            http_status(400),
        );

        let client = client_with(transport.clone());
        let report = submit_share_to_helpers_unrecorded(
            &client,
            r#"{"share_index":0}"#,
            &[helper(1)],
            3,
            10,
            &never_cancel(),
        )
        .await;

        assert!(report.accepted_urls.is_empty());
        // One attempt, then the candidate pool is exhausted — no spinning.
        assert_eq!(transport.call_count(&helper(1)), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn fan_out_returns_partial_acceptance_rather_than_failing() {
        let transport = Arc::new(MockTransport::default());
        transport.queue_post(
            &format!("{}/shielded-vote/v1/shares", helper(1)),
            json_status("queued"),
        );
        transport.queue_post(
            &format!("{}/shielded-vote/v1/shares", helper(2)),
            http_status(400),
        );

        let client = client_with(transport.clone());
        let report = submit_share_to_helpers_unrecorded(
            &client,
            r#"{"share_index":0}"#,
            &helpers(2),
            2,
            10,
            &never_cancel(),
        )
        .await;

        // Under-placed, not lost: tracking spreads it further later.
        assert_eq!(report.accepted_urls, vec![helper(1)]);
    }

    #[tokio::test(start_paused = true)]
    async fn fan_out_retains_ambiguous_attempts_separately() {
        let transport = Arc::new(MockTransport::default());
        transport.queue_post(
            &format!("{}/shielded-vote/v1/shares", helper(1)),
            Err(HelperTransportError::Ambiguous(
                "connection closed before headers".to_string(),
            )),
        );
        transport.queue_post(
            &format!("{}/shielded-vote/v1/shares", helper(2)),
            json_status("queued"),
        );

        let client = client_with(transport.clone());
        let report = submit_share_to_helpers_unrecorded(
            &client,
            r#"{"share_index":0}"#,
            &helpers(2),
            1,
            10,
            &never_cancel(),
        )
        .await;

        assert_eq!(report.accepted_urls, vec![helper(2)]);
        assert_eq!(report.ambiguous_urls, vec![helper(1)]);
        assert_eq!(transport.call_count(&helper(1)), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn fan_out_retains_unusable_successful_response_as_ambiguous() {
        let transport = Arc::new(MockTransport::default());
        let first_url = format!("{}/shielded-vote/v1/shares", helper(1));
        transport.queue_post(
            &first_url,
            Ok(HelperResponse {
                status: 200,
                body: br#"{}"#.to_vec(),
            }),
        );
        transport.queue_post(&first_url, json_status("queued"));
        transport.queue_post(
            &format!("{}/shielded-vote/v1/shares", helper(2)),
            json_status("queued"),
        );

        let client = client_with(transport.clone());
        let report = submit_share_to_helpers_unrecorded(
            &client,
            r#"{"share_index":0}"#,
            &helpers(2),
            1,
            10,
            &never_cancel(),
        )
        .await;

        assert_eq!(report.accepted_urls, vec![helper(2)]);
        assert_eq!(report.ambiguous_urls, vec![helper(1)]);
        assert_eq!(transport.call_count(&first_url), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn fan_out_retains_server_error_as_ambiguous_without_retrying() {
        let transport = Arc::new(MockTransport::default());
        let first_url = format!("{}/shielded-vote/v1/shares", helper(1));
        transport.queue_post(&first_url, http_status(503));
        transport.queue_post(&first_url, json_status("queued"));
        transport.queue_post(
            &format!("{}/shielded-vote/v1/shares", helper(2)),
            json_status("queued"),
        );

        let client = client_with(transport.clone());
        let report = submit_share_to_helpers_unrecorded(
            &client,
            r#"{"share_index":0}"#,
            &helpers(2),
            1,
            10,
            &never_cancel(),
        )
        .await;

        assert_eq!(report.accepted_urls, vec![helper(2)]);
        assert_eq!(report.ambiguous_urls, vec![helper(1)]);
        assert_eq!(transport.call_count(&first_url), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn fan_out_stops_at_the_overall_deadline_and_clamps_the_last_request() {
        let transport = Arc::new(MockTransport::default());
        let first_url = format!("{}/shielded-vote/v1/shares", helper(1));
        let second_url = format!("{}/shielded-vote/v1/shares", helper(2));
        transport.queue_post_after(&first_url, Duration::from_secs(50), json_status("queued"));
        transport.queue_post_after(&second_url, Duration::from_secs(20), json_status("queued"));
        transport.queue_post(
            &format!("{}/shielded-vote/v1/shares", helper(3)),
            json_status("queued"),
        );
        let config = HelperClientConfig {
            post_timeout: Duration::from_secs(90),
            retry_delays: Vec::new(),
            ..HelperClientConfig::default()
        };
        let client = HelperClient::with_config(transport.clone(), HelperHealth::default(), config);
        let started = tokio::time::Instant::now();

        let report = submit_share_to_helpers_unrecorded(
            &client,
            r#"{"share_index":0}"#,
            &helpers(3),
            3,
            10,
            &never_cancel(),
        )
        .await;

        assert_eq!(
            started.elapsed(),
            Duration::from_millis(SHARE_INITIAL_DELIVERY_TIMEOUT_MILLISECONDS)
        );
        assert_eq!(report.accepted_urls, vec![helper(1)]);
        assert_eq!(report.ambiguous_urls, vec![helper(2)]);
        assert_eq!(transport.timeout_for(&first_url), Duration::from_secs(60));
        assert_eq!(transport.timeout_for(&second_url), Duration::from_secs(10));
        assert_eq!(transport.call_count(&helper(3)), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn fan_out_canonicalizes_candidates_without_shrinking_the_target() {
        let transport = Arc::new(MockTransport::default());
        transport.queue_post(
            &format!("{}/shielded-vote/v1/shares", helper(1)),
            json_status("queued"),
        );

        let client = client_with(transport.clone());
        let report = submit_share_to_helpers_unrecorded(
            &client,
            r#"{"share_index":0}"#,
            &[helper(1), format!("{}/", helper(1))],
            3,
            10,
            &never_cancel(),
        )
        .await;

        assert_eq!(report.accepted_urls, vec![helper(1)]);
        assert_eq!(report.target_count, 3);
        assert_eq!(transport.call_count(&helper(1)), 1);
    }

    // ---- Durable end-to-end passes ---------------------------------------

    use crate::{
        round::RoundParams,
        storage::queries,
        types::{EncryptedShare, NoteInfo},
        vote::{serialize_recovery, VoteRecoveryBundle},
    };

    const ROUND_ID: &str = "0101010101010101010101010101010101010101010101010101010101010101";
    const WALLET_ID: &str = "wallet";
    /// Base submission time every timing fixture is anchored to.
    const SUBMIT_AT: u64 = 1_000;
    /// Far enough out that the overdue threshold clamps to its maximum.
    const VOTE_END: u64 = 1_000_000;

    fn field_bytes(value: u8) -> [u8; 32] {
        let mut bytes = [0u8; 32];
        bytes[0] = value;
        bytes
    }

    fn note(position: u64) -> NoteInfo {
        NoteInfo {
            commitment: vec![0x01; 32],
            nullifier: vec![0x02; 32],
            value: crate::governance::BALLOT_DIVISOR,
            position,
            diversifier: vec![0x03; 11],
            rho: vec![0x04; 32],
            rseed: vec![0x05; 32],
            scope: 0,
            ufvk_str: "uview1test".to_string(),
        }
    }

    fn recovery_bundle_fixture() -> VoteRecoveryBundle {
        VoteRecoveryBundle {
            vote_round_id: ROUND_ID.to_string(),
            bundle_index: 0,
            proposal_id: 1,
            vote_decision: 2,
            anchor_height: 123,
            vc_tree_position: 456,
            single_share: false,
            num_options: 3,
            van_nullifier: [0x10; 32],
            vote_authority_note_new: [0x11; 32],
            vote_commitment: [0x12; 32],
            proof: vec![0x13; 96],
            shares_hash: [0x14; 32],
            r_vpk: [0x15; 32],
            alpha_v: [0x16; 32],
            vote_auth_sig: [0x17; 64],
            encrypted_shares: vec![
                EncryptedShare {
                    c1: vec![0x21; 32],
                    c2: vec![0x22; 32],
                    share_index: 0,
                    plaintext_value: 5,
                    randomness: vec![0x23; 32],
                },
                EncryptedShare {
                    c1: vec![0x31; 32],
                    c2: vec![0x32; 32],
                    share_index: 1,
                    plaintext_value: 6,
                    randomness: vec![0x33; 32],
                },
            ],
            share_blinds: vec![field_bytes(1), field_bytes(2)],
            share_comms: vec![[0x51; 32], [0x52; 32]],
        }
    }

    /// Builds a round holding one recoverable vote and one recorded share.
    fn db_with_share(sent_to_urls: &[String]) -> VotingDb {
        db_with_delivery(sent_to_urls, &[], sent_to_urls.len())
    }

    fn db_with_delivery(
        sent_to_urls: &[String],
        ambiguous_urls: &[String],
        target_count: usize,
    ) -> VotingDb {
        let db = VotingDb::open_in_memory().unwrap();
        db.set_wallet_id(WALLET_ID);
        db.create_round(
            crate::Network::Testnet,
            &RoundParams {
                vote_round_id: ROUND_ID.to_string(),
                snapshot_height: 1000,
                ea_pk: vec![0xEA; 32],
                nc_root: vec![0xAA; 32],
                nullifier_imt_root: vec![0xBB; 32],
            },
            None,
        )
        .unwrap();
        db.ensure_bundles(ROUND_ID, &[note(0)]).unwrap();
        queries::store_vote(&db.conn(), ROUND_ID, WALLET_ID, 0, 1, 2, &[0xCA; 32]).unwrap();
        let json = serialize_recovery(&recovery_bundle_fixture()).unwrap();
        db.conn()
            .execute(
                "UPDATE votes SET commitment_bundle_json = :json, vc_tree_position = :pos
                 WHERE round_id = :round_id AND wallet_id = :wallet_id
                   AND bundle_index = 0 AND proposal_id = 1",
                rusqlite::named_params! {
                    ":json": json,
                    ":pos": 456i64,
                    ":round_id": ROUND_ID,
                    ":wallet_id": WALLET_ID,
                },
            )
            .unwrap();
        let submission = ShareSubmissionReport {
            accepted_urls: sent_to_urls.to_vec(),
            ambiguous_urls: ambiguous_urls.to_vec(),
            target_count,
        };
        share::record_delivery(
            &db,
            &share::ShareDeliveryRecordParams {
                round_id: ROUND_ID,
                bundle_index: 0,
                proposal_id: 1,
                share_index: 0,
                submission: &submission,
                submit_at: SUBMIT_AT,
            },
        )
        .unwrap();
        db
    }

    fn only_share(db: &VotingDb) -> ShareDelegationRecord {
        share::list(db, ROUND_ID)
            .unwrap()
            .into_iter()
            .next()
            .unwrap()
    }

    fn initial_submission<'a>(servers: &'a [String]) -> InitialShareSubmissionParams<'a> {
        InitialShareSubmissionParams {
            round_id: ROUND_ID,
            bundle_index: 0,
            proposal_id: 1,
            share_index: 0,
            share_wire_json: r#"{"share_index":0}"#,
            candidate_servers: servers,
            target_count: 1,
            submit_at: SUBMIT_AT,
            now_seconds: SUBMIT_AT + 1,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn initial_post_is_journaled_before_transport_dispatch() {
        let db = Arc::new(db_with_delivery(&[], &[], 1));
        let transport = Arc::new(MockTransport::default());
        let post_url = format!("{}/shielded-vote/v1/shares", helper(1));
        transport.queue_post(&post_url, json_status("queued"));
        let observed_db = db.clone();
        transport.observe_posts(move |_| {
            let stored = only_share(&observed_db);
            assert_eq!(stored.attempting_urls, vec![helper(1)]);
            assert!(stored.sent_to_urls.is_empty());
        });
        let client = client_with(transport);
        let servers = vec![helper(1)];

        let report =
            submit_share_to_helpers(&db, &client, &initial_submission(&servers), &never_cancel())
                .await
                .unwrap();

        assert_eq!(report.accepted_urls, vec![helper(1)]);
        let stored = only_share(&db);
        assert_eq!(stored.sent_to_urls, vec![helper(1)]);
        assert!(stored.attempting_urls.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn definite_initial_failure_clears_attempt_and_remains_retryable() {
        let db = db_with_delivery(&[], &[], 1);
        let transport = Arc::new(MockTransport::default());
        let post_url = format!("{}/shielded-vote/v1/shares", helper(1));
        transport.queue_post(
            &post_url,
            Err(HelperTransportError::Transport(
                "connect failed".to_string(),
            )),
        );
        transport.queue_post(&post_url, json_status("queued"));
        let client = HelperClient::with_config(
            transport.clone(),
            HelperHealth::default(),
            HelperClientConfig {
                retry_delays: Vec::new(),
                ..HelperClientConfig::default()
            },
        );
        let servers = vec![helper(1)];

        let first =
            submit_share_to_helpers(&db, &client, &initial_submission(&servers), &never_cancel())
                .await
                .unwrap();
        assert!(first.accepted_urls.is_empty());
        assert!(only_share(&db).attempting_urls.is_empty());

        let second =
            submit_share_to_helpers(&db, &client, &initial_submission(&servers), &never_cancel())
                .await
                .unwrap();
        assert_eq!(second.accepted_urls, vec![helper(1)]);
        assert_eq!(transport.call_count(&post_url), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn ambiguous_initial_failure_is_persisted_and_never_replayed() {
        let db = db_with_delivery(&[], &[], 1);
        let transport = Arc::new(MockTransport::default());
        let post_url = format!("{}/shielded-vote/v1/shares", helper(1));
        transport.queue_post(
            &post_url,
            Err(HelperTransportError::Ambiguous(
                "request timeout".to_string(),
            )),
        );
        transport.queue_post(&post_url, json_status("queued"));
        let client = client_with(transport.clone());
        let servers = vec![helper(1)];

        submit_share_to_helpers(&db, &client, &initial_submission(&servers), &never_cancel())
            .await
            .unwrap();
        let stored = only_share(&db);
        assert_eq!(stored.ambiguous_urls, vec![helper(1)]);
        assert!(stored.attempting_urls.is_empty());

        submit_share_to_helpers(&db, &client, &initial_submission(&servers), &never_cancel())
            .await
            .unwrap();
        assert_eq!(transport.call_count(&post_url), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn failed_outcome_write_leaves_attempting_marker() {
        let db = Arc::new(db_with_delivery(&[], &[], 1));
        let transport = Arc::new(MockTransport::default());
        let post_url = format!("{}/shielded-vote/v1/shares", helper(1));
        transport.queue_post(&post_url, json_status("queued"));
        let trigger_db = db.clone();
        transport.observe_posts(move |_| {
            trigger_db
                .conn()
                .execute_batch(
                    "CREATE TRIGGER fail_delivery_promotion
                     BEFORE UPDATE OF sent_to_urls ON share_delegations
                     BEGIN SELECT RAISE(FAIL, 'injected promotion failure'); END;",
                )
                .unwrap();
        });
        let client = client_with(transport.clone());
        let servers = vec![helper(1)];

        let result =
            submit_share_to_helpers(&db, &client, &initial_submission(&servers), &never_cancel())
                .await;

        assert!(result.is_err());
        let stored = only_share(&db);
        assert_eq!(stored.attempting_urls, vec![helper(1)]);
        assert!(stored.sent_to_urls.is_empty());

        db.conn()
            .execute_batch("DROP TRIGGER fail_delivery_promotion")
            .unwrap();
        transport.queue_post(&post_url, json_status("queued"));
        submit_share_to_helpers(&db, &client, &initial_submission(&servers), &never_cancel())
            .await
            .unwrap();
        assert_eq!(transport.call_count(&post_url), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn failed_attempt_write_prevents_network_dispatch() {
        let db = db_with_delivery(&[], &[], 1);
        db.conn()
            .execute_batch(
                "CREATE TRIGGER fail_attempt_write
                 BEFORE UPDATE OF attempting_urls ON share_delegations
                 BEGIN SELECT RAISE(FAIL, 'injected attempt failure'); END;",
            )
            .unwrap();
        let transport = Arc::new(MockTransport::default());
        let client = client_with(transport.clone());
        let servers = vec![helper(1)];

        let result =
            submit_share_to_helpers(&db, &client, &initial_submission(&servers), &never_cancel())
                .await;

        assert!(result.is_err());
        assert!(transport.calls().is_empty());
    }

    fn share_id_of(db: &VotingDb) -> String {
        hex::encode(only_share(db).nullifier)
    }

    fn zero_bytes(len: usize) -> Vec<u8> {
        vec![0u8; len]
    }

    fn preserve_server_order(len: usize) -> Vec<u8> {
        let shuffle_steps = len / std::mem::size_of::<u64>();
        (1..=shuffle_steps)
            .rev()
            .flat_map(|index| (index as u64).to_le_bytes())
            .collect()
    }

    fn preserve_two_server_order(len: usize) -> Vec<u8> {
        let mut bytes = vec![0u8; len];
        if !bytes.is_empty() {
            bytes[0] = 1;
        }
        bytes
    }

    fn params<'a>(
        configured: &'a [String],
        now_seconds: u64,
        random_bytes: &'a (dyn Fn(usize) -> Vec<u8> + Send + Sync),
    ) -> ShareTrackingParams<'a> {
        ShareTrackingParams {
            round_id: ROUND_ID,
            configured_server_urls: configured,
            now_seconds,
            vote_end_time_seconds: Some(VOTE_END),
            policy: ShareTimingPolicy::default(),
            random_bytes,
        }
    }

    /// Ready for a status check but not yet overdue.
    fn ready_not_overdue() -> u64 {
        SUBMIT_AT + ShareTimingPolicy::default().status_check_grace_seconds + 1
    }

    /// Past the (clamped) overdue threshold, so retry is also armed.
    fn overdue() -> u64 {
        SUBMIT_AT + ShareTimingPolicy::default().max_overdue_threshold_seconds + 1
    }

    #[tokio::test(start_paused = true)]
    async fn invalid_status_scores_a_failure_without_blocking_confirmation() {
        let configured = helpers(5);
        let db = db_with_share(&configured);
        let share_id = share_id_of(&db);
        let now = ready_not_overdue();

        let transport = Arc::new(MockTransport::default());
        let status_url = |index: usize| {
            format!(
                "{}/shielded-vote/v1/share-status/{ROUND_ID}/{share_id}",
                helper(index)
            )
        };
        // Helper 1 answers outside the protocol's two states.
        transport.queue_get(&status_url(1), json_status("not_found"));
        transport.queue_get(&status_url(2), json_status("confirmed"));

        let client = client_with(transport.clone());
        let random = zero_bytes;
        let report = track_pending_shares(
            &db,
            &params(&configured, now, &random),
            &client,
            &never_cancel(),
        )
        .await
        .unwrap();

        // Confirmed locally off a single helper's answer.
        assert_eq!(
            report.confirmed,
            vec![ShareKey {
                bundle_index: 0,
                proposal_id: 1,
                share_index: 0
            }]
        );
        assert!(only_share(&db).confirmed);
        assert!(share::unconfirmed(&db, ROUND_ID).unwrap().is_empty());

        // The invalid answer cost helper 1 health.
        assert_eq!(client.health().failure_count(&helper(1)), 1);

        // Confirmation short-circuited the remaining helpers and any repair.
        assert_eq!(transport.call_count("/shielded-vote/v1/shares"), 0);
        assert!(report.resubmitted.is_empty());
        for index in 3..=5 {
            assert_eq!(transport.call_count(&helper(index)), 0);
        }

        // Existing definite placement history is unchanged.
        assert_eq!(only_share(&db).sent_to_urls.len(), 5);
    }

    #[tokio::test(start_paused = true)]
    async fn overdue_share_reaches_an_untried_helper_and_records_it() {
        let configured = helpers(2);
        let db = db_with_share(&[helper(1)]);
        let share_id = share_id_of(&db);
        let now = overdue();

        let transport = Arc::new(MockTransport::default());
        transport.queue_get(
            &format!(
                "{}/shielded-vote/v1/share-status/{ROUND_ID}/{share_id}",
                helper(1)
            ),
            json_status("pending"),
        );
        // Untried helpers come first in the resubmission order.
        transport.queue_post(
            &format!("{}/shielded-vote/v1/shares", helper(2)),
            json_status("queued"),
        );

        let client = client_with(transport.clone());
        let random = zero_bytes;
        let report = track_pending_shares(
            &db,
            &params(&configured, now, &random),
            &client,
            &never_cancel(),
        )
        .await
        .unwrap();

        assert_eq!(
            report.resubmitted,
            vec![ResubmittedShare {
                share: ShareKey {
                    bundle_index: 0,
                    proposal_id: 1,
                    share_index: 0
                },
                server_url: helper(2),
            }]
        );
        // The new helper is durably recorded, so the next pass polls it too.
        let stored = only_share(&db);
        assert!(!stored.confirmed);
        assert_eq!(stored.sent_to_urls, vec![helper(1), helper(2)]);
        assert_eq!(stored.submit_at, 0);
        assert_eq!(
            transport.posted_submit_at(&format!("{}/shielded-vote/v1/shares", helper(2))),
            0
        );
    }

    #[tokio::test(start_paused = true)]
    async fn under_placed_share_preserves_delayed_submit_at() {
        let configured = helpers(3);
        let db = db_with_delivery(&[helper(1)], &[], 2);
        let post_url = format!("{}/shielded-vote/v1/shares", helper(2));
        let transport = Arc::new(MockTransport::default());
        transport.queue_post(&post_url, json_status("queued"));

        let client = client_with(transport.clone());
        let random = preserve_two_server_order;
        let report = track_pending_shares(
            &db,
            &params(&configured, SUBMIT_AT - 1, &random),
            &client,
            &never_cancel(),
        )
        .await
        .unwrap();

        assert_eq!(report.resubmitted.len(), 1);
        assert_eq!(report.resubmitted[0].server_url, helper(2));
        let stored = only_share(&db);
        assert_eq!(stored.sent_to_urls, vec![helper(1), helper(2)]);
        assert_eq!(stored.submit_at, SUBMIT_AT);
        assert_eq!(transport.call_count("share-status"), 0);
        assert_eq!(transport.posted_submit_at(&post_url), SUBMIT_AT);
    }

    #[tokio::test(start_paused = true)]
    async fn one_tracking_pass_fills_the_complete_placement_deficit() {
        let configured = helpers(3);
        let db = db_with_delivery(&[], &[], 3);
        let transport = Arc::new(MockTransport::default());
        for server_url in &configured {
            transport.queue_post(
                &format!("{server_url}/shielded-vote/v1/shares"),
                json_status("queued"),
            );
        }

        let client = client_with(transport.clone());
        let random = zero_bytes;
        let report = track_pending_shares(
            &db,
            &params(&configured, SUBMIT_AT - 1, &random),
            &client,
            &never_cancel(),
        )
        .await
        .unwrap();

        assert_eq!(report.resubmitted.len(), 3);
        assert_eq!(transport.call_count("/shares"), 3);
        let stored = only_share(&db);
        assert_eq!(stored.sent_to_urls.len(), 3);
        assert!(configured
            .iter()
            .all(|url| stored.sent_to_urls.contains(url)));
        assert_eq!(stored.submit_at, SUBMIT_AT);
    }

    #[tokio::test(start_paused = true)]
    async fn early_replenishment_never_reposts_to_an_accepted_helper() {
        let configured = helpers(3);
        let db = db_with_delivery(&[helper(1)], &[helper(2)], 2);
        let transport = Arc::new(MockTransport::default());
        transport.queue_post(
            &format!("{}/shielded-vote/v1/shares", helper(3)),
            Err(HelperTransportError::Transport("refused".to_string())),
        );

        let client = client_with(transport.clone());
        let random = preserve_server_order;
        let report = track_pending_shares(
            &db,
            &params(&configured, SUBMIT_AT - 1, &random),
            &client,
            &never_cancel(),
        )
        .await
        .unwrap();

        assert!(report.resubmitted.is_empty());
        assert!(report.ambiguous.is_empty());
        assert_eq!(transport.call_count(&helper(3)), 1);
        assert_eq!(transport.call_count(&helper(1)), 0);
        assert_eq!(only_share(&db).sent_to_urls, vec![helper(1)]);
    }

    #[tokio::test(start_paused = true)]
    async fn one_tracking_pass_does_not_repeat_a_definite_failure() {
        let configured = helpers(4);
        let db = db_with_delivery(&[], &[], 3);
        let transport = Arc::new(MockTransport::default());
        transport.queue_post(
            &format!("{}/shielded-vote/v1/shares", helper(1)),
            Err(HelperTransportError::Transport("refused".to_string())),
        );
        for index in 2..=4 {
            transport.queue_post(
                &format!("{}/shielded-vote/v1/shares", helper(index)),
                json_status("queued"),
            );
        }

        let client = client_with(transport.clone());
        let random = preserve_server_order;
        let report = track_pending_shares(
            &db,
            &params(&configured, SUBMIT_AT - 1, &random),
            &client,
            &never_cancel(),
        )
        .await
        .unwrap();

        assert_eq!(report.resubmitted.len(), 3);
        assert_eq!(transport.call_count(&helper(1)), 1);
        assert_eq!(transport.call_count("/shares"), 4);
        let stored = only_share(&db);
        assert_eq!(stored.sent_to_urls, vec![helper(2), helper(3), helper(4)]);
        assert_eq!(stored.submit_at, SUBMIT_AT);
    }

    #[tokio::test(start_paused = true)]
    async fn a_definite_failure_is_eligible_again_on_a_later_pass() {
        let configured = helpers(2);
        let db = db_with_delivery(&[], &[], 1);
        let transport = Arc::new(MockTransport::default());
        for index in 1..=2 {
            transport.queue_post(
                &format!("{}/shielded-vote/v1/shares", helper(index)),
                Err(HelperTransportError::Transport("refused".to_string())),
            );
        }

        let client = client_with(transport.clone());
        let random = preserve_server_order;
        let first = track_pending_shares(
            &db,
            &params(&configured, SUBMIT_AT - 1, &random),
            &client,
            &never_cancel(),
        )
        .await
        .unwrap();
        assert!(first.resubmitted.is_empty());
        assert_eq!(transport.call_count("/shares"), 2);

        transport.queue_post(
            &format!("{}/shielded-vote/v1/shares", helper(1)),
            json_status("queued"),
        );
        let second = track_pending_shares(
            &db,
            &params(&configured, SUBMIT_AT - 1, &random),
            &client,
            &never_cancel(),
        )
        .await
        .unwrap();

        assert_eq!(second.resubmitted[0].server_url, helper(1));
        assert_eq!(transport.call_count(&helper(1)), 2);
        assert_eq!(only_share(&db).sent_to_urls, vec![helper(1)]);
    }

    #[tokio::test(start_paused = true)]
    async fn persisted_desired_target_replenishes_when_the_fleet_expands() {
        let configured = helpers(3);
        let db = db_with_delivery(&[helper(1), helper(2)], &[], 3);
        let post_url = format!("{}/shielded-vote/v1/shares", helper(3));
        let transport = Arc::new(MockTransport::default());
        transport.queue_post(&post_url, json_status("queued"));

        let client = client_with(transport.clone());
        let random = zero_bytes;
        let report = track_pending_shares(
            &db,
            &params(&configured, SUBMIT_AT - 1, &random),
            &client,
            &never_cancel(),
        )
        .await
        .unwrap();

        assert_eq!(report.resubmitted[0].server_url, helper(3));
        let stored = only_share(&db);
        assert_eq!(stored.target_count, 3);
        assert_eq!(stored.sent_to_urls, configured);
    }

    #[tokio::test(start_paused = true)]
    async fn under_placement_stops_at_the_resubmission_cutoff() {
        for now_seconds in [
            VOTE_END - ShareTimingPolicy::default().resubmit_cutoff_seconds,
            VOTE_END,
        ] {
            let configured = helpers(2);
            let db = db_with_delivery(&[helper(1)], &[], 2);
            let share_id = share_id_of(&db);
            let transport = Arc::new(MockTransport::default());
            transport.queue_get(
                &format!(
                    "{}/shielded-vote/v1/share-status/{ROUND_ID}/{share_id}",
                    helper(1)
                ),
                json_status("pending"),
            );

            let client = client_with(transport.clone());
            let no_recovery_randomness = |_: usize| -> Vec<u8> {
                panic!("cutoff must be checked before building a recovery order")
            };
            let report = track_pending_shares(
                &db,
                &params(&configured, now_seconds, &no_recovery_randomness),
                &client,
                &never_cancel(),
            )
            .await
            .unwrap();

            assert!(report.resubmitted.is_empty());
            assert!(report.ambiguous.is_empty());
            assert_eq!(transport.call_count("/shares"), 0);
            let stored = only_share(&db);
            assert_eq!(stored.sent_to_urls, vec![helper(1)]);
            assert_eq!(stored.submit_at, SUBMIT_AT);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn resubmission_rechecks_the_cutoff_before_every_post() {
        use std::sync::atomic::{AtomicU64, Ordering};

        let configured = helpers(3);
        let db = db_with_delivery(&[], &[], 3);
        let transport = Arc::new(MockTransport::default());
        for server_url in &configured {
            transport.queue_post(
                &format!("{server_url}/shielded-vote/v1/shares"),
                json_status("queued"),
            );
        }
        let client = client_with(transport.clone());
        let random = zero_bytes;
        let now_seconds = VOTE_END - ShareTimingPolicy::default().resubmit_cutoff_seconds - 1;
        let elapsed = AtomicU64::new(0);
        let elapsed_seconds = || elapsed.fetch_add(1, Ordering::Relaxed);

        let report = track_pending_shares_with_elapsed(
            &db,
            &params(&configured, now_seconds, &random),
            &client,
            &never_cancel(),
            &elapsed_seconds,
        )
        .await
        .unwrap();

        assert_eq!(report.resubmitted.len(), 1);
        assert_eq!(transport.call_count("/shares"), 1);
        assert_eq!(only_share(&db).sent_to_urls.len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn missing_vote_end_still_allows_early_replenishment() {
        let configured = helpers(2);
        let db = db_with_delivery(&[helper(1)], &[], 2);
        let post_url = format!("{}/shielded-vote/v1/shares", helper(2));
        let transport = Arc::new(MockTransport::default());
        transport.queue_post(&post_url, json_status("queued"));

        let client = client_with(transport.clone());
        let random = zero_bytes;
        let mut tracking_params = params(&configured, SUBMIT_AT - 1, &random);
        tracking_params.vote_end_time_seconds = None;
        let report = track_pending_shares(&db, &tracking_params, &client, &never_cancel())
            .await
            .unwrap();

        assert_eq!(report.resubmitted.len(), 1);
        assert_eq!(transport.posted_submit_at(&post_url), SUBMIT_AT);
        assert_eq!(only_share(&db).submit_at, SUBMIT_AT);
    }

    #[tokio::test(start_paused = true)]
    async fn pending_status_keeps_an_ambiguous_attempt_out_of_placement() {
        let configured = helpers(3);
        let db = db_with_delivery(&[helper(1)], &[helper(2)], 2);
        let share_id = share_id_of(&db);
        let transport = Arc::new(MockTransport::default());
        for index in 1..=2 {
            transport.queue_get(
                &format!(
                    "{}/shielded-vote/v1/share-status/{ROUND_ID}/{share_id}",
                    helper(index)
                ),
                json_status("pending"),
            );
        }
        transport.queue_post(
            &format!("{}/shielded-vote/v1/shares", helper(3)),
            json_status("queued"),
        );

        let client = client_with(transport.clone());
        let random = zero_bytes;
        let report = track_pending_shares(
            &db,
            &params(&configured, ready_not_overdue(), &random),
            &client,
            &never_cancel(),
        )
        .await
        .unwrap();

        assert_eq!(report.resubmitted.len(), 1);
        assert_eq!(report.resubmitted[0].server_url, helper(3));
        let stored = only_share(&db);
        assert_eq!(stored.sent_to_urls, vec![helper(1), helper(3)]);
        assert_eq!(stored.ambiguous_urls, vec![helper(2)]);
        assert_eq!(stored.submit_at, SUBMIT_AT);
        assert_eq!(transport.call_count(&helper(2)), 1);
        assert_eq!(transport.call_count("/shares"), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn ambiguous_resubmission_is_recorded_while_recovery_continues() {
        let configured = helpers(3);
        let db = Arc::new(db_with_delivery(&[helper(1)], &[], 2));
        let transport = Arc::new(MockTransport::default());
        transport.queue_post(
            &format!("{}/shielded-vote/v1/shares", helper(2)),
            Err(HelperTransportError::Timeout),
        );
        transport.queue_post(
            &format!("{}/shielded-vote/v1/shares", helper(3)),
            json_status("queued"),
        );
        let observed_db = db.clone();
        transport.observe_posts(move |url| {
            let expected = if url.contains("helper-2") {
                helper(2)
            } else {
                helper(3)
            };
            assert!(only_share(&observed_db).attempting_urls.contains(&expected));
        });

        let client = client_with(transport.clone());
        let random = preserve_two_server_order;
        let report = track_pending_shares(
            &db,
            &params(&configured, SUBMIT_AT, &random),
            &client,
            &never_cancel(),
        )
        .await
        .unwrap();

        assert_eq!(report.ambiguous.len(), 1);
        assert_eq!(report.ambiguous[0].server_url, helper(2));
        assert_eq!(report.resubmitted[0].server_url, helper(3));
        let stored = only_share(&db);
        assert_eq!(stored.sent_to_urls, vec![helper(1), helper(3)]);
        assert_eq!(stored.ambiguous_urls, vec![helper(2)]);
        assert_eq!(stored.submit_at, SUBMIT_AT);
    }

    #[tokio::test(start_paused = true)]
    async fn ambiguous_attempt_is_durable_before_recovery_advances() {
        let configured = helpers(3);
        let db = db_with_delivery(&[helper(1)], &[], 2);
        let transport = Arc::new(MockTransport::default());
        transport.queue_post(
            &format!("{}/shielded-vote/v1/shares", helper(2)),
            Err(HelperTransportError::Timeout),
        );
        transport.queue_post(
            &format!("{}/shielded-vote/v1/shares", helper(3)),
            json_status("queued"),
        );

        let client = client_with(transport.clone());
        let random = preserve_two_server_order;
        let cancel_before_second_helper = || {
            if transport.call_count(&helper(2)) == 0 {
                return false;
            }
            let stored = only_share(&db);
            assert_eq!(stored.ambiguous_urls, vec![helper(2)]);
            assert_eq!(stored.submit_at, SUBMIT_AT);
            true
        };
        let report = track_pending_shares(
            &db,
            &params(&configured, SUBMIT_AT, &random),
            &client,
            &cancel_before_second_helper,
        )
        .await
        .unwrap();

        assert!(report.cancelled);
        assert_eq!(report.ambiguous.len(), 1);
        assert_eq!(report.ambiguous[0].server_url, helper(2));
        assert_eq!(transport.call_count(&helper(3)), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn overdue_ambiguous_attempt_resets_the_delayed_schedule() {
        let configured = helpers(2);
        let db = db_with_delivery(&[helper(1)], &[], 2);
        let share_id = share_id_of(&db);
        let transport = Arc::new(MockTransport::default());
        transport.queue_get(
            &format!(
                "{}/shielded-vote/v1/share-status/{ROUND_ID}/{share_id}",
                helper(1)
            ),
            json_status("pending"),
        );
        transport.queue_post(
            &format!("{}/shielded-vote/v1/shares", helper(2)),
            Err(HelperTransportError::Timeout),
        );

        let client = client_with(transport);
        let random = zero_bytes;
        let report = track_pending_shares(
            &db,
            &params(&configured, overdue(), &random),
            &client,
            &never_cancel(),
        )
        .await
        .unwrap();

        assert_eq!(report.ambiguous.len(), 1);
        let stored = only_share(&db);
        assert_eq!(stored.ambiguous_urls, vec![helper(2)]);
        assert_eq!(stored.submit_at, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn unusable_successful_resubmission_is_recorded_while_recovery_continues() {
        let configured = helpers(3);
        let db = db_with_delivery(&[helper(1)], &[], 2);
        let transport = Arc::new(MockTransport::default());
        transport.queue_post(
            &format!("{}/shielded-vote/v1/shares", helper(2)),
            Ok(HelperResponse {
                status: 200,
                body: br#"{"message":"queued"}"#.to_vec(),
            }),
        );
        transport.queue_post(
            &format!("{}/shielded-vote/v1/shares", helper(3)),
            json_status("queued"),
        );

        let client = client_with(transport.clone());
        let random = preserve_two_server_order;
        let report = track_pending_shares(
            &db,
            &params(&configured, SUBMIT_AT, &random),
            &client,
            &never_cancel(),
        )
        .await
        .unwrap();

        assert_eq!(report.ambiguous.len(), 1);
        assert_eq!(report.ambiguous[0].server_url, helper(2));
        assert_eq!(report.resubmitted[0].server_url, helper(3));
        let stored = only_share(&db);
        assert_eq!(stored.sent_to_urls, vec![helper(1), helper(3)]);
        assert_eq!(stored.ambiguous_urls, vec![helper(2)]);
    }

    #[tokio::test(start_paused = true)]
    async fn persisted_ambiguous_helper_is_excluded_from_recovery_posts() {
        let configured = helpers(3);
        let db = db_with_delivery(&[helper(1)], &[helper(2)], 2);
        db.conn()
            .execute(
                "UPDATE share_delegations SET ambiguous_urls = :urls",
                rusqlite::named_params! {
                    ":urls": serde_json::to_string(&[format!("{}/", helper(2))]).unwrap(),
                },
            )
            .unwrap();
        let transport = Arc::new(MockTransport::default());
        transport.queue_post(
            &format!("{}/shielded-vote/v1/shares", helper(3)),
            http_status(400),
        );

        let client = client_with(transport.clone());
        let random = zero_bytes;
        let report = track_pending_shares(
            &db,
            &params(&configured, SUBMIT_AT - 1, &random),
            &client,
            &never_cancel(),
        )
        .await
        .unwrap();

        assert!(report.resubmitted.is_empty());
        assert!(report.ambiguous.is_empty());
        assert_eq!(transport.call_count(&helper(3)), 1);
        assert_eq!(transport.call_count(&helper(1)), 0);
        assert_eq!(transport.call_count(&helper(2)), 0);
        let stored = only_share(&db);
        assert_eq!(stored.sent_to_urls, vec![helper(1)]);
        assert_eq!(stored.ambiguous_urls, vec![helper(2)]);
    }

    #[tokio::test(start_paused = true)]
    async fn resubmission_demotes_degraded_helpers_within_the_untried_group() {
        let configured = helpers(3);
        let db = db_with_delivery(&[helper(1)], &[], 2);
        let transport = Arc::new(MockTransport::default());
        transport.queue_post(
            &format!("{}/shielded-vote/v1/shares", helper(3)),
            json_status("queued"),
        );

        let client = client_with(transport.clone());
        for _ in 0..crate::helper::health::HELPER_FAILURE_THRESHOLD {
            client.health().record_failure(&helper(2), SUBMIT_AT);
        }
        let random = zero_bytes;
        let report = track_pending_shares(
            &db,
            &params(&configured, SUBMIT_AT, &random),
            &client,
            &never_cancel(),
        )
        .await
        .unwrap();

        assert_eq!(report.resubmitted[0].server_url, helper(3));
        assert_eq!(transport.call_count(&helper(2)), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn confirmed_share_is_never_resubmitted_even_when_overdue() {
        let configured = helpers(2);
        let db = db_with_share(&[helper(1)]);
        let share_id = share_id_of(&db);
        let now = overdue();

        let transport = Arc::new(MockTransport::default());
        transport.queue_get(
            &format!(
                "{}/shielded-vote/v1/share-status/{ROUND_ID}/{share_id}",
                helper(1)
            ),
            json_status("confirmed"),
        );

        let client = client_with(transport.clone());
        let random = zero_bytes;
        let report = track_pending_shares(
            &db,
            &params(&configured, now, &random),
            &client,
            &never_cancel(),
        )
        .await
        .unwrap();

        assert_eq!(report.confirmed.len(), 1);
        assert!(report.resubmitted.is_empty());
        // Confirmation short-circuits the overdue branch entirely.
        assert_eq!(transport.call_count("/shielded-vote/v1/shares"), 0);
        assert_eq!(only_share(&db).sent_to_urls, vec![helper(1)]);
    }

    #[tokio::test(start_paused = true)]
    async fn idle_share_contacts_no_helper() {
        let configured = helpers(3);
        let db = db_with_share(&configured);
        let transport = Arc::new(MockTransport::default());
        let client = client_with(transport.clone());
        let random = zero_bytes;

        let report = track_pending_shares(
            &db,
            &params(&configured, SUBMIT_AT, &random),
            &client,
            &never_cancel(),
        )
        .await
        .unwrap();

        assert!(transport.calls().is_empty());
        assert!(report.confirmed.is_empty());
        // Still pending, so the caller is told when to come back.
        assert!(report.next_delay_seconds.is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn unconfigured_helpers_are_not_polled() {
        let configured = vec![helper(1)];
        // The share was also sent to a helper the wallet has since dropped.
        let db = db_with_share(&[helper(1), helper(9)]);
        let share_id = share_id_of(&db);

        let transport = Arc::new(MockTransport::default());
        transport.queue_get(
            &format!(
                "{}/shielded-vote/v1/share-status/{ROUND_ID}/{share_id}",
                helper(1)
            ),
            json_status("pending"),
        );

        let client = client_with(transport.clone());
        let random = zero_bytes;
        track_pending_shares(
            &db,
            &params(&configured, ready_not_overdue(), &random),
            &client,
            &never_cancel(),
        )
        .await
        .unwrap();

        assert_eq!(transport.call_count(&helper(9)), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn cancelled_pass_reports_cancellation_and_keeps_durable_effects() {
        let configured = helpers(2);
        let db = db_with_share(&configured);
        let transport = Arc::new(MockTransport::default());
        let client = client_with(transport.clone());
        let random = zero_bytes;
        let always_cancel = || true;

        let report = track_pending_shares(
            &db,
            &params(&configured, ready_not_overdue(), &random),
            &client,
            &always_cancel,
        )
        .await
        .unwrap();

        assert!(report.cancelled);
        assert!(transport.calls().is_empty());
        assert!(!only_share(&db).confirmed);
    }

    #[tokio::test(start_paused = true)]
    async fn missing_recovery_material_is_reported_not_retried() {
        let configured = helpers(2);
        let db = db_with_share(&[helper(1)]);
        let share_id = share_id_of(&db);
        // Drop the recovery bundle the resubmission body is built from.
        db.conn()
            .execute(
                "UPDATE votes SET commitment_bundle_json = NULL, vc_tree_position = NULL
                 WHERE round_id = :round_id AND wallet_id = :wallet_id",
                rusqlite::named_params! {
                    ":round_id": ROUND_ID,
                    ":wallet_id": WALLET_ID,
                },
            )
            .unwrap();

        let transport = Arc::new(MockTransport::default());
        transport.queue_get(
            &format!(
                "{}/shielded-vote/v1/share-status/{ROUND_ID}/{share_id}",
                helper(1)
            ),
            json_status("pending"),
        );

        let client = client_with(transport.clone());
        let random = zero_bytes;
        let report = track_pending_shares(
            &db,
            &params(&configured, overdue(), &random),
            &client,
            &never_cancel(),
        )
        .await
        .unwrap();

        assert_eq!(report.unrecoverable.len(), 1);
        assert!(report.resubmitted.is_empty());
        assert_eq!(transport.call_count("/shielded-vote/v1/shares"), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn resubmission_waits_for_the_confirmed_vc_position() {
        let configured = helpers(2);
        let db = db_with_delivery(&[helper(1)], &[], 2);
        let share_id = share_id_of(&db);
        db.conn()
            .execute(
                "UPDATE votes SET vc_tree_position = NULL
                 WHERE round_id = :round_id AND wallet_id = :wallet_id",
                rusqlite::named_params! {
                    ":round_id": ROUND_ID,
                    ":wallet_id": WALLET_ID,
                },
            )
            .unwrap();

        let transport = Arc::new(MockTransport::default());
        let status_url = format!(
            "{}/shielded-vote/v1/share-status/{ROUND_ID}/{share_id}",
            helper(1)
        );
        transport.queue_get(&status_url, json_status("pending"));
        let client = client_with(transport.clone());
        let no_randomness =
            |_: usize| -> Vec<u8> { panic!("recovery order must wait for the real VC position") };

        let deferred = track_pending_shares(
            &db,
            &params(&configured, overdue(), &no_randomness),
            &client,
            &never_cancel(),
        )
        .await
        .unwrap();

        assert!(deferred.unrecoverable.is_empty());
        assert!(deferred.resubmitted.is_empty());
        assert_eq!(transport.call_count("/shielded-vote/v1/shares"), 0);

        db.conn()
            .execute(
                "UPDATE votes SET vc_tree_position = 789
                 WHERE round_id = :round_id AND wallet_id = :wallet_id",
                rusqlite::named_params! {
                    ":round_id": ROUND_ID,
                    ":wallet_id": WALLET_ID,
                },
            )
            .unwrap();
        transport.queue_get(&status_url, json_status("pending"));
        let post_url = format!("{}/shielded-vote/v1/shares", helper(2));
        transport.queue_post(&post_url, json_status("queued"));
        let random = zero_bytes;

        let resumed = track_pending_shares(
            &db,
            &params(&configured, overdue(), &random),
            &client,
            &never_cancel(),
        )
        .await
        .unwrap();

        assert_eq!(resumed.resubmitted[0].server_url, helper(2));
        let bodies = transport.post_bodies.lock().unwrap();
        let (_, body) = bodies.iter().find(|(url, _)| url == &post_url).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(body).unwrap()["tree_position"],
            789
        );
    }

    #[test]
    fn dedupe_keeps_first_occurrence_order() {
        let urls = ["b", "a", "b", "c"].iter().map(|s| s.to_string());
        assert_eq!(dedupe_preserving_order(urls), vec!["b", "a", "c"]);
    }
}
