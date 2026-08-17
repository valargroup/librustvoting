//! Stable helper-share planning and recovery API.
//!
//! This module wraps share nullifier computation, helper payload recovery, and
//! share-delegation persistence so wallets do not need direct access to
//! `share_delegations` SQL or recovery JSON internals.

use crate::{
    round::VotingDb,
    types::{
        ct_option_to_result, validate_share_index, ShareDelegationRecord, SharePayload,
        VotingError, WireEncryptedShare,
    },
    vote::{validate_recovery_bundle_vote_fields, VoteRecoveryBundle},
};
use ff::PrimeField;
use pasta_curves::pallas;
use serde::{Deserialize, Serialize};

pub use crate::types::ShareDelegationRecord as ShareRecord;

/// Share scheduling and retry policy helpers.
pub mod policy {
    pub use crate::share_policy::{
        is_last_moment, is_share_ready_for_status_check, last_moment_buffer_seconds,
        last_moment_deadline_seconds, next_tracking_delay_seconds, overdue_threshold_seconds,
        plan_share_submission, plan_share_submission_from_order, plan_share_submissions,
        resubmission_server_order, resubmission_server_order_from_configured_order,
        resubmission_server_order_from_groups, resubmission_server_order_random_bytes_required,
        scheduled_share_submit_at_from_entropy, scheduled_share_submit_at_from_random_unit,
        select_share_submission_targets, select_share_submission_targets_from_order,
        share_recovery_base_time, share_server_order_random_bytes_required,
        share_submission_random_bytes_required, share_submission_target_count,
        share_submit_at_random_bytes_required, should_resubmit_share, shuffled_share_server_order,
        summarize_share_tracking, ShareSubmissionPlan, ShareSubmissionRandomBytesRequired,
        ShareTimingPolicy, ShareTrackingSummary, LAST_MOMENT_BUFFER_FRACTION_DENOMINATOR,
        LAST_MOMENT_BUFFER_FRACTION_NUMERATOR, LAST_MOMENT_BUFFER_MAX_SECONDS,
        SHARE_SUBMIT_AT_MAX_DELAY_SECONDS,
    };
}

pub use policy::{ShareSubmissionPlan as SharePlan, ShareTimingPolicy, ShareTrackingSummary};

/// One helper-share plan keyed by its public share index.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexedShareSubmissionPlan {
    /// Public share index committed by the encrypted share.
    pub share_index: u32,
    /// Unix seconds when helpers should submit the share, or 0 for immediate.
    pub submit_at: u64,
    /// Number of helpers each share should reach.
    pub target_count: u32,
    /// Helper targets selected for initial share submission.
    pub target_servers: Vec<String>,
}

