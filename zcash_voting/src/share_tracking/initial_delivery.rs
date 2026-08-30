use std::{collections::HashSet, time::Duration};

use crate::recovery as vote_recovery;
use crate::{
    helper::{client::HelperClient, url::canonical_helper_url_list},
    round::VotingDb,
    share,
    share_policy::{
        share_submission_target_count, SHARE_DELIVERY_MIN_ATTEMPT_BUDGET_MILLISECONDS,
        SHARE_INITIAL_DELIVERY_TIMEOUT_MILLISECONDS,
    },
    types::{SharePayload, VotingError},
};

use super::{
    configured_fleet::ConfiguredHelperFleet, InitialShareSubmissionParams, ShareSubmissionReport,
    ShareSubmissionRequest,
};

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
/// A process death or failed outcome write therefore leaves an outcome-unknown
/// marker. Early replenishment will not replay it; overdue recovery may retry
/// it only through the helper's duplicate-safe endpoint.
///
/// # Errors
///
/// Returns [`VotingError::InvalidInput`] when the share JSON is malformed or
/// when any candidate URL fails
/// [`crate::helper::url::canonicalize_helper_base_url`]. Validation happens
/// before any network I/O and is all-or-nothing: one invalid configured URL
/// fails the call so a misconfigured fleet is surfaced instead of silently
/// shrinking delivery. Pre-filter with that function to tolerate individual
/// invalid entries.
#[cfg(test)]
pub(crate) async fn submit_share_to_helpers(
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
    // The fleet is part of the request contract. Validate it before creating
    // or merging any durable row so invalid input is storage-atomic.
    let planned = canonical_helper_url_list(params.planned_servers)?;
    let fallback = canonical_helper_url_list(params.fallback_servers)?;
    if planned.len() != params.planned_servers.len()
        || fallback.len() != params.fallback_servers.len()
        || fallback.iter().any(|url| planned.contains(url))
    {
        return Err(VotingError::InvalidInput {
            message: "planned and fallback helper groups must contain distinct canonical helpers"
                .to_string(),
        });
    }
    submit_share_to_canonical_helpers(db, client, params, &planned, &fallback, cancel).await
}

