//! Pure share delivery and recovery workflow reducer.
//!
//! Wallets still own HTTP, persistence, timers, and UI. This module owns the
//! cross-wallet decisions about which helper IO should happen next and which
//! storage side effects should be applied after host IO completes.

use serde::{Deserialize, Serialize};

use crate::share_policy::{
    initial_share_delivery_random_bytes_required, next_initial_share_targets,
    plan_share_recovery_actions, resubmission_server_order,
    resubmission_server_order_random_bytes_required, ShareTimingPolicy,
};
use crate::types::{ShareDelegationRecord, VotingError};

/// Stable share identifier used by workflow actions.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ShareWorkflowKey {
    pub round_id: String,
    pub bundle_index: u32,
    pub proposal_id: u32,
    pub share_index: u32,
}

impl From<&ShareDelegationRecord> for ShareWorkflowKey {
    fn from(share: &ShareDelegationRecord) -> Self {
        Self {
            round_id: share.round_id.clone(),
            bundle_index: share.bundle_index,
            proposal_id: share.proposal_id,
            share_index: share.share_index,
        }
    }
}

/// Planned share for initial helper delivery.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareDeliveryPlan {
    pub key: ShareWorkflowKey,
    pub submit_at: u64,
    pub target_count: u64,
    pub target_servers: Vec<String>,
}

/// Per-share state for initial helper delivery.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareDeliveryShareState {
    pub key: ShareWorkflowKey,
    pub submit_at: u64,
    pub target_count: u64,
    pub target_servers: Vec<String>,
    pub accepted_server_urls: Vec<String>,
    pub tried_server_urls: Vec<String>,
    pub recorded: bool,
}

impl From<ShareDeliveryPlan> for ShareDeliveryShareState {
    fn from(plan: ShareDeliveryPlan) -> Self {
        Self {
            key: plan.key,
            submit_at: plan.submit_at,
            target_count: plan.target_count,
            target_servers: plan.target_servers,
            accepted_server_urls: Vec::new(),
            tried_server_urls: Vec::new(),
            recorded: false,
        }
    }
}

/// Initial share delivery state. Hosts should persist this between workflow
/// calls if their process can be suspended while network requests are in flight.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareDeliveryState {
    pub available_server_urls: Vec<String>,
    pub shares: Vec<ShareDeliveryShareState>,
    pub next_share_index: usize,
    pub finished: bool,
}

/// Resubmission state for one overdue share.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareResubmissionState {
    pub key: ShareWorkflowKey,
    pub server_order: Vec<String>,
    pub next_server_index: usize,
    pub accepted_server_url: Option<String>,
    pub finished: bool,
}

/// Result of a host POST to one helper.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharePostResult {
    pub key: ShareWorkflowKey,
    pub server_url: String,
    pub accepted: bool,
}

/// Result of a host helper-status lookup.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareStatusResult {
    pub key: ShareWorkflowKey,
    pub server_url: String,
    pub confirmed: bool,
}

/// Workflow request. The host performs returned actions, then feeds IO results
/// back through another request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ShareWorkflowRequest {
    StartDelivery {
        shares: Vec<ShareDeliveryPlan>,
        available_server_urls: Vec<String>,
    },
    ApplyDeliveryResults {
        state: ShareDeliveryState,
        results: Vec<SharePostResult>,
    },
    StartResubmission {
        key: ShareWorkflowKey,
        configured_server_urls: Vec<String>,
        sent_to_urls: Vec<String>,
    },
    ApplyResubmissionResult {
        state: ShareResubmissionState,
        result: SharePostResult,
    },
    PlanRecoveryPoll {
        shares: Vec<ShareDelegationRecord>,
        now_seconds: u64,
        vote_end_time_seconds: u64,
    },
    ApplyRecoveryStatusResults {
        shares: Vec<ShareDelegationRecord>,
        status_results: Vec<ShareStatusResult>,
        now_seconds: u64,
        vote_end_time_seconds: u64,
    },
}