/// Plans one vote's helper-share timing and initial targets.
///
/// This is an additive wrapper around [`policy::plan_share_submissions`]. When
/// `submit_largest_share_now` is false, every plan keeps the existing randomized
/// timing policy and the result is ordered by public share index. When it is
/// true, the active share with the greatest `plaintext_value` is returned first
/// with `submit_at` set to `now_seconds`; equal values use the lowest public
/// share index. All remaining plans keep their sampled times and helper targets.
///
/// Callers should submit the returned plans in order. Sending the first share
/// now intentionally makes its public index timing-linkable as the vote's
/// largest share.
#[allow(clippy::too_many_arguments)]
pub fn plan_vote_share_submissions(
    bundle: &VoteRecoveryBundle,
    server_urls: &[String],
    now_seconds: u64,
    vote_end_time_seconds: u64,
    last_moment_buffer_seconds: Option<u64>,
    submit_largest_share_now: bool,
    submit_at_random_bytes: &[u8],
    server_random_bytes: &[u8],
) -> Result<Vec<IndexedShareSubmissionPlan>, VotingError> {
    validate_recovery_bundle_vote_fields(bundle)?;

    let active_share_count = if bundle.single_share {
        1.min(bundle.encrypted_shares.len())
    } else {
        bundle.encrypted_shares.len()
    };
    let mut active_shares = bundle
        .encrypted_shares
        .iter()
        .take(active_share_count)
        .collect::<Vec<_>>();
    if active_shares.is_empty() {
        return Err(VotingError::InvalidInput {
            message: "vote recovery bundle contains no active shares".to_string(),
        });
    }
    for share in &active_shares {
        validate_share_index(share.share_index)?;
    }
    active_shares.sort_by_key(|share| share.share_index);
    if let Some(duplicate) = active_shares
        .windows(2)
        .find(|pair| pair[0].share_index == pair[1].share_index)
    {
        return Err(VotingError::InvalidInput {
            message: format!(
                "vote recovery bundle contains duplicate share_index {}",
                duplicate[0].share_index
            ),
        });
    }

    let largest_share_index = submit_largest_share_now.then(|| largest_share_index(&active_shares));
    let plans = policy::plan_share_submissions(
        active_shares.len(),
        server_urls,
        now_seconds,
        vote_end_time_seconds,
        last_moment_buffer_seconds,
        bundle.single_share,
        submit_at_random_bytes,
        server_random_bytes,
    )?;
    let mut submissions = active_shares
        .into_iter()
        .zip(plans)
        .map(|(share, plan)| IndexedShareSubmissionPlan {
            share_index: share.share_index,
            submit_at: if largest_share_index == Some(share.share_index) {
                now_seconds
            } else {
                plan.submit_at
            },
            target_count: plan.target_count,
            target_servers: plan.target_servers,
        })
        .collect::<Vec<_>>();

    if let Some(largest_share_index) = largest_share_index {
        let position = submissions
            .iter()
            .position(|plan| plan.share_index == largest_share_index)
            .expect("selected share is present in the planned batch");
        let largest = submissions.remove(position);
        submissions.insert(0, largest);
    }

    Ok(submissions)
}

// The input is nonempty and sorted by share index.
fn largest_share_index(active_shares: &[&crate::types::EncryptedShare]) -> u32 {
    let mut largest = active_shares[0];
    for candidate in &active_shares[1..] {
        if candidate.plaintext_value > largest.plaintext_value
            || (candidate.plaintext_value == largest.plaintext_value
                && candidate.share_index < largest.share_index)
        {
            largest = candidate;
        }
    }
    largest.share_index
}

/// Computes the 32-byte share reveal nullifier.
pub fn compute_nullifier(
    vote_commitment: &[u8; 32],
    share_index: u32,
    primary_blind: &[u8; 32],
) -> Result<[u8; 32], VotingError> {
    if share_index > 15 {
        return Err(VotingError::InvalidInput {
            message: format!("share_index must be 0..15, got {share_index}"),
        });
    }

    let vc = ct_option_to_result(
        pallas::Base::from_repr(*vote_commitment),
        "invalid vote_commitment field element",
    )?;
    let blind = ct_option_to_result(
        pallas::Base::from_repr(*primary_blind),
        "invalid primary_blind field element",
    )?;
    let nullifier = voting_circuits::share_reveal::share_nullifier_hash(
        vc,
        pallas::Base::from(share_index as u64),
        blind,
    );
    Ok(nullifier.to_repr())
}

/// Records a helper-share submission using nullifier material from recovery state.
pub fn record(
    db: &VotingDb,
    round_id: &str,
    bundle_index: u32,
    proposal_id: u32,
    share_index: u32,
    sent_to_urls: &[String],
    submit_at: u64,
) -> Result<(), VotingError> {
    let bundle = crate::vote::recovery_bundle(db, round_id, bundle_index, proposal_id)?
        .ok_or_else(|| VotingError::InvalidInput {
            message: format!(
                "vote recovery bundle not found for round={round_id}, bundle={bundle_index}, proposal={proposal_id}"
            ),
        })?;
    let payload = recover_payload(&bundle, share_index)?;
    let primary_blind = array32("primary_blind", payload.primary_blind.clone())?;
    let nullifier = compute_nullifier(&bundle.vote_commitment, share_index, &primary_blind)?;
    db.record_share_delegation(
        round_id,
        bundle_index,
        proposal_id,
        share_index,
        sent_to_urls,
        &nullifier,
        submit_at,
    )
}

