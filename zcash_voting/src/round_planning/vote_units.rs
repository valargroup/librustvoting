//! Grouping a round's votes into the units the chain dispatches: singletons
//! and atomic batches. A batch is one signed envelope, so it is planned,
//! retired, advanced, and recast as one thing; this module is the only
//! place that decides which votes belong together.

use std::collections::BTreeMap;

use crate::phases::VotePhase;
use crate::session::Decision;
use crate::types::VotingError;
use crate::vote::{assemble_vote_batch_recoveries, VoteRecoveryBundle};

use super::snapshot::{RoundSnapshot, VoteSnapshot};

/// The durable identity of one vote unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum VoteUnitId {
    /// One vote dispatched on its own.
    Singleton { bundle_index: u32, proposal_id: u32 },
    /// One atomic batch, identified by its ordered batch digest.
    Batch {
        bundle_index: u32,
        ordered_batch_digest: [u8; 32],
    },
}

/// One member of a vote unit.
#[derive(Clone, Debug)]
pub(crate) struct VoteMember {
    pub(crate) proposal_id: u32,
    /// The persisted recovery bundle, when the vote holds one.
    pub(crate) recovery: Option<VoteRecoveryBundle>,
}

/// A singleton vote or an atomic batch, with the facts planning reads.
#[derive(Clone, Debug)]
pub(crate) struct VoteUnit {
    pub(crate) id: VoteUnitId,
    pub(crate) bundle_index: u32,
    /// Members in batch order; one member for a singleton.
    pub(crate) members: Vec<VoteMember>,
    /// The unit's phase. Every member of a batch is in this phase.
    pub(crate) phase: VotePhase,
}

impl VoteUnit {
    /// The proposal that names the unit in a step: the singleton's proposal,
    /// or the batch's first ordered action.
    pub(crate) fn anchor_proposal_id(&self) -> u32 {
        self.members[0].proposal_id
    }

    pub(crate) fn proposal_ids(&self) -> impl Iterator<Item = &u32> {
        self.members.iter().map(|member| &member.proposal_id)
    }

    pub(crate) fn ordered_batch_digest(&self) -> Option<[u8; 32]> {
        match self.id {
            VoteUnitId::Singleton { .. } => None,
            VoteUnitId::Batch {
                ordered_batch_digest,
                ..
            } => Some(ordered_batch_digest),
        }
    }

    pub(crate) fn contains(&self, proposal_id: u32) -> bool {
        self.members
            .iter()
            .any(|member| member.proposal_id == proposal_id)
    }
}