/// Host action selected by the shared workflow.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ShareWorkflowAction {
    PostShare {
        key: ShareWorkflowKey,
        server_url: String,
        submit_at: u64,
        resubmission: bool,
    },
    FetchShareStatus {
        key: ShareWorkflowKey,
        server_url: String,
        round_id: String,
        nullifier: Vec<u8>,
    },
    RecordShareDelegation {
        key: ShareWorkflowKey,
        sent_to_urls: Vec<String>,
        submit_at: u64,
    },
    MarkShareConfirmed {
        key: ShareWorkflowKey,
    },
    AddSentServers {
        key: ShareWorkflowKey,
        server_urls: Vec<String>,
    },
    StartResubmission {
        key: ShareWorkflowKey,
        sent_to_urls: Vec<String>,
    },
    ScheduleWakeup {
        delay_seconds: u64,
    },
    DeliveryComplete,
    DeliveryFailed {
        key: Option<ShareWorkflowKey>,
        reason: String,
    },
    ResubmissionComplete {
        key: ShareWorkflowKey,
        accepted_server_url: Option<String>,
    },
}

/// Workflow response. At most one state field is populated for stateful delivery
/// or resubmission steps.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareWorkflowResponse {
    pub delivery_state: Option<ShareDeliveryState>,
    pub resubmission_state: Option<ShareResubmissionState>,
    pub actions: Vec<ShareWorkflowAction>,
}

/// Return the entropy needed by `apply_share_workflow_request`.
pub fn share_workflow_random_bytes_required(
    request: &ShareWorkflowRequest,
) -> Result<usize, VotingError> {
    match request {
        ShareWorkflowRequest::StartDelivery {
            shares,
            available_server_urls,
        } => delivery_random_bytes_required(
            &ShareDeliveryState {
                available_server_urls: available_server_urls.clone(),
                shares: shares.iter().cloned().map(Into::into).collect(),
                next_share_index: 0,
                finished: false,
            },
            &[],
        ),
        ShareWorkflowRequest::ApplyDeliveryResults { state, results } => {
            delivery_random_bytes_required(state, results)
        }
        ShareWorkflowRequest::StartResubmission {
            configured_server_urls,
            sent_to_urls,
            ..
        } => Ok(resubmission_server_order_random_bytes_required(
            configured_server_urls,
            sent_to_urls,
        )),
        ShareWorkflowRequest::ApplyResubmissionResult { state, result } => {
            if result.accepted || state.finished {
                Ok(0)
            } else {
                Ok(0)
            }
        }
        ShareWorkflowRequest::PlanRecoveryPoll { .. }
        | ShareWorkflowRequest::ApplyRecoveryStatusResults { .. } => Ok(0),
    }
}