/// Executes a fan-out after the caller has established canonical, disjoint
/// candidate groups.
async fn submit_share_to_canonical_helpers(
    db: &VotingDb,
    client: &HelperClient,
    params: &InitialShareSubmissionParams<'_>,
    planned: &[String],
    fallback: &[String],
    cancel: &(dyn Fn() -> bool + Send + Sync),
) -> Result<ShareSubmissionReport, VotingError> {
    let planned_set: HashSet<&str> = planned.iter().map(String::as_str).collect();
    let candidates = planned.iter().chain(fallback).cloned().collect::<Vec<_>>();
    let initial_persisted_report = ShareSubmissionReport {
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
            submission: &initial_persisted_report,
            submit_at: params.submit_at,
        },
    )?;
    let persisted_delivery = share::list(db, params.round_id)?
        .into_iter()
        .find(|share| {
            share.bundle_index == params.bundle_index
                && share.proposal_id == params.proposal_id
                && share.share_index == params.share_index
        })
        .ok_or_else(|| VotingError::Internal {
            message: "newly journaled helper share was not found".to_string(),
        })?;
    let definite_acceptance_urls = persisted_delivery
        .sent_to_urls
        .into_iter()
        .filter(|url| candidates.contains(url))
        .collect::<Vec<_>>();
    let outcome_unknown_urls = persisted_delivery
        .ambiguous_urls
        .into_iter()
        .filter(|url| candidates.contains(url))
        .collect::<Vec<_>>();
    let interrupted_attempt_urls = persisted_delivery
        .attempting_urls
        .into_iter()
        .filter(|url| candidates.contains(url))
        .collect::<Vec<_>>();
    let mut delivery_state = share::ShareDeliveryState::from_url_lists(
        &definite_acceptance_urls,
        &outcome_unknown_urls,
        &interrupted_attempt_urls,
    )?;
    let mut remaining = candidates;
    remaining.retain(|url| {
        !delivery_state.accepted_urls().contains(url)
            && !delivery_state.outcome_unknown_urls().contains(url)
            && !delivery_state.in_flight_urls().contains(url)
    });
    let definite_acceptance_target = params.target_count.min(
        delivery_state
            .accepted_urls()
            .len()
            .saturating_add(remaining.len()),
    );
    let deadline = tokio::time::Instant::now()
        + Duration::from_millis(SHARE_INITIAL_DELIVERY_TIMEOUT_MILLISECONDS);

    while !remaining.is_empty() && delivery_state.accepted_urls().len() < definite_acceptance_target
    {
        if cancel() {
            break;
        }
        let active_group = if remaining
            .iter()
            .any(|url| planned_set.contains(url.as_str()))
        {
            remaining
                .iter()
                .filter(|url| planned_set.contains(url.as_str()))
                .cloned()
                .collect::<Vec<_>>()
        } else {
            remaining.clone()
        };
        let ordered = client
            .health()
            .candidate_servers(&active_group, params.now_seconds);
        let Some(server_url) = ordered.into_iter().next() else {
            break;
        };
        remaining.retain(|url| url != &server_url);

        let remaining_time = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining_time < Duration::from_millis(SHARE_DELIVERY_MIN_ATTEMPT_BUDGET_MILLISECONDS) {
            // An attempt started with almost no budget is guaranteed to end
            // as an unknown outcome; leave the helper untouched instead.
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

        let helper_post_outcome = tokio::time::timeout_at(
            deadline,
            client.submit_share_with_timeout(
                &server_url,
                params.share_wire_json,
                params.now_seconds,
                cancel,
                remaining_time,
                Some(deadline),
            ),
        )
        .await;
        match helper_post_outcome {
            Err(_) => {
                // The client returns a held definite error rather than
                // sleeping a retry backoff into this deadline, so an elapse
                // here can only cancel an in-flight HTTP attempt, whose
                // transport outcome is genuinely unknown; retain it for
                // polling.
                share::resolve_delivery_attempt(
                    db,
                    &attempt,
                    share::ShareDeliveryAttemptOutcome::Ambiguous,
                    false,
                )?;
                delivery_state.mark_outcome_unknown(&server_url)?;
                break;
            }
            Ok(Ok(_)) => {
                share::resolve_delivery_attempt(
                    db,
                    &attempt,
                    share::ShareDeliveryAttemptOutcome::Accepted,
                    false,
                )?;
                delivery_state.mark_accepted(&server_url)?;
            }
            Ok(Err(error)) if error.is_ambiguous() => {
                share::resolve_delivery_attempt(
                    db,
                    &attempt,
                    share::ShareDeliveryAttemptOutcome::Ambiguous,
                    false,
                )?;
                delivery_state.mark_outcome_unknown(&server_url)?;
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
        accepted_urls: delivery_state.accepted_urls().to_vec(),
        ambiguous_urls: delivery_state.outcome_unknown_urls().to_vec(),
        target_count: params.target_count,
    })
}

/// Validates and submits one payload owned by a committed vote.
///
/// The complete configured fleet, plan membership and target, committed share
/// identity, and recovery material are validated before delivery creates
/// durable state or dispatches network requests.
pub(crate) async fn submit_committed_share_to_helpers(
    db: &VotingDb,
    client: &HelperClient,
    round_id: &str,
    bundle_index: u32,
    proposal_id: u32,
    payloads: &[SharePayload],
    request: ShareSubmissionRequest<'_>,
    cancel: &(dyn Fn() -> bool + Send + Sync),
) -> Result<ShareSubmissionReport, VotingError> {
    let configured = ConfiguredHelperFleet::new(request.configured_server_urls)?;
    let planned = canonical_helper_url_list(&request.plan.target_servers)?;
    if planned.len() != request.plan.target_servers.len() {
        return Err(VotingError::InvalidInput {
            message: "plan target_servers must contain distinct canonical helpers".to_string(),
        });
    }
    let expected_target = share_submission_target_count(configured.len());
    let planned_target = usize::try_from(request.plan.target_count).unwrap_or(usize::MAX);
    if planned_target != expected_target || planned.len() != planned_target {
        return Err(VotingError::InvalidInput {
            message: format!(
                "plan target_count and target_servers must match the configured fleet target {expected_target}"
            ),
        });
    }
    if let Some(server_url) = planned.iter().find(|url| !configured.contains(url)) {
        return Err(VotingError::InvalidInput {
            message: format!("planned helper is not in configured_server_urls: {server_url}"),
        });
    }
    let payload = payloads
        .iter()
        .find(|payload| payload.enc_share.share_index == request.share_index)
        .ok_or_else(|| VotingError::InvalidInput {
            message: format!(
                "share_index {} is not present in the committed vote",
                request.share_index
            ),
        })?;
    if payload.vote_round_id != round_id || payload.proposal_id != proposal_id {
        return Err(VotingError::Internal {
            message: "committed share payload identity does not match its vote handle".to_string(),
        });
    }
    let vc_tree_position =
        match vote_recovery::helper_recovery_material(db, round_id, bundle_index, proposal_id)? {
            vote_recovery::HelperRecoveryMaterial::Ready(bundle) => bundle.vc_tree_position,
            vote_recovery::HelperRecoveryMaterial::AwaitingVcPosition => {
                return Err(VotingError::InvalidInput {
                    message: "committed vote must be confirmed before submitting helper shares"
                        .to_string(),
                });
            }
            vote_recovery::HelperRecoveryMaterial::Missing => {
                return Err(VotingError::Internal {
                    message: "committed vote is missing durable helper recovery material"
                        .to_string(),
                });
            }
        };
    let share_wire_json = payload.to_wire_json(Some(vc_tree_position), request.plan.submit_at)?;
    let mut candidates = planned;
    let fallback = configured
        .urls()
        .iter()
        .filter(|url| !candidates.contains(url))
        .cloned()
        .collect::<Vec<_>>();
    candidates.extend(fallback);
    submit_share_to_canonical_helpers(
        db,
        client,
        &InitialShareSubmissionParams {
            round_id,
            bundle_index,
            proposal_id,
            share_index: request.share_index,
            share_wire_json: &share_wire_json,
            #[cfg(test)]
            planned_servers: &candidates[..planned_target],
            #[cfg(test)]
            fallback_servers: &candidates[planned_target..],
            target_count: planned_target,
            submit_at: request.plan.submit_at,
            now_seconds: request.now_seconds,
        },
        &candidates[..planned_target],
        &candidates[planned_target..],
        cancel,
    )
    .await
}