/// Lists all helper-share records for a round.
pub fn list(db: &VotingDb, round_id: &str) -> Result<Vec<ShareDelegationRecord>, VotingError> {
    db.get_share_delegations(round_id)
}

/// Lists unconfirmed helper-share records for retry and polling.
pub fn unconfirmed(
    db: &VotingDb,
    round_id: &str,
) -> Result<Vec<ShareDelegationRecord>, VotingError> {
    db.get_unconfirmed_delegations(round_id)
}

/// Marks one helper-share record confirmed.
pub fn confirm(
    db: &VotingDb,
    round_id: &str,
    bundle_index: u32,
    proposal_id: u32,
    share_index: u32,
) -> Result<(), VotingError> {
    db.mark_share_confirmed(round_id, bundle_index, proposal_id, share_index)
}

/// Adds helper URLs to an existing share record after resubmission.
pub fn add_sent_servers(
    db: &VotingDb,
    round_id: &str,
    bundle_index: u32,
    proposal_id: u32,
    share_index: u32,
    new_urls: &[String],
) -> Result<(), VotingError> {
    db.add_sent_servers(round_id, bundle_index, proposal_id, share_index, new_urls)
}

/// Reconstructs one helper-server payload from a persisted vote recovery bundle.
pub fn recover_payload(
    bundle: &VoteRecoveryBundle,
    share_index: u32,
) -> Result<SharePayload, VotingError> {
    recover_payloads(bundle)?
        .into_iter()
        .find(|payload| payload.enc_share.share_index == share_index)
        .ok_or_else(|| VotingError::InvalidInput {
            message: format!("share_index {share_index} not found in vote recovery bundle"),
        })
}

/// Reconstructs all helper-server payloads from a persisted vote recovery bundle.
pub fn recover_payloads(bundle: &VoteRecoveryBundle) -> Result<Vec<SharePayload>, VotingError> {
    validate_recovery_bundle_vote_fields(bundle)?;

    let all_enc_shares = bundle
        .encrypted_shares
        .iter()
        .map(WireEncryptedShare::from)
        .collect::<Vec<_>>();
    let iter_shares: &[WireEncryptedShare] = if bundle.single_share {
        &all_enc_shares[..1.min(all_enc_shares.len())]
    } else {
        &all_enc_shares
    };
    iter_shares
        .iter()
        .enumerate()
        .map(|(idx, share)| {
            let primary_blind =
                bundle
                    .share_blinds
                    .get(idx)
                    .ok_or_else(|| VotingError::InvalidInput {
                        message: format!("missing primary blind for encrypted share index {idx}"),
                    })?;
            Ok(SharePayload {
                vote_round_id: bundle.vote_round_id.clone(),
                shares_hash: bundle.shares_hash.to_vec(),
                proposal_id: bundle.proposal_id,
                vote_decision: bundle.vote_decision,
                enc_share: share.clone(),
                tree_position: bundle.vc_tree_position,
                all_enc_shares: all_enc_shares.clone(),
                share_comms: bundle
                    .share_comms
                    .iter()
                    .map(|comm| comm.to_vec())
                    .collect(),
                primary_blind: primary_blind.to_vec(),
            })
        })
        .collect()
}

/// Reconstructs one helper-server payload from persisted recovery JSON and
/// serializes it as helper wire JSON.
pub fn recover_wire_json(
    commitment_bundle_json: &str,
    proposal_id: u32,
    share_index: u32,
    vc_tree_position: u64,
    submit_at: u64,
) -> Result<String, VotingError> {
    let bundle = crate::vote::parse_recovery(commitment_bundle_json)?;
    if bundle.proposal_id != proposal_id {
        return Err(VotingError::InvalidInput {
            message: format!(
                "recovery proposal_id {} does not match requested {proposal_id}",
                bundle.proposal_id
            ),
        });
    }
    let payload = recover_payload(&bundle, share_index)?;
    payload.to_wire_json(Some(vc_tree_position), submit_at)
}