/// Apply a share workflow request using caller-provided CSPRNG bytes.
pub fn apply_share_workflow_request(
    request: ShareWorkflowRequest,
    random_bytes: &[u8],
) -> Result<ShareWorkflowResponse, VotingError> {
    match request {
        ShareWorkflowRequest::StartDelivery {
            shares,
            available_server_urls,
        } => {
            let mut state = ShareDeliveryState {
                available_server_urls,
                shares: shares.into_iter().map(Into::into).collect(),
                next_share_index: 0,
                finished: false,
            };
            validate_delivery_state(&state)?;
            let actions = plan_delivery_actions(&mut state, random_bytes)?;
            Ok(ShareWorkflowResponse {
                delivery_state: Some(state),
                resubmission_state: None,
                actions,
            })
        }
        ShareWorkflowRequest::ApplyDeliveryResults { mut state, results } => {
            validate_delivery_state(&state)?;
            apply_delivery_results_to_state(&mut state, &results)?;
            let actions = plan_delivery_actions(&mut state, random_bytes)?;
            Ok(ShareWorkflowResponse {
                delivery_state: Some(state),
                resubmission_state: None,
                actions,
            })
        }
        ShareWorkflowRequest::StartResubmission {
            key,
            configured_server_urls,
            sent_to_urls,
        } => {
            let server_order =
                resubmission_server_order(&configured_server_urls, &sent_to_urls, random_bytes)?;
            let mut state = ShareResubmissionState {
                key,
                server_order,
                next_server_index: 0,
                accepted_server_url: None,
                finished: false,
            };
            let actions = plan_resubmission_actions(&mut state);
            Ok(ShareWorkflowResponse {
                delivery_state: None,
                resubmission_state: Some(state),
                actions,
            })
        }
        ShareWorkflowRequest::ApplyResubmissionResult { mut state, result } => {
            apply_resubmission_result_to_state(&mut state, &result)?;
            let actions = plan_resubmission_actions(&mut state);
            Ok(ShareWorkflowResponse {
                delivery_state: None,
                resubmission_state: Some(state),
                actions,
            })
        }
        ShareWorkflowRequest::PlanRecoveryPoll {
            shares,
            now_seconds,
            vote_end_time_seconds,
        } => Ok(ShareWorkflowResponse {
            delivery_state: None,
            resubmission_state: None,
            actions: plan_recovery_poll_actions(&shares, now_seconds, vote_end_time_seconds),
        }),
        ShareWorkflowRequest::ApplyRecoveryStatusResults {
            shares,
            status_results,
            now_seconds,
            vote_end_time_seconds,
        } => Ok(ShareWorkflowResponse {
            delivery_state: None,
            resubmission_state: None,
            actions: apply_recovery_status_results(
                &shares,
                &status_results,
                now_seconds,
                vote_end_time_seconds,
            ),
        }),
    }
}

fn delivery_random_bytes_required(
    state: &ShareDeliveryState,
    results: &[SharePostResult],
) -> Result<usize, VotingError> {
    let mut state = state.clone();
    validate_delivery_state(&state)?;
    apply_delivery_results_to_state(&mut state, results)?;
    let mut required = 0usize;
    while !state.finished && state.next_share_index < state.shares.len() {
        let share = &state.shares[state.next_share_index];
        if share.recorded {
            state.next_share_index += 1;
            continue;
        }
        let accepted_count = available_accepted_count(share, &state.available_server_urls);
        if accepted_count >= share.target_count as usize {
            state.next_share_index += 1;
            continue;
        }
        required = required.saturating_add(initial_share_delivery_random_bytes_required(
            &share.target_servers,
            &state.available_server_urls,
            &share.accepted_server_urls,
            &share.tried_server_urls,
        ));
        break;
    }
    Ok(required)
}

fn validate_delivery_state(state: &ShareDeliveryState) -> Result<(), VotingError> {
    require_unique_servers(&state.available_server_urls, "available_server_urls")?;
    if state.shares.is_empty() {
        return Err(VotingError::InvalidInput {
            message: "share delivery requires at least one share".to_string(),
        });
    }
    if state.next_share_index > state.shares.len() {
        return Err(VotingError::InvalidInput {
            message: "next_share_index is out of bounds".to_string(),
        });
    }
    for share in &state.shares {
        if share.target_count == 0 {
            return Err(VotingError::InvalidInput {
                message: "target_count must be greater than zero".to_string(),
            });
        }
        require_unique_servers(&share.target_servers, "target_servers")?;
        require_unique_servers(&share.accepted_server_urls, "accepted_server_urls")?;
        require_unique_servers(&share.tried_server_urls, "tried_server_urls")?;
    }
    Ok(())
}

fn require_unique_servers(servers: &[String], field: &str) -> Result<(), VotingError> {
    let mut seen = std::collections::HashSet::new();
    for server in servers {
        if !seen.insert(server.as_str()) {
            return Err(VotingError::InvalidInput {
                message: format!("{field} must not contain duplicate helper URLs"),
            });
        }
    }
    Ok(())
}

