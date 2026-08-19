//! Durable pacing policy for vote-chain delegation submissions.
//!
//! The jitter is derived deterministically from local wallet context and the
//! accepted transaction hash, so it remains stable across process restarts
//! without exposing a fixed one-minute cadence.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{round::VotingDb, VotingError};

/// Default lower bound for the randomized delay between accepted delegations.
pub const DELEGATION_STAGGER_MIN_SECONDS: u64 = 45;
/// Default upper bound for the randomized delay between accepted delegations.
pub const DELEGATION_STAGGER_MAX_SECONDS: u64 = 75;

/// Controls wallet-side pacing of vote-chain delegation submissions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DelegationPacingPolicy {
    min_delay_seconds: u64,
    max_delay_seconds: u64,
}

impl DelegationPacingPolicy {
    /// Creates a policy whose delay is sampled inclusively from `min..=max`.
    pub fn new(min_delay_seconds: u64, max_delay_seconds: u64) -> Result<Self, VotingError> {
        if min_delay_seconds > max_delay_seconds {
            return Err(VotingError::InvalidInput {
                message: format!(
                    "delegation pacing minimum {min_delay_seconds} exceeds maximum {max_delay_seconds}"
                ),
            });
        }
        Ok(Self {
            min_delay_seconds,
            max_delay_seconds,
        })
    }

    pub fn min_delay_seconds(self) -> u64 {
        self.min_delay_seconds
    }

    pub fn max_delay_seconds(self) -> u64 {
        self.max_delay_seconds
    }
}

impl Default for DelegationPacingPolicy {
    fn default() -> Self {
        Self {
            min_delay_seconds: DELEGATION_STAGGER_MIN_SECONDS,
            max_delay_seconds: DELEGATION_STAGGER_MAX_SECONDS,
        }
    }
}

/// Eligibility of the next pending delegation bundle.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DelegationSubmissionEligibility {
    /// Exactly this bundle may be submitted now.
    Ready { bundle_index: u32 },
    /// Keep the bundle queued until this Unix timestamp.
    WaitUntil { bundle_index: u32, submit_at: u64 },
    /// None of the supplied bundles still needs submission.
    Complete,
}

impl DelegationSubmissionEligibility {
    /// Returns the stable discriminator used by wallet bridges.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Ready { .. } => "ready",
            Self::WaitUntil { .. } => "wait_until",
            Self::Complete => "complete",
        }
    }
}

/// Selects at most one delegation bundle for wallet submission.
///
/// `pending_bundle_indexes` should come from the current round plan's
/// `NextStep::Delegate` entries. The caller must serialize calls and replan
/// after recording an accepted transaction. `safe_deadline` is the last Unix
/// timestamp at which a delegation may safely be accepted; the policy
/// compresses delays as needed to fit every pending bundle before it.
pub fn delegation_submission_eligibility(
    db: &VotingDb,
    round_id: &str,
    pending_bundle_indexes: &[u32],
    now: u64,
    safe_deadline: Option<u64>,
) -> Result<DelegationSubmissionEligibility, VotingError> {
    delegation_submission_eligibility_with_policy(
        db,
        round_id,
        pending_bundle_indexes,
        now,
        safe_deadline,
        DelegationPacingPolicy::default(),
    )
}

/// Policy-configurable form of [`delegation_submission_eligibility`].
pub fn delegation_submission_eligibility_with_policy(
    db: &VotingDb,
    round_id: &str,
    pending_bundle_indexes: &[u32],
    now: u64,
    safe_deadline: Option<u64>,
    policy: DelegationPacingPolicy,
) -> Result<DelegationSubmissionEligibility, VotingError> {
    let mut pending = pending_bundle_indexes.to_vec();
    pending.sort_unstable();
    pending.dedup();

    let mut unsubmitted = Vec::with_capacity(pending.len());
    for bundle_index in pending {
        if db.get_delegation_tx_hash(round_id, bundle_index)?.is_none() {
            unsubmitted.push(bundle_index);
        }
    }
    let Some(&bundle_index) = unsubmitted.first() else {
        return Ok(DelegationSubmissionEligibility::Complete);
    };

    let Some((_, previous_tx_hash, previous_submitted_at)) =
        db.latest_delegation_submission(round_id)?
    else {
        return Ok(DelegationSubmissionEligibility::Ready { bundle_index });
    };

    let sampled_delay = sampled_delay_seconds(
        policy,
        &db.wallet_id(),
        round_id,
        &previous_tx_hash,
        bundle_index,
    );
    let effective_delay = match safe_deadline {
        Some(deadline) if deadline <= previous_submitted_at => 0,
        Some(deadline) => {
            let available = deadline - previous_submitted_at;
            let remaining_count = u64::try_from(unsubmitted.len()).unwrap_or(u64::MAX);
            sampled_delay.min(available / remaining_count.max(1))
        }
        None => sampled_delay,
    };
    let submit_at = previous_submitted_at.saturating_add(effective_delay);

    if submit_at <= now {
        Ok(DelegationSubmissionEligibility::Ready { bundle_index })
    } else {
        Ok(DelegationSubmissionEligibility::WaitUntil {
            bundle_index,
            submit_at,
        })
    }
}