fn array32(label: &str, value: Vec<u8>) -> Result<[u8; 32], VotingError> {
    value
        .try_into()
        .map_err(|value: Vec<u8>| VotingError::Internal {
            message: format!("{label} must be 32 bytes, got {}", value.len()),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        round::RoundParams,
        storage::{queries, VotingDb},
        types::{EncryptedShare, NoteInfo},
        vote::{serialize_recovery, VoteRecoveryBundle},
    };

    const ROUND_ID: &str = "0101010101010101010101010101010101010101010101010101010101010101";
    const WALLET_ID: &str = "wallet";

    fn db_with_vote_recovery() -> VotingDb {
        let db = VotingDb::open_in_memory().unwrap();
        db.set_wallet_id(WALLET_ID);
        db.create_round(crate::Network::Testnet, &round_params(), None)
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
        db
    }

    fn round_params() -> RoundParams {
        RoundParams {
            vote_round_id: ROUND_ID.to_string(),
            snapshot_height: 1000,
            ea_pk: vec![0xEA; 32],
            nc_root: vec![0xAA; 32],
            nullifier_imt_root: vec![0xBB; 32],
        }
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

    #[test]
    fn share_recovery_payload_and_nullifier_happy_path() {
        let bundle = recovery_bundle_fixture();

        let payloads = recover_payloads(&bundle).unwrap();
        let payload = recover_payload(&bundle, 1).unwrap();
        let nullifier = compute_nullifier(&bundle.vote_commitment, 1, &field_bytes(2)).unwrap();

        assert_eq!(payloads.len(), 2);
        assert_eq!(payload.vote_round_id, ROUND_ID);
        assert_eq!(payload.enc_share.share_index, 1);
        assert_eq!(payload.all_enc_shares.len(), 2);
        assert_eq!(payload.share_comms[1], vec![0x52; 32]);
        assert_eq!(payload.primary_blind, field_bytes(2).to_vec());
        assert_eq!(nullifier.len(), 32);
    }

    #[test]
    fn recover_wire_json_uses_recovery_bundle_payload() {
        let bundle = recovery_bundle_fixture();
        let json = crate::vote::serialize_recovery(&bundle).unwrap();
        let wire_json = recover_wire_json(&json, 1, 1, 999, 123).unwrap();
        let value: serde_json::Value = serde_json::from_str(&wire_json).unwrap();
        assert_eq!(value["proposal_id"].as_u64().unwrap(), 1);
        assert_eq!(value["share_index"].as_u64().unwrap(), 1);
        assert_eq!(value["tree_position"].as_u64().unwrap(), 999);
        assert_eq!(value["submit_at"].as_u64().unwrap(), 123);
        assert_eq!(value["vote_round_id"].as_str().unwrap(), ROUND_ID);
        assert_eq!(value["enc_share"]["share_index"].as_u64().unwrap(), 1);
        assert!(
            value.get("all_enc_shares").is_none(),
            "recovered helper wire JSON does not include all_enc_shares"
        );
    }

    #[test]
    fn share_recovery_payloads_reject_invalid_vote_bounds() {
        let mut bundle = recovery_bundle_fixture();
        bundle.num_options = 1;
        assert!(recover_payloads(&bundle).is_err());

        let mut bundle = recovery_bundle_fixture();
        bundle.vote_decision = bundle.num_options;
        assert!(recover_payloads(&bundle).is_err());

        let mut bundle = recovery_bundle_fixture();
        bundle.vote_round_id = "AA".repeat(32);
        assert!(recover_payloads(&bundle).is_err());
    }

    #[test]
    fn vote_share_planning_preserves_existing_policy_when_disabled() {
        let bundle = recovery_bundle_fixture();
        let servers = helper_servers();
        let submit_at_entropy = scheduling_entropy();
        let server_entropy = vec![0; 16];
        let legacy = policy::plan_share_submissions(
            2,
            &servers,
            1_000,
            2_000,
            Some(100),
            false,
            &submit_at_entropy,
            &server_entropy,
        )
        .unwrap();

        let planned = plan_vote_share_submissions(
            &bundle,
            &servers,
            1_000,
            2_000,
            Some(100),
            false,
            &submit_at_entropy,
            &server_entropy,
        )
        .unwrap();

        assert_eq!(
            planned
                .iter()
                .map(|plan| plan.share_index)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
        for (indexed, legacy) in planned.iter().zip(legacy) {
            assert_eq!(indexed.submit_at, legacy.submit_at);
            assert_eq!(indexed.target_count, legacy.target_count);
            assert_eq!(indexed.target_servers, legacy.target_servers);
        }
    }

    #[test]
    fn vote_share_planning_returns_largest_share_first_at_current_time() {
        let bundle = recovery_bundle_fixture();
        let servers = helper_servers();
        let submit_at_entropy = scheduling_entropy();
        let server_entropy = vec![0; 16];
        let randomized = plan_vote_share_submissions(
            &bundle,
            &servers,
            1_000,
            2_000,
            Some(100),
            false,
            &submit_at_entropy,
            &server_entropy,
        )
        .unwrap();

        let planned = plan_vote_share_submissions(
            &bundle,
            &servers,
            1_000,
            2_000,
            Some(100),
            true,
            &submit_at_entropy,
            &server_entropy,
        )
        .unwrap();

        assert_eq!(randomized[0].submit_at, 1_225);
        assert_eq!(randomized[1].submit_at, 1_450);
        assert_eq!(planned[0].share_index, 1);
        assert_eq!(planned[0].submit_at, 1_000);
        assert_eq!(planned[1].share_index, 0);
        assert_eq!(planned[1].submit_at, 1_225);
        assert_eq!(planned[0].target_servers, randomized[1].target_servers);
        assert_eq!(planned[1].target_servers, randomized[0].target_servers);

        let serialized = serde_json::to_string(&planned).unwrap();
        assert!(!serialized.contains("plaintext_value"));
        assert!(!serialized.contains("randomness"));
    }

    #[test]
    fn vote_share_planning_preserves_complete_batch_target_spreading() {
        let mut bundle = recovery_bundle_fixture();
        bundle.encrypted_shares = (0..16)
            .map(|share_index| EncryptedShare {
                c1: vec![0x21; 32],
                c2: vec![0x22; 32],
                share_index,
                plaintext_value: if share_index == 9 {
                    100
                } else {
                    u64::from(share_index) + 1
                },
                randomness: vec![0x23; 32],
            })
            .collect();
        let servers = vec![
            "https://helper-1.example".to_string(),
            "https://helper-2.example".to_string(),
            "https://helper-3.example".to_string(),
        ];
        let required = policy::share_submission_random_bytes_required(
            16,
            servers.len(),
            1_000,
            2_000,
            Some(100),
            false,
        );
        let submit_at_entropy = vec![0x55; required.submit_at_random_bytes];
        let server_entropy = vec![0xAA; required.server_random_bytes];
        let legacy = policy::plan_share_submissions(
            16,
            &servers,
            1_000,
            2_000,
            Some(100),
            false,
            &submit_at_entropy,
            &server_entropy,
        )
        .unwrap();

        let planned = plan_vote_share_submissions(
            &bundle,
            &servers,
            1_000,
            2_000,
            Some(100),
            true,
            &submit_at_entropy,
            &server_entropy,
        )
        .unwrap();

        assert_eq!(planned[0].share_index, 9);
        for submission in &planned {
            let legacy = &legacy[submission.share_index as usize];
            let expected_submit_at = if submission.share_index == 9 {
                1_000
            } else {
                legacy.submit_at
            };
            assert_eq!(submission.submit_at, expected_submit_at);
            assert_eq!(submission.target_servers, legacy.target_servers);
        }
        for server in &servers {
            assert!(planned
                .iter()
                .any(|submission| !submission.target_servers.contains(server)));
        }
    }

    #[test]
    fn vote_share_planning_breaks_largest_value_ties_by_lowest_index() {
        let mut bundle = recovery_bundle_fixture();
        bundle.encrypted_shares[0].plaintext_value = 6;
        bundle.encrypted_shares.swap(0, 1);

        let planned = plan_vote_share_submissions(
            &bundle,
            &helper_servers(),
            1_000,
            2_000,
            Some(100),
            true,
            &scheduling_entropy(),
            &[0; 16],
        )
        .unwrap();

        assert_eq!(planned[0].share_index, 0);
        assert_eq!(planned[0].submit_at, 1_000);
        assert_eq!(planned[1].share_index, 1);
    }

    #[test]
    fn vote_share_planning_handles_single_share_and_rejects_duplicate_indices() {
        let mut single = recovery_bundle_fixture();
        single.single_share = true;
        let planned = plan_vote_share_submissions(
            &single,
            &helper_servers(),
            1_000,
            2_000,
            Some(100),
            true,
            &[],
            &[0; 8],
        )
        .unwrap();
        assert_eq!(planned.len(), 1);
        assert_eq!(planned[0].share_index, 0);
        assert_eq!(planned[0].submit_at, 1_000);

        let mut duplicate = recovery_bundle_fixture();
        duplicate.encrypted_shares[1].share_index = 0;
        let err = plan_vote_share_submissions(
            &duplicate,
            &helper_servers(),
            1_000,
            2_000,
            Some(100),
            false,
            &scheduling_entropy(),
            &[0; 16],
        )
        .unwrap_err();
        assert!(err.to_string().contains("duplicate share_index 0"));
    }

    #[test]
    fn share_tracking_apis_happy_path() {
        let db = db_with_vote_recovery();
        let initial_urls = vec!["https://helper-1.example".to_string()];
        record(&db, ROUND_ID, 0, 1, 1, &initial_urls, 99).unwrap();

        let records = list(&db, ROUND_ID).unwrap();
        let unconfirmed_records = unconfirmed(&db, ROUND_ID).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(unconfirmed_records.len(), 1);
        assert_eq!(records[0].share_index, 1);
        assert_eq!(records[0].sent_to_urls, initial_urls);
        assert!(!records[0].confirmed);

        add_sent_servers(
            &db,
            ROUND_ID,
            0,
            1,
            1,
            &["https://helper-2.example".to_string()],
        )
        .unwrap();
        let records = list(&db, ROUND_ID).unwrap();
        assert_eq!(records[0].sent_to_urls.len(), 2);
        assert_eq!(records[0].submit_at, 0);

        db.conn()
            .execute(
                "UPDATE share_delegations SET nullifier = :nullifier
                 WHERE round_id = :round_id
                   AND wallet_id = :wallet_id
                   AND bundle_index = 0
                   AND proposal_id = 1
                   AND share_index = 1",
                rusqlite::named_params! {
                    ":nullifier": vec![0xFF_u8; 32],
                    ":round_id": ROUND_ID,
                    ":wallet_id": WALLET_ID,
                },
            )
            .unwrap();
        let err = record(&db, ROUND_ID, 0, 1, 1, &initial_urls, 99).unwrap_err();
        assert!(
            err.to_string().contains("share nullifier conflict"),
            "unexpected error: {err}"
        );

        confirm(&db, ROUND_ID, 0, 1, 1).unwrap();
        assert!(unconfirmed(&db, ROUND_ID).unwrap().is_empty());
        assert_eq!(list(&db, ROUND_ID).unwrap()[0].confirmed, true);
    }

    #[test]
    fn share_policy_re_exports_are_callable() {
        assert_eq!(policy::share_submission_target_count(3), 2);
        assert_eq!(policy::SHARE_SUBMIT_AT_MAX_DELAY_SECONDS, 48 * 60 * 60);
        assert_eq!(
            policy::scheduled_share_submit_at_from_random_unit(10, 100, Some(10), false, 0.0)
                .unwrap(),
            10
        );
    }

    fn helper_servers() -> Vec<String> {
        vec![
            "https://helper-1.example".to_string(),
            "https://helper-2.example".to_string(),
        ]
    }

    fn scheduling_entropy() -> Vec<u8> {
        [1_u64 << 62, 1_u64 << 63]
            .into_iter()
            .flat_map(u64::to_le_bytes)
            .collect()
    }

    fn field_bytes(value: u8) -> [u8; 32] {
        let mut bytes = [0u8; 32];
        bytes[0] = value;
        bytes
    }
}