/// Groups the snapshot's votes into units, in `(bundle, proposal)` order of
/// their anchors.
///
/// A vote whose recovery bundle names an atomic batch, and whose phase the
/// chain may still act on, is grouped with every other member of that batch.
/// Every other vote is its own unit. A persisted batch that is incomplete,
/// out of order, of mixed phases, claimed twice, conflicting with the
/// recorded ballot, or whose submitted members report different transaction
/// hashes is an `InvalidInput` error rather than a guess.
pub(crate) fn group_vote_units(
    snapshot: &RoundSnapshot,
    intents: &BTreeMap<u32, Decision>,
) -> Result<Vec<VoteUnit>, VotingError> {
    let round_id = snapshot.round_id.as_str();
    let mut units = Vec::new();
    let mut grouped: BTreeMap<(u32, u32), VoteUnitId> = BTreeMap::new();

    for (&(bundle_index, proposal_id), vote) in &snapshot.votes {
        if grouped.contains_key(&(bundle_index, proposal_id)) {
            continue;
        }
        let batch = match vote.phase {
            VotePhase::Committed | VotePhase::Submitted | VotePhase::SubmissionManaged => vote
                .recovery
                .as_ref()
                .and_then(|recovery| recovery.batch.clone()),
            VotePhase::Prepared
            | VotePhase::SubmittedWithoutHash
            | VotePhase::SubmissionRejected
            | VotePhase::Confirmed => None,
        };
        let Some(batch) = batch else {
            units.push(singleton_unit(bundle_index, proposal_id, vote));
            continue;
        };

        let recoveries = assemble_vote_batch_recoveries(
            round_id,
            bundle_index,
            batch.digest,
            snapshot.bundle_recoveries(bundle_index),
        )?;
        if recoveries.is_empty() {
            return Err(VotingError::InvalidInput {
                message: format!(
                    "persisted atomic vote batch is empty for round={round_id}, bundle={bundle_index}"
                ),
            });
        }
        let phase = vote.phase;
        let mut shared_tx_hash: Option<String> = None;
        let mut members = Vec::with_capacity(recoveries.len());
        for recovery in &recoveries {
            let member_key = (bundle_index, recovery.proposal_id);
            let member = snapshot.votes.get(&member_key).ok_or_else(|| {
                VotingError::InvalidInput {
                    message: format!(
                        "persisted atomic vote batch is missing proposal {} for round={round_id}, bundle={bundle_index}",
                        recovery.proposal_id
                    ),
                }
            })?;
            if member.phase != phase {
                return Err(VotingError::InvalidInput {
                    message: format!(
                        "persisted atomic vote batch has mixed phases for round={round_id}, bundle={bundle_index}: proposal {} is {}, expected {}",
                        recovery.proposal_id,
                        member.phase.as_str(),
                        phase.as_str()
                    ),
                });
            }
            // A member with no recorded intent is not a conflict: the batch is
            // lifecycle-owned and must stay schedulable whatever the host has
            // recorded so far. Only a differing or skipped intent conflicts.
            if member.choice != recovery.vote_decision
                || intents
                    .get(&recovery.proposal_id)
                    .is_some_and(|intent| *intent != Decision::Choice(recovery.vote_decision))
            {
                return Err(VotingError::InvalidInput {
                    message: format!(
                        "persisted atomic vote batch conflicts with ballot intent for round={round_id}, bundle={bundle_index}, proposal={}",
                        recovery.proposal_id
                    ),
                });
            }
            if phase == VotePhase::Submitted {
                // A batch generation reserved before its POST has no hash yet.
                // Every member that does report one must agree, because one
                // atomic batch is exactly one transaction.
                if let Some(tx_hash) = snapshot.vote_tx_hash(bundle_index, recovery.proposal_id) {
                    if shared_tx_hash
                        .as_ref()
                        .is_some_and(|expected| expected != &tx_hash)
                    {
                        return Err(VotingError::InvalidInput {
                            message: format!(
                                "submitted atomic vote batch has conflicting transaction hashes for round={round_id}, bundle={bundle_index}"
                            ),
                        });
                    }
                    shared_tx_hash = Some(tx_hash);
                }
            }
            members.push(VoteMember {
                proposal_id: recovery.proposal_id,
                recovery: member.recovery.clone(),
            });
        }

        let id = VoteUnitId::Batch {
            bundle_index,
            ordered_batch_digest: batch.digest,
        };
        for member in &members {
            if grouped
                .insert((bundle_index, member.proposal_id), id)
                .is_some()
            {
                return Err(VotingError::InvalidInput {
                    message: format!(
                        "vote belongs to more than one atomic batch for round={round_id}, bundle={bundle_index}, proposal={}",
                        member.proposal_id
                    ),
                });
            }
        }
        units.push(VoteUnit {
            id,
            bundle_index,
            members,
            phase,
        });
    }
    Ok(units)
}

fn singleton_unit(bundle_index: u32, proposal_id: u32, vote: &VoteSnapshot) -> VoteUnit {
    VoteUnit {
        id: VoteUnitId::Singleton {
            bundle_index,
            proposal_id,
        },
        bundle_index,
        members: vec![VoteMember {
            proposal_id,
            recovery: vote.recovery.clone(),
        }],
        phase: vote.phase,
    }
}
