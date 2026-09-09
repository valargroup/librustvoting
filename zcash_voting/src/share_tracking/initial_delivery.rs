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
    types::{ShareDelegationRecord, SharePayload, VotingError},
};

use super::{
    configured_fleet::ConfiguredHelperFleet, CommittedShareSubmissionRequest,
    InitialShareSubmissionParams, ShareSubmissionReport,
};

/// Fans one freshly built share out to helpers until enough accept it.
///
/// Outstanding targets are posted together in waves capped at the definite
/// acceptances still needed. Ambiguous attempts do not count as acceptances,
/// so later waves may contact additional helpers.
///
/// Helpers are reserved in health order within the planned and fallback
/// groups. Outcomes from one wave affect the ordering of later waves. Each
/// helper is selected at most once by this fan-out; the client separately
/// retries definite transient failures under its bounded retry policy.
///
/// Every helper is first persisted as `attempting`; only then can its POST be
/// dispatched. Definite acknowledgements and unknown outcomes are promoted to
/// their final state, while definite pre-dispatch failures clear the marker.
/// A process death or failed outcome write therefore leaves an outcome-unknown
/// marker. Recovery retries that interrupted helper after untried helpers,
/// preserving the original schedule unless the share is genuinely overdue.
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
    let scope = share::ShareOperationScope::capture(db);
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
    submit_share_to_canonical_helpers(db, &scope, client, params, &planned, &fallback, cancel).await
}

/// Executes a fan-out after the caller has established canonical, disjoint
/// candidate groups.
#[cfg(test)]
async fn submit_share_to_canonical_helpers(
    db: &VotingDb,
    scope: &share::ShareOperationScope,
    client: &HelperClient,
    params: &InitialShareSubmissionParams<'_>,
    planned: &[String],
    fallback: &[String],
    cancel: &(dyn Fn() -> bool + Send + Sync),
) -> Result<ShareSubmissionReport, VotingError> {
    let (_, persisted_delivery) = prepare_share_delivery(db, scope, params)?;
    dispatch_share_to_canonical_helpers(
        db,
        scope,
        client,
        params,
        planned,
        fallback,
        persisted_delivery,
        cancel,
    )
    .await
}

/// Creates or merges the durable record before dispatch and returns the
/// write-once schedule together with the merged delivery state.
#[cfg(test)]
fn prepare_share_delivery(
    db: &VotingDb,
    scope: &share::ShareOperationScope,
    params: &InitialShareSubmissionParams<'_>,
) -> Result<(u64, ShareDelegationRecord), VotingError> {
    let initial_persisted_report = ShareSubmissionReport {
        target_count: params.target_count,
        ..ShareSubmissionReport::default()
    };
    let (durable_submit_at, expected_nullifier) = share::record_delivery_for_scope(
        db,
        scope,
        &share::ShareDeliveryRecordParams {
            round_id: params.round_id,
            bundle_index: params.bundle_index,
            proposal_id: params.proposal_id,
            share_index: params.share_index,
            submission: &initial_persisted_report,
            submit_at: params.submit_at,
        },
    )?;
    load_prepared_share_delivery(db, scope, params, durable_submit_at, &expected_nullifier)
}

fn prepare_committed_share_delivery(
    db: &VotingDb,
    scope: &share::ShareOperationScope,
    params: &InitialShareSubmissionParams<'_>,
    expected_commitment_bundle_json: &str,
    expected_nullifier: &[u8; 32],
) -> Result<(u64, ShareDelegationRecord), VotingError> {
    let initial_persisted_report = ShareSubmissionReport {
        target_count: params.target_count,
        ..ShareSubmissionReport::default()
    };
    let (durable_submit_at, expected_nullifier) = share::record_delivery_for_committed_vote(
        db,
        scope,
        &share::ShareDeliveryRecordParams {
            round_id: params.round_id,
            bundle_index: params.bundle_index,
            proposal_id: params.proposal_id,
            share_index: params.share_index,
            submission: &initial_persisted_report,
            submit_at: params.submit_at,
        },
        expected_commitment_bundle_json,
        expected_nullifier,
    )?;
    load_prepared_share_delivery(db, scope, params, durable_submit_at, &expected_nullifier)
}