fn apply_delivery_results_to_state(
    state: &mut ShareDeliveryState,
    results: &[SharePostResult],
) -> Result<(), VotingError> {
    for result in results {
        let Some(share) = state.shares.iter_mut().find(|share| {
            share.key == result.key && share.tried_server_urls.contains(&result.server_url)
        }) else {
            return Err(VotingError::InvalidInput {
                message: "post result does not match an in-flight delivery target".to_string(),
            });
        };
        if result.accepted {
            push_unique(&mut share.accepted_server_urls, result.server_url.clone());
        } else {
            state
                .available_server_urls
                .retain(|server| server != &result.server_url);
        }
    }
    Ok(())
}

fn plan_delivery_actions(
    state: &mut ShareDeliveryState,
    random_bytes: &[u8],
) -> Result<Vec<ShareWorkflowAction>, VotingError> {
    let mut actions = Vec::new();
    let mut random_offset = 0usize;

    loop {
        if state.finished {
            return Ok(actions);
        }
        if state.next_share_index >= state.shares.len() {
            state.finished = true;
            actions.push(ShareWorkflowAction::DeliveryComplete);
            return Ok(actions);
        }

        let share = &mut state.shares[state.next_share_index];
        let accepted_count = available_accepted_count(share, &state.available_server_urls);
        if !share.recorded && accepted_count >= share.target_count as usize {
            let sent_to_urls = share.accepted_server_urls.clone();
            share.recorded = true;
            actions.push(ShareWorkflowAction::RecordShareDelegation {
                key: share.key.clone(),
                sent_to_urls,
                submit_at: share.submit_at,
            });
            state.next_share_index += 1;
            continue;
        }

        let needed = initial_share_delivery_random_bytes_required(
            &share.target_servers,
            &state.available_server_urls,
            &share.accepted_server_urls,
            &share.tried_server_urls,
        );
        let random_slice = random_bytes.get(random_offset..).unwrap_or(&[]);
        let targets = next_initial_share_targets(
            share.target_count,
            &share.target_servers,
            &state.available_server_urls,
            &share.accepted_server_urls,
            &share.tried_server_urls,
            random_slice,
        )?;
        random_offset += needed;

        if targets.is_empty() {
            if accepted_count > 0 {
                let sent_to_urls = share.accepted_server_urls.clone();
                share.recorded = true;
                actions.push(ShareWorkflowAction::RecordShareDelegation {
                    key: share.key.clone(),
                    sent_to_urls,
                    submit_at: share.submit_at,
                });
                state.next_share_index += 1;
                continue;
            }
            state.finished = true;
            actions.push(ShareWorkflowAction::DeliveryFailed {
                key: Some(share.key.clone()),
                reason: "no reachable helper servers for share delivery".to_string(),
            });
            return Ok(actions);
        }

        for server_url in targets {
            push_unique(&mut share.tried_server_urls, server_url.clone());
            actions.push(ShareWorkflowAction::PostShare {
                key: share.key.clone(),
                server_url,
                submit_at: share.submit_at,
                resubmission: false,
            });
        }
        return Ok(actions);
    }
}

fn available_accepted_count(
    share: &ShareDeliveryShareState,
    available_server_urls: &[String],
) -> usize {
    share
        .accepted_server_urls
        .iter()
        .filter(|server| available_server_urls.contains(*server))
        .count()
}

fn plan_resubmission_actions(state: &mut ShareResubmissionState) -> Vec<ShareWorkflowAction> {
    if state.finished {
        return Vec::new();
    }
    if let Some(server_url) = &state.accepted_server_url {
        state.finished = true;
        return vec![
            ShareWorkflowAction::AddSentServers {
                key: state.key.clone(),
                server_urls: vec![server_url.clone()],
            },
            ShareWorkflowAction::ResubmissionComplete {
                key: state.key.clone(),
                accepted_server_url: Some(server_url.clone()),
            },
        ];
    }
    if state.next_server_index >= state.server_order.len() {
        state.finished = true;
        return vec![ShareWorkflowAction::ResubmissionComplete {
            key: state.key.clone(),
            accepted_server_url: None,
        }];
    }
    let server_url = state.server_order[state.next_server_index].clone();
    state.next_server_index += 1;
    vec![ShareWorkflowAction::PostShare {
        key: state.key.clone(),
        server_url,
        submit_at: 0,
        resubmission: true,
    }]
}

