//! The durable state of one round, read once.

use std::collections::BTreeMap;

use rusqlite::Transaction;

use crate::chain_submission::planning::{
    lifecycle_transaction_hashes, LifecycleTransactionHashes, PlanningTarget,
};
use crate::phases::{
    delegation_submission_statuses_on, share_phases_on, vote_submission_statuses_on,
    DelegationSubmissionStatus, SharePhase, VotePhase,
};
use crate::session::{load_ballot_intents, Decision};
use crate::share_policy::ImmediateShareKey;
use crate::share_tracking::persisted_round_immediate_key;
use crate::storage::queries;
use crate::types::{ShareDelegationRecord, VotingError};
use crate::vote::{parse_recovery, VoteRecoveryBundle};

/// One bundle's durable facts that planning reads beside its delegation
/// phase.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BundleSnapshot {
    /// The bundle came from a delegation capability import and advances
    /// without a signer.
    pub(crate) capability_imported: bool,
    /// The version-17 projection of the delegation transaction hash, the
    /// fallback when no lifecycle row reports one.
    pub(crate) delegation_tx_hash: Option<String>,
}

/// One vote row as planning sees it.
#[derive(Clone, Debug)]
pub(crate) struct VoteSnapshot {
    /// The choice the vote row stores.
    pub(crate) choice: u32,
    /// Canonical phase through the authoritative singleton or batch row.
    pub(crate) phase: VotePhase,
    /// The version-17 projection of the vote transaction hash.
    pub(crate) tx_hash: Option<String>,
    /// The confirmed vote-commitment tree position, once known.
    pub(crate) vc_tree_position: Option<i64>,
    /// The persisted recovery bundle: proof, shares, and batch membership.
    pub(crate) recovery: Option<VoteRecoveryBundle>,
}

/// Everything the planner reads about one wallet's round, taken in one
/// transaction so no write can fall between two of its parts.
#[derive(Clone, Debug)]
pub(crate) struct RoundSnapshot {
    pub(crate) round_id: String,
    /// Delegation phase and diagnostic per bundle, by bundle index.
    pub(crate) delegations: Vec<DelegationSubmissionStatus>,
    pub(crate) bundles: BTreeMap<u32, BundleSnapshot>,
    pub(crate) votes: BTreeMap<(u32, u32), VoteSnapshot>,
    /// Every helper-share row of the round.
    pub(crate) shares: Vec<ShareDelegationRecord>,
    /// `(bundle, proposal, share, phase)` per helper-share row.
    pub(crate) share_phases: Vec<(u32, u32, u32, SharePhase)>,
    pub(crate) intents: BTreeMap<u32, Decision>,
    lifecycle_hashes: LifecycleTransactionHashes,
    /// The immediate share a persisted helper plan designates, if any.
    pub(crate) persisted_immediate_share: Option<ImmediateShareKey>,
}

/// Reads the round's durable state on `tx`.
///
/// The transaction is the caller's read transaction; this function performs
/// no writes and takes no other connection.
pub(crate) fn load_round_snapshot(
    tx: &Transaction<'_>,
    wallet_id: &str,
    round_id: &str,
) -> Result<RoundSnapshot, VotingError> {
    let delegations = delegation_submission_statuses_on(tx, wallet_id, round_id)?;
    let bundles = queries::bundle_planning_rows(tx, round_id, wallet_id)?
        .into_iter()
        .map(|row| {
            (
                row.bundle_index,
                BundleSnapshot {
                    capability_imported: row.capability_imported,
                    delegation_tx_hash: row.delegation_tx_hash,
                },
            )
        })
        .collect();
    let phases: BTreeMap<(u32, u32), VotePhase> =
        vote_submission_statuses_on(tx, wallet_id, round_id)?
            .into_iter()
            .map(|status| ((status.bundle_index, status.proposal_id), status.phase))
            .collect();
    let mut votes = BTreeMap::new();
    for row in queries::vote_recovery_rows(tx, round_id, wallet_id)? {
        let key = (row.bundle_index, row.proposal_id);
        let phase = phases
            .get(&key)
            .copied()
            .ok_or_else(|| VotingError::Internal {
                message: format!(
                "vote row without a canonical phase for round={round_id}, bundle={}, proposal={}",
                row.bundle_index, row.proposal_id
            ),
            })?;
        let recovery = row
            .commitment_bundle_json
            .as_deref()
            .map(parse_recovery)
            .transpose()?;
        votes.insert(
            key,
            VoteSnapshot {
                choice: row.choice,
                phase,
                tx_hash: row.tx_hash,
                vc_tree_position: row.vc_tree_position,
                recovery,
            },
        );
    }
    let share_phases = share_phases_on(tx, wallet_id, round_id)?;
    let shares = queries::get_share_delegations(tx, round_id, wallet_id)?;
    let intents = load_ballot_intents(tx, round_id, wallet_id)?
        .into_iter()
        .collect();
    let lifecycle_hashes = lifecycle_transaction_hashes(tx, wallet_id, round_id)?;
    let persisted_immediate_share = persisted_round_immediate_key(tx, round_id, wallet_id)?;
    Ok(RoundSnapshot {
        round_id: round_id.to_string(),
        delegations,
        bundles,
        votes,
        shares,
        share_phases,
        intents,
        lifecycle_hashes,
        persisted_immediate_share,
    })
}

impl RoundSnapshot {
    /// Transaction hash to report for one bundle's delegation: the lifecycle
    /// row's hash, else the version-17 projection column.
    pub(crate) fn delegation_tx_hash(&self, bundle_index: u32) -> Option<String> {
        self.lifecycle_hashes
            .hash(bundle_index, PlanningTarget::Delegation)
            .or_else(|| {
                self.bundles
                    .get(&bundle_index)
                    .and_then(|bundle| bundle.delegation_tx_hash.clone())
            })
    }

    /// Transaction hash to report for one vote, singleton or batch member:
    /// the vote's own lifecycle row, else its projection column.
    pub(crate) fn vote_tx_hash(&self, bundle_index: u32, proposal_id: u32) -> Option<String> {
        self.lifecycle_hashes
            .hash(bundle_index, PlanningTarget::Vote { proposal_id })
            .or_else(|| {
                self.votes
                    .get(&(bundle_index, proposal_id))
                    .and_then(|vote| vote.tx_hash.clone())
            })
    }

    /// Transaction hash to report for one atomic batch: the batch row is
    /// authoritative in flight; a confirmed or migrated batch reports the
    /// anchor member's own hash.
    pub(crate) fn vote_batch_tx_hash(
        &self,
        bundle_index: u32,
        ordered_batch_digest: [u8; 32],
        anchor_proposal_id: u32,
    ) -> Option<String> {
        self.lifecycle_hashes
            .hash(
                bundle_index,
                PlanningTarget::VoteBatch {
                    ordered_batch_digest,
                },
            )
            .or_else(|| self.vote_tx_hash(bundle_index, anchor_proposal_id))
    }

    /// The persisted recovery bundles on `bundle_index`, in proposal order.
    pub(crate) fn bundle_recoveries(&self, bundle_index: u32) -> Vec<VoteRecoveryBundle> {
        self.votes
            .range((bundle_index, 0)..=(bundle_index, u32::MAX))
            .filter_map(|(_, vote)| vote.recovery.clone())
            .collect()
    }
}