fn load_prepared_share_delivery(
    db: &VotingDb,
    scope: &share::ShareOperationScope,
    params: &InitialShareSubmissionParams<'_>,
    durable_submit_at: u64,
    expected_nullifier: &[u8; 32],
) -> Result<(u64, ShareDelegationRecord), VotingError> {
    let persisted_delivery = share::list_for_scope(db, scope, params.round_id)?
        .into_iter()
        .find(|share| {
            share.bundle_index == params.bundle_index
                && share.proposal_id == params.proposal_id
                && share.share_index == params.share_index
                && share.nullifier == expected_nullifier.as_slice()
        })
        .ok_or_else(|| VotingError::Internal {
            message: "newly journaled helper share was not found".to_string(),
        })?;
    Ok((durable_submit_at, persisted_delivery))
}

/// What one wave member's slot produced.
///
/// The two are kept apart because only one of them is evidence about the
/// helper. Local admission expiry is a property of this process's own POST
/// queue: the request never left, so the helper is neither slow, unreachable,
/// nor refusing, and spending its single attempt on that would be reading a
/// queue as an answer.
enum WaveAttempt {
    /// Local admission never granted a POST slot, so this helper was never
    /// contacted. Carries why, because only budget expiry is the throttling a
    /// pass reports; cancellation ends the pass on its own.
    NotAdmitted(crate::HelperError),
    /// The POST ran. Inner `Err` is the delivery deadline elapsing mid-flight.
    Posted(
        Result<
            Result<crate::helper::client::ShareSubmissionStatus, crate::HelperError>,
            tokio::time::error::Elapsed,
        >,
    ),
}