fn apply_resubmission_result_to_state(
    state: &mut ShareResubmissionState,
    result: &SharePostResult,
) -> Result<(), VotingError> {
    if state.key != result.key {
        return Err(VotingError::InvalidInput {
            message: "post result does not match resubmission share".to_string(),
        });
    }
    if result.accepted {
        state.accepted_server_url = Some(result.server_url.clone());
    }
    Ok(())
}

fn plan_recovery_poll_actions(
    shares: &[ShareDelegationRecord],
    now_seconds: u64,
    vote_end_time_seconds: u64,
) -> Vec<ShareWorkflowAction> {
    let plan = plan_share_recovery_actions(
        shares,
        now_seconds,
        vote_end_time_seconds,
        ShareTimingPolicy::default(),
    );
    let mut actions = Vec::new();
    for ready_key in plan.ready_for_status_check {
        for share in shares
            .iter()
            .filter(|share| ShareWorkflowKey::from(*share) == key_from_policy(&ready_key))
        {
            for server_url in &share.sent_to_urls {
                actions.push(ShareWorkflowAction::FetchShareStatus {
                    key: ShareWorkflowKey::from(share),
                    server_url: server_url.clone(),
                    round_id: share.round_id.clone(),
                    nullifier: share.nullifier.clone(),
                });
            }
        }
    }
    if actions.is_empty() {
        if let Some(delay_seconds) = plan.next_delay_seconds {
            actions.push(ShareWorkflowAction::ScheduleWakeup { delay_seconds });
        }
    }
    actions
}

fn apply_recovery_status_results(
    shares: &[ShareDelegationRecord],
    status_results: &[ShareStatusResult],
    now_seconds: u64,
    vote_end_time_seconds: u64,
) -> Vec<ShareWorkflowAction> {
    let plan = plan_share_recovery_actions(
        shares,
        now_seconds,
        vote_end_time_seconds,
        ShareTimingPolicy::default(),
    );
    let overdue_keys: std::collections::HashSet<_> = plan
        .overdue_for_resubmission
        .iter()
        .map(key_from_policy)
        .collect();
    let mut actions = Vec::new();

    for share in shares.iter().filter(|share| !share.confirmed) {
        let key = ShareWorkflowKey::from(share);
        let share_results: Vec<&ShareStatusResult> = status_results
            .iter()
            .filter(|result| result.key == key)
            .collect();
        if share_results.is_empty() {
            continue;
        }
        if share_results.iter().any(|result| result.confirmed) {
            actions.push(ShareWorkflowAction::MarkShareConfirmed { key });
        } else if overdue_keys.contains(&key) {
            actions.push(ShareWorkflowAction::StartResubmission {
                key,
                sent_to_urls: share.sent_to_urls.clone(),
            });
        }
    }

    if actions.is_empty() {
        if let Some(delay_seconds) = plan.next_delay_seconds {
            actions.push(ShareWorkflowAction::ScheduleWakeup { delay_seconds });
        }
    }
    actions
}