fn sampled_delay_seconds(
    policy: DelegationPacingPolicy,
    wallet_id: &str,
    round_id: &str,
    previous_tx_hash: &str,
    bundle_index: u32,
) -> u64 {
    let span = policy
        .max_delay_seconds
        .saturating_sub(policy.min_delay_seconds)
        .saturating_add(1);
    if span <= 1 {
        return policy.min_delay_seconds;
    }

    let mut hasher = Sha256::new();
    hasher.update(b"zcash-voting/delegation-stagger/v1");
    hasher.update(wallet_id.as_bytes());
    hasher.update(round_id.as_bytes());
    hasher.update(previous_tx_hash.as_bytes());
    hasher.update(bundle_index.to_le_bytes());
    let digest = hasher.finalize();
    let entropy = u64::from_le_bytes(digest[..8].try_into().expect("SHA-256 prefix"));
    policy.min_delay_seconds + (entropy % span)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{governance::BALLOT_DIVISOR, round::RoundParams, types::NoteInfo, Network};

    const ROUND: &str = "0101010101010101010101010101010101010101010101010101010101010101";
    const WALLET: &str = "pacing-wallet";

    fn note(position: u64) -> NoteInfo {
        NoteInfo {
            commitment: vec![position as u8; 32],
            nullifier: vec![position as u8 + 1; 32],
            value: BALLOT_DIVISOR,
            position,
            diversifier: vec![position as u8; 11],
            rho: vec![position as u8 + 2; 32],
            rseed: vec![position as u8 + 3; 32],
            scope: 0,
            ufvk_str: "uview1pacing".to_string(),
        }
    }

    fn db_with_bundles(bundle_count: usize) -> VotingDb {
        let db = VotingDb::open_in_memory().unwrap();
        initialize_bundles(&db, bundle_count);
        db
    }

    fn initialize_bundles(db: &VotingDb, bundle_count: usize) {
        db.set_wallet_id(WALLET);
        db.create_round(
            Network::Testnet,
            &RoundParams {
                vote_round_id: ROUND.to_string(),
                snapshot_height: 100,
                ea_pk: vec![0xEA; 32],
                nc_root: vec![0xAA; 32],
                nullifier_imt_root: vec![0xBB; 32],
            },
            None,
        )
        .unwrap();
        let notes: Vec<_> = (0..bundle_count * crate::BUNDLE_NOTE_SLOTS)
            .map(|position| note(position as u64))
            .collect();
        db.ensure_bundles(ROUND, &notes).unwrap();
    }

    #[test]
    fn first_bundle_is_immediately_ready_in_canonical_order() {
        let db = db_with_bundles(3);

        let eligibility =
            delegation_submission_eligibility(&db, ROUND, &[2, 1, 1, 0], 1_000, None).unwrap();

        assert_eq!(
            eligibility,
            DelegationSubmissionEligibility::Ready { bundle_index: 0 }
        );
    }

    #[test]
    fn failed_submission_does_not_advance_queue() {
        let db = db_with_bundles(2);

        let first = delegation_submission_eligibility(&db, ROUND, &[0, 1], 1_000, None).unwrap();
        let retry = delegation_submission_eligibility(&db, ROUND, &[0, 1], 1_030, None).unwrap();

        assert_eq!(first, retry);
        assert_eq!(
            retry,
            DelegationSubmissionEligibility::Ready { bundle_index: 0 }
        );
    }

    #[test]
    fn accepted_submission_gates_exactly_one_next_bundle() {
        let db = db_with_bundles(3);
        db.mark_delegation_submitted_at(ROUND, 0, "accepted-0", 1_000)
            .unwrap();

        let waiting =
            delegation_submission_eligibility(&db, ROUND, &[0, 1, 2], 1_000, None).unwrap();
        let DelegationSubmissionEligibility::WaitUntil {
            bundle_index,
            submit_at,
        } = waiting
        else {
            panic!("next bundle should be delayed");
        };
        assert_eq!(bundle_index, 1);
        assert!((1_045..=1_075).contains(&submit_at));

        assert_eq!(
            delegation_submission_eligibility(&db, ROUND, &[0, 1, 2], submit_at, None).unwrap(),
            DelegationSubmissionEligibility::Ready { bundle_index: 1 }
        );
    }

    #[test]
    fn persisted_timestamp_keeps_restart_plan_stable() {
        let path =
            std::env::temp_dir().join(format!("voting-pacing-{}.sqlite", uuid::Uuid::new_v4()));
        let path_string = path.to_string_lossy().into_owned();
        let first = {
            let db = VotingDb::open(&path_string).unwrap();
            initialize_bundles(&db, 2);
            db.mark_delegation_submitted_at(ROUND, 0, "accepted-0", 5_000)
                .unwrap();
            delegation_submission_eligibility(&db, ROUND, &[1], 5_001, None).unwrap()
        };
        let replanned = {
            let db = VotingDb::open(&path_string).unwrap();
            db.set_wallet_id(WALLET);
            delegation_submission_eligibility(&db, ROUND, &[1], 5_001, None).unwrap()
        };
        std::fs::remove_file(path).ok();

        assert_eq!(first, replanned);
    }

    #[test]
    fn deadline_compresses_remaining_delays_proportionally() {
        let db = db_with_bundles(3);
        db.mark_delegation_submitted_at(ROUND, 0, "accepted-0", 100)
            .unwrap();
        let fixed_minute = DelegationPacingPolicy::new(60, 60).unwrap();

        assert_eq!(
            delegation_submission_eligibility_with_policy(
                &db,
                ROUND,
                &[1, 2],
                100,
                Some(140),
                fixed_minute,
            )
            .unwrap(),
            DelegationSubmissionEligibility::WaitUntil {
                bundle_index: 1,
                submit_at: 120,
            }
        );
        assert_eq!(
            delegation_submission_eligibility_with_policy(
                &db,
                ROUND,
                &[1, 2],
                120,
                Some(140),
                fixed_minute,
            )
            .unwrap(),
            DelegationSubmissionEligibility::Ready { bundle_index: 1 }
        );
    }

    #[test]
    fn expired_deadline_falls_back_to_serial_immediate_submission() {
        let db = db_with_bundles(2);
        db.mark_delegation_submitted_at(ROUND, 0, "accepted-0", 100)
            .unwrap();

        assert_eq!(
            delegation_submission_eligibility(&db, ROUND, &[1], 101, Some(100)).unwrap(),
            DelegationSubmissionEligibility::Ready { bundle_index: 1 }
        );
    }

    #[test]
    fn repeated_recording_keeps_original_acceptance_time() {
        let db = db_with_bundles(1);
        db.mark_delegation_submitted_at(ROUND, 0, "accepted-0", 100)
            .unwrap();
        db.mark_delegation_submitted_at(ROUND, 0, "accepted-0", 200)
            .unwrap();

        assert_eq!(
            db.latest_delegation_submission(ROUND).unwrap(),
            Some((0, "accepted-0".to_string(), 100))
        );
        assert!(db
            .mark_delegation_submitted_at(ROUND, 0, "conflict", 300)
            .is_err());
        assert_eq!(
            db.latest_delegation_submission(ROUND).unwrap(),
            Some((0, "accepted-0".to_string(), 100))
        );
    }

    #[test]
    fn public_recording_path_stores_acceptance_time_with_hash() {
        let db = db_with_bundles(1);
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        crate::delegate::record_submission(&db, ROUND, 0, "accepted-0").unwrap();

        let after = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let (_, hash, submitted_at) = db.latest_delegation_submission(ROUND).unwrap().unwrap();
        assert_eq!(hash, "accepted-0");
        assert!((before..=after).contains(&submitted_at));
    }

    #[test]
    fn submitted_or_polled_bundles_are_not_returned_again() {
        let db = db_with_bundles(2);
        db.mark_delegation_submitted_at(ROUND, 0, "accepted-0", 100)
            .unwrap();

        assert_eq!(
            delegation_submission_eligibility(&db, ROUND, &[0], 200, None).unwrap(),
            DelegationSubmissionEligibility::Complete
        );
    }

    #[test]
    fn large_policy_delay_saturates_instead_of_overflowing() {
        let db = db_with_bundles(2);
        db.mark_delegation_submitted_at(ROUND, 0, "accepted-0", i64::MAX as u64)
            .unwrap();
        let maximum_delay = DelegationPacingPolicy::new(u64::MAX, u64::MAX).unwrap();

        assert_eq!(
            delegation_submission_eligibility_with_policy(
                &db,
                ROUND,
                &[1],
                i64::MAX as u64,
                None,
                maximum_delay,
            )
            .unwrap(),
            DelegationSubmissionEligibility::WaitUntil {
                bundle_index: 1,
                submit_at: u64::MAX,
            }
        );
    }

    #[test]
    fn pacing_policy_rejects_inverted_bounds() {
        assert!(DelegationPacingPolicy::new(76, 45).is_err());
    }
}