/// Continues fan-out from an already-prepared durable delivery record.
#[allow(clippy::too_many_arguments)]
async fn dispatch_share_to_canonical_helpers(
    db: &VotingDb,
    scope: &share::ShareOperationScope,
    client: &HelperClient,
    params: &InitialShareSubmissionParams<'_>,
    planned: &[String],
    fallback: &[String],
    persisted_delivery: ShareDelegationRecord,
    cancel: &(dyn Fn() -> bool + Send + Sync),
) -> Result<ShareSubmissionReport, VotingError> {
    let expected_nullifier = persisted_delivery.nullifier.clone();
    let candidates = planned.iter().chain(fallback).cloned().collect::<Vec<_>>();
    let Some(_operation_guard) = super::lock_share_operation_or_cancel(
        scope,
        params.round_id,
        params.bundle_index,
        params.proposal_id,
        params.share_index,
        cancel,
    )
    .await?
    else {
        let delivery_state =
            load_current_delivery_state(db, scope, params, &expected_nullifier, &candidates)?;
        return Ok(delivery_report(&delivery_state, params.target_count, false));
    };
    let generation = share::ShareGeneration::new(scope, &expected_nullifier);
    let planned_set: HashSet<&str> = planned.iter().map(String::as_str).collect();
    let mut delivery_state =
        load_current_delivery_state(db, scope, params, &expected_nullifier, &candidates)?;
    // Both are set for the whole pass, not one wave: together they explain
    // the report the caller finally sees. `helper_answered` is what keeps a
    // real refusal from being read as local throttling: a pass that asked
    // even one helper has evidence about the fleet, whatever else it also
    // failed to send.
    let mut never_admitted_a_candidate = false;
    let mut helper_answered = false;
    let mut remaining = candidates.clone();
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
        // Reserve only the outstanding acceptance need. Sequential reservations
        // preserve planned-before-fallback ordering and let the durable capacity
        // check observe every reservation already made in this wave.
        let need = definite_acceptance_target.saturating_sub(delivery_state.accepted_urls().len());
        let mut wave: Vec<String> = Vec::new();
        let mut stop_after_wave = false;
        while wave.len() < need && !remaining.is_empty() {
            if cancel() {
                stop_after_wave = true;
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
            let remaining_time = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining_time
                < Duration::from_millis(SHARE_DELIVERY_MIN_ATTEMPT_BUDGET_MILLISECONDS)
            {
                // An attempt started with almost no budget is guaranteed to end
                // as an unknown outcome; leave the helper untouched instead.
                stop_after_wave = true;
                break;
            }

            let attempt = share::ShareDeliveryAttemptParams {
                round_id: params.round_id,
                bundle_index: params.bundle_index,
                proposal_id: params.proposal_id,
                share_index: params.share_index,
                server_url: &server_url,
                target_count: definite_acceptance_target,
                submit_at: params.submit_at,
            };
            match share::begin_delivery_attempt_for_generation(
                db,
                &attempt,
                generation,
                &candidates,
            )? {
                crate::storage::queries::ShareAttemptReservation::Started => {
                    remaining.retain(|url| url != &server_url);
                    wave.push(server_url);
                }
                crate::storage::queries::ShareAttemptReservation::AlreadyRecorded => {
                    remaining.retain(|url| url != &server_url);
                    continue;
                }
                crate::storage::queries::ShareAttemptReservation::PlacementCapacityReached => {
                    // Resolving this wave can free capacity. Keep this helper
                    // eligible and reassess after those outcomes are durable.
                    break;
                }
                crate::storage::queries::ShareAttemptReservation::StaleGeneration => {
                    return Err(stale_delivery_error(params));
                }
            }
        }

        if wave.is_empty() {
            break;
        }

        let placement_fleet = candidates.as_slice();
        let posts = wave.iter().map(|server_url| async move {
            // Waiting consumes the fan-out budget. Failing here means local
            // admission never granted a slot, however it failed, so the helper
            // was never contacted and nothing about it can be concluded. Both
            // reasons clear the reservation as a definite failure; only budget
            // expiry is the throttling the pass reports.
            let _permit = match super::post_capacity::acquire(deadline, cancel).await {
                Ok(permit) => permit,
                Err(error) => return Ok(WaveAttempt::NotAdmitted(error)),
            };
            // A permit wait can outlive the durable generation or its attempt.
            // Validate the starting wallet's journal again before any POST.
            let current_delivery = load_current_delivery_state(
                db,
                scope,
                params,
                generation.nullifier(),
                placement_fleet,
            )?;
            if !current_delivery.in_flight_urls().contains(server_url) {
                return Err(stale_delivery_error(params));
            }
            let remaining_time = deadline.saturating_duration_since(tokio::time::Instant::now());
            Ok(WaveAttempt::Posted(
                tokio::time::timeout_at(
                    deadline,
                    client.submit_share_with_timeout(
                        server_url,
                        params.share_wire_json,
                        params.now_seconds,
                        cancel,
                        remaining_time,
                        Some(deadline),
                    ),
                )
                .await,
            ))
        });
        let helper_post_outcomes = futures_util::future::join_all(posts).await;

        // Every wave member's outcome is written before any of them reports a
        // failure. `join_all` has already run all of them to completion, so by
        // this point each answer is something the process observed, and
        // returning at the first bad one left the rest of the wave sitting in
        // `attempting_urls`: a helper that answered `queued` was re-POSTed as
        // an interrupted attempt on the next pass, discarding an acceptance
        // this process saw. Ordering is still safe because every write is
        // guarded on the generation and refuses a stale one on its own, so a
        // later write cannot mask an earlier staleness.
        let mut deadline_elapsed = false;
        let mut stale_generation = false;
        let mut wave_failure = None;
        for (server_url, helper_post_outcome) in wave.iter().zip(helper_post_outcomes) {
            let helper_post_outcome = match helper_post_outcome {
                Ok(helper_post_outcome) => helper_post_outcome,
                Err(error) => {
                    wave_failure.get_or_insert(error);
                    continue;
                }
            };
            let attempt = share::ShareDeliveryAttemptParams {
                round_id: params.round_id,
                bundle_index: params.bundle_index,
                proposal_id: params.proposal_id,
                share_index: params.share_index,
                server_url,
                target_count: definite_acceptance_target,
                submit_at: params.submit_at,
            };
            // The client returns a held definite error rather than sleeping a
            // retry backoff into this deadline, so an elapse can only cancel an
            // in-flight HTTP attempt, whose transport outcome is genuinely
            // unknown; retain it for polling.
            let posted = match helper_post_outcome {
                WaveAttempt::Posted(posted) => posted,
                WaveAttempt::NotAdmitted(reason) => {
                    // Nothing was sent, so this says nothing about the helper.
                    // The reservation is released as a definite failure, which
                    // clears the marker and leaves the helper eligible for a
                    // later pass, but the pass must not go on to report an
                    // empty result as though a helper had refused: that is how
                    // a share ends with no accepted and no ambiguous answers,
                    // which `DeliveryProgress` reads as `Incomplete` and vote
                    // completion turns into a hard step failure.
                    if !share::resolve_delivery_attempt_for_generation(
                        db,
                        &attempt,
                        generation,
                        share::ShareDeliveryAttemptOutcome::DefiniteFailure,
                        false,
                    )? {
                        stale_generation = true;
                        continue;
                    }
                    // Cancellation also never contacted the helper, but it is
                    // not throttling and the pass is ending anyway: the batch
                    // report's own cancellation flag is what a caller reads.
                    if matches!(reason, crate::HelperError::DeadlineExceeded) {
                        never_admitted_a_candidate = true;
                    }
                    continue;
                }
            };
            helper_answered = true;
            let (outcome, elapsed) = match posted {
                Err(_) => (share::ShareDeliveryAttemptOutcome::Ambiguous, true),
                Ok(Ok(_)) => (share::ShareDeliveryAttemptOutcome::Accepted, false),
                Ok(Err(error)) if error.is_ambiguous() => {
                    (share::ShareDeliveryAttemptOutcome::Ambiguous, false)
                }
                Ok(Err(_)) => (share::ShareDeliveryAttemptOutcome::DefiniteFailure, false),
            };
            if !share::resolve_delivery_attempt_for_generation(
                db, &attempt, generation, outcome, false,
            )? {
                stale_generation = true;
                continue;
            }
            match outcome {
                share::ShareDeliveryAttemptOutcome::Accepted => {
                    delivery_state.mark_accepted(server_url)?
                }
                share::ShareDeliveryAttemptOutcome::Ambiguous => {
                    delivery_state.mark_outcome_unknown(server_url)?
                }
                share::ShareDeliveryAttemptOutcome::DefiniteFailure => {}
            }
            deadline_elapsed |= elapsed;
        }
        if stale_generation {
            return Err(stale_delivery_error(params));
        }
        if let Some(error) = wave_failure {
            return Err(error);
        }
        // Local admission only expires once the delivery budget is nearly
        // spent, so there is nothing left to try in this pass. Stopping here
        // keeps the remaining candidates untouched rather than reserving them
        // into a queue that will expire the same way; their durable rows are
        // clean, so a later pass starts from a full fleet.
        if never_admitted_a_candidate {
            break;
        }

        if deadline_elapsed || stop_after_wave {
            break;
        }
    }
    delivery_state =
        load_current_delivery_state(db, scope, params, &expected_nullifier, &candidates)?;
    Ok(delivery_report(
        &delivery_state,
        params.target_count,
        never_admitted_a_candidate && !helper_answered,
    ))
}