fn key_from_policy(key: &crate::share_policy::ShareDelegationKey) -> ShareWorkflowKey {
    ShareWorkflowKey {
        round_id: key.round_id.clone(),
        bundle_index: key.bundle_index,
        proposal_id: key.proposal_id,
        share_index: key.share_index,
    }
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !values.contains(&value) {
        values.push(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROUND: &str = "aabb";

    fn key(share_index: u32) -> ShareWorkflowKey {
        ShareWorkflowKey {
            round_id: ROUND.to_string(),
            bundle_index: 0,
            proposal_id: 1,
            share_index,
        }
    }

    fn delivery_plan(
        share_index: u32,
        target_count: u64,
        target_servers: &[&str],
    ) -> ShareDeliveryPlan {
        ShareDeliveryPlan {
            key: key(share_index),
            submit_at: 1000 + share_index as u64,
            target_count,
            target_servers: target_servers
                .iter()
                .map(|server| server.to_string())
                .collect(),
        }
    }

    fn random_bytes(samples: &[u64]) -> Vec<u8> {
        samples
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect()
    }

    fn post_actions(response: &ShareWorkflowResponse) -> Vec<(ShareWorkflowKey, String)> {
        response
            .actions
            .iter()
            .filter_map(|action| match action {
                ShareWorkflowAction::PostShare {
                    key, server_url, ..
                } => Some((key.clone(), server_url.clone())),
                _ => None,
            })
            .collect()
    }

    fn share(submit_at: u64, created_at: u64) -> ShareDelegationRecord {
        ShareDelegationRecord {
            round_id: ROUND.to_string(),
            bundle_index: 0,
            proposal_id: 1,
            share_index: 0,
            sent_to_urls: vec!["https://helper-one.example.com".to_string()],
            nullifier: vec![7; 32],
            confirmed: false,
            submit_at,
            created_at,
        }
    }

    #[test]
    fn delivery_backfills_failed_target_for_same_share_then_later_share() {
        let request = ShareWorkflowRequest::StartDelivery {
            shares: vec![
                delivery_plan(0, 1, &["https://offline.example.com"]),
                delivery_plan(
                    1,
                    2,
                    &[
                        "https://offline.example.com",
                        "https://online-one.example.com",
                    ],
                ),
            ],
            available_server_urls: vec![
                "https://offline.example.com".to_string(),
                "https://online-one.example.com".to_string(),
                "https://online-two.example.com".to_string(),
            ],
        };
        let response = apply_share_workflow_request(request, &[]).unwrap();
        assert_eq!(
            post_actions(&response),
            vec![(key(0), "https://offline.example.com".to_string())]
        );

        let state = response.delivery_state.unwrap();
        let response = apply_share_workflow_request(
            ShareWorkflowRequest::ApplyDeliveryResults {
                state,
                results: vec![SharePostResult {
                    key: key(0),
                    server_url: "https://offline.example.com".to_string(),
                    accepted: false,
                }],
            },
            &random_bytes(&[1]),
        )
        .unwrap();
        assert_eq!(
            post_actions(&response),
            vec![(key(0), "https://online-one.example.com".to_string())]
        );

        let state = response.delivery_state.unwrap();
        let response = apply_share_workflow_request(
            ShareWorkflowRequest::ApplyDeliveryResults {
                state,
                results: vec![SharePostResult {
                    key: key(0),
                    server_url: "https://online-one.example.com".to_string(),
                    accepted: true,
                }],
            },
            &random_bytes(&[0]),
        )
        .unwrap();
        assert!(response.actions.iter().any(|action| matches!(
            action,
            ShareWorkflowAction::RecordShareDelegation { key: record_key, sent_to_urls, .. }
                if record_key == &key(0)
                    && sent_to_urls == &vec!["https://online-one.example.com".to_string()]
        )));
        assert_eq!(
            post_actions(&response),
            vec![
                (key(1), "https://online-one.example.com".to_string()),
                (key(1), "https://online-two.example.com".to_string())
            ]
        );
    }

    #[test]
    fn delivery_preserves_target_count_with_backfill_after_partial_failure() {
        let request = ShareWorkflowRequest::StartDelivery {
            shares: vec![delivery_plan(
                0,
                2,
                &[
                    "https://offline.example.com",
                    "https://online-one.example.com",
                ],
            )],
            available_server_urls: vec![
                "https://offline.example.com".to_string(),
                "https://online-one.example.com".to_string(),
                "https://online-two.example.com".to_string(),
            ],
        };
        let response = apply_share_workflow_request(request, &[]).unwrap();
        assert_eq!(post_actions(&response).len(), 2);

        let state = response.delivery_state.unwrap();
        let response = apply_share_workflow_request(
            ShareWorkflowRequest::ApplyDeliveryResults {
                state,
                results: vec![
                    SharePostResult {
                        key: key(0),
                        server_url: "https://offline.example.com".to_string(),
                        accepted: false,
                    },
                    SharePostResult {
                        key: key(0),
                        server_url: "https://online-one.example.com".to_string(),
                        accepted: true,
                    },
                ],
            },
            &random_bytes(&[0]),
        )
        .unwrap();
        assert_eq!(
            post_actions(&response),
            vec![(key(0), "https://online-two.example.com".to_string())]
        );
    }

    #[test]
    fn delivery_records_share_after_target_count_is_met() {
        let request = ShareWorkflowRequest::StartDelivery {
            shares: vec![delivery_plan(0, 1, &["https://one.example.com"])],
            available_server_urls: vec!["https://one.example.com".to_string()],
        };
        let response = apply_share_workflow_request(request, &[]).unwrap();
        let state = response.delivery_state.unwrap();
        let response = apply_share_workflow_request(
            ShareWorkflowRequest::ApplyDeliveryResults {
                state,
                results: vec![SharePostResult {
                    key: key(0),
                    server_url: "https://one.example.com".to_string(),
                    accepted: true,
                }],
            },
            &[],
        )
        .unwrap();
        assert!(response.actions.iter().any(|action| matches!(
            action,
            ShareWorkflowAction::RecordShareDelegation { sent_to_urls, .. }
                if sent_to_urls == &vec!["https://one.example.com".to_string()]
        )));
        assert!(response
            .actions
            .iter()
            .any(|action| matches!(action, ShareWorkflowAction::DeliveryComplete)));
    }

    #[test]
    fn recovery_fetches_ready_shares_and_starts_overdue_resubmission() {
        let shares = vec![share(0, 100)];
        let response = apply_share_workflow_request(
            ShareWorkflowRequest::PlanRecoveryPoll {
                shares: shares.clone(),
                now_seconds: 130,
                vote_end_time_seconds: 200,
            },
            &[],
        )
        .unwrap();
        assert!(matches!(
            response.actions.first(),
            Some(ShareWorkflowAction::FetchShareStatus { .. })
        ));

        let response = apply_share_workflow_request(
            ShareWorkflowRequest::ApplyRecoveryStatusResults {
                shares,
                status_results: vec![ShareStatusResult {
                    key: key(0),
                    server_url: "https://helper-one.example.com".to_string(),
                    confirmed: false,
                }],
                now_seconds: 130,
                vote_end_time_seconds: 200,
            },
            &[],
        )
        .unwrap();
        assert!(matches!(
            response.actions.first(),
            Some(ShareWorkflowAction::StartResubmission { .. })
        ));
    }

    #[test]
    fn resubmission_posts_untried_helpers_then_records_success() {
        let response = apply_share_workflow_request(
            ShareWorkflowRequest::StartResubmission {
                key: key(0),
                configured_server_urls: vec![
                    "https://old.example.com".to_string(),
                    "https://new.example.com".to_string(),
                ],
                sent_to_urls: vec!["https://old.example.com".to_string()],
            },
            &random_bytes(&[0, 0]),
        )
        .unwrap();
        assert_eq!(
            post_actions(&response),
            vec![(key(0), "https://new.example.com".to_string())]
        );
        let state = response.resubmission_state.unwrap();
        let response = apply_share_workflow_request(
            ShareWorkflowRequest::ApplyResubmissionResult {
                state,
                result: SharePostResult {
                    key: key(0),
                    server_url: "https://new.example.com".to_string(),
                    accepted: true,
                },
            },
            &[],
        )
        .unwrap();
        assert!(response.actions.iter().any(|action| matches!(
            action,
            ShareWorkflowAction::AddSentServers { server_urls, .. }
                if server_urls == &vec!["https://new.example.com".to_string()]
        )));
    }
}