fn load_current_delivery_state(
    db: &VotingDb,
    scope: &share::ShareOperationScope,
    params: &InitialShareSubmissionParams<'_>,
    expected_nullifier: &[u8],
    candidates: &[String],
) -> Result<share::ShareDeliveryState, VotingError> {
    let current_delivery = share::list_for_scope(db, scope, params.round_id)?
        .into_iter()
        .find(|share| {
            share.bundle_index == params.bundle_index
                && share.proposal_id == params.proposal_id
                && share.share_index == params.share_index
                && share.nullifier == expected_nullifier
        })
        .ok_or_else(|| stale_delivery_error(params))?;
    let definite_acceptance_urls = current_delivery
        .sent_to_urls
        .into_iter()
        .filter(|url| candidates.contains(url))
        .collect::<Vec<_>>();
    let outcome_unknown_urls = current_delivery
        .ambiguous_urls
        .into_iter()
        .filter(|url| candidates.contains(url))
        .collect::<Vec<_>>();
    let interrupted_attempt_urls = current_delivery
        .attempting_urls
        .into_iter()
        .filter(|url| candidates.contains(url))
        .collect::<Vec<_>>();
    share::ShareDeliveryState::from_url_lists(
        &definite_acceptance_urls,
        &outcome_unknown_urls,
        &interrupted_attempt_urls,
    )
}

fn delivery_report(
    delivery_state: &share::ShareDeliveryState,
    target_count: usize,
    local_capacity_exhausted: bool,
) -> ShareSubmissionReport {
    ShareSubmissionReport {
        accepted_urls: delivery_state.accepted_urls().to_vec(),
        ambiguous_urls: delivery_state
            .outcome_unknown_urls()
            .iter()
            .chain(delivery_state.in_flight_urls())
            .cloned()
            .collect(),
        target_count,
        local_capacity_exhausted,
    }
}

fn stale_delivery_error(params: &InitialShareSubmissionParams<'_>) -> VotingError {
    VotingError::InvalidInput {
        message: format!(
            "committed share changed while helper delivery was in flight for \
             round={}, bundle={}, proposal={}, share={}",
            params.round_id, params.bundle_index, params.proposal_id, params.share_index
        ),
    }
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
    expected_vote_commitment: &[u8; 32],
    payloads: &[SharePayload],
    request: CommittedShareSubmissionRequest<'_>,
    expected_commitment_bundle_json: &str,
    scope: &share::ShareOperationScope,
    cancel: &(dyn Fn() -> bool + Send + Sync),
) -> Result<ShareSubmissionReport, VotingError> {
    // Batch steps name an anchor proposal, but each concurrent share must
    // override that inherited identity before any of its transport work.
    let observations = client.observation_scope();
    observations.bind_round_id(round_id);
    let observations = observations.attributed(crate::ObservationAttribution {
        bundle_index: Some(bundle_index),
        proposal_id: Some(proposal_id),
        share_index: Some(request.share_index),
    });
    let observed_client = client.observing(&observations);
    let client = &observed_client;
    let configured = ConfiguredHelperFleet::new(request.configured_server_urls)?;
    let planning_fleet = ConfiguredHelperFleet::new(request.planning_server_urls)?;
    let planned = canonical_helper_url_list(&request.plan.target_servers)?;
    if planned.len() != request.plan.target_servers.len() {
        return Err(VotingError::InvalidInput {
            message: "plan target_servers must contain distinct canonical helpers".to_string(),
        });
    }
    let expected_target = share_submission_target_count(planning_fleet.len());
    let planned_target = usize::try_from(request.plan.target_count).unwrap_or(usize::MAX);
    if planned_target != expected_target || planned.len() != planned_target {
        return Err(VotingError::InvalidInput {
            message: format!(
                "plan target_count and target_servers must match the persisted planning fleet target {expected_target}"
            ),
        });
    }
    if let Some(server_url) = planned.iter().find(|url| !planning_fleet.contains(url)) {
        return Err(VotingError::InvalidInput {
            message: format!("planned helper is not in the persisted planning fleet: {server_url}"),
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
    let (vc_tree_position, expected_nullifier) =
        match vote_recovery::helper_recovery_material_for_wallet(
            db,
            scope.wallet_id(),
            round_id,
            bundle_index,
            proposal_id,
        )? {
            vote_recovery::HelperRecoveryMaterial::Ready(bundle) => {
                if bundle.commitment_bundle_json != expected_commitment_bundle_json {
                    return Err(VotingError::InvalidInput {
                        message: format!(
                            "committed vote changed before helper share submission for \
                             round={round_id}, bundle={bundle_index}, proposal={proposal_id}; \
                             recover the current committed vote"
                        ),
                    });
                }
                let recovery = crate::vote::parse_recovery(&bundle.commitment_bundle_json)?;
                if recovery.vote_commitment != *expected_vote_commitment {
                    return Err(VotingError::InvalidInput {
                        message: format!(
                            "committed vote changed before helper share submission for \
                             round={round_id}, bundle={bundle_index}, proposal={proposal_id}; \
                             recover the current committed vote"
                        ),
                    });
                }
                let expected_nullifier = share::nullifier_from_recovery_json(
                    &bundle.commitment_bundle_json,
                    proposal_id,
                    request.share_index,
                )?;
                (bundle.vc_tree_position, expected_nullifier)
            }
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
    // Validate the complete typed payload before the durable row is created.
    // A continuation may replace this requested schedule with the write-once
    // value returned by persistence.
    let requested_share_wire_json =
        payload.to_wire_json(Some(vc_tree_position), request.plan.submit_at)?;
    let mut candidates = planned
        .into_iter()
        .filter(|url| configured.contains(url))
        .collect::<Vec<_>>();
    let eligible_planned_count = candidates.len();
    let fallback = configured
        .urls()
        .iter()
        .filter(|url| !candidates.contains(url))
        .cloned()
        .collect::<Vec<_>>();
    candidates.extend(fallback);
    let requested_params = InitialShareSubmissionParams {
        round_id,
        bundle_index,
        proposal_id,
        share_index: request.share_index,
        share_wire_json: &requested_share_wire_json,
        #[cfg(test)]
        planned_servers: &candidates[..eligible_planned_count],
        #[cfg(test)]
        fallback_servers: &candidates[eligible_planned_count..],
        target_count: planned_target,
        submit_at: request.plan.submit_at,
        now_seconds: request.now_seconds,
    };
    let (durable_submit_at, persisted_delivery) = prepare_committed_share_delivery(
        db,
        scope,
        &requested_params,
        expected_commitment_bundle_json,
        &expected_nullifier,
    )?;
    let share_wire_json = payload.to_wire_json(Some(vc_tree_position), durable_submit_at)?;
    let durable_params = InitialShareSubmissionParams {
        share_wire_json: &share_wire_json,
        submit_at: durable_submit_at,
        ..requested_params
    };
    dispatch_share_to_canonical_helpers(
        db,
        scope,
        client,
        &durable_params,
        &candidates[..eligible_planned_count],
        &candidates[eligible_planned_count..],
        persisted_delivery,
        cancel,
    )
    .await
}
