//! The one rule table: from a snapshot, its vote units, and the ballot, to
//! the obligations the round still owes.
//!
//! Classification is pure. It has no clock and no network, and every fact
//! it uses comes from the snapshot or the authenticated roster.

use std::collections::{BTreeMap, BTreeSet};

use crate::phases::{DelegationPhase, VotePhase};
use crate::session::{BallotIntentClassification, Decision};
use crate::types::VotingError;

use super::lifecycle::{
    ballot_relation, delegation_phase_holds_bundle, roster_relation, vote_phase_holds_bundle,
    vote_phase_is_lifecycle_owned, BallotRelation, LifecyclePosition, RosterRelation,
};
use super::snapshot::RoundSnapshot;
use super::vote_units::{VoteUnit, VoteUnitId};

/// One proposal's draft in a fresh cast.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CastDraft {
    pub(crate) proposal_id: u32,
    pub(crate) choice: u32,
}

/// Why a fresh cast is withheld. The host resolves it; nothing is dispatched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BlockedReason {
    /// Proposals in the roster still have no terminal decision.
    OpenBallot(Vec<u32>),
    /// Durable intents outside the roster must be cleared first.
    UnrosteredIntents(Vec<u32>),
}

/// One unit of work the round still owes, carrying everything its
/// execution needs.
// Retirement and blocked reasons are consumed by executor dispatch, which
// moves onto obligations in the next change; the plan projects neither.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Debug)]
pub(crate) enum Obligation {
    /// The bundle still needs a signed delegation before a vote can be cast.
    Delegate { bundle_index: u32 },
    /// The bundle's delegation is durably in flight.
    AdvanceDelegation {
        bundle_index: u32,
        /// The delegation is a structurally imported capability that
        /// advances without a signer.
        imported: bool,
        phase: DelegationPhase,
        tx_hash: Option<String>,
    },
    /// An undispatched unit that can never be dispatched as it is: a member
    /// left the roster. It is cleared before the bundle's next cast.
    Retire { unit: VoteUnitId, members: Vec<u32> },
    /// The bundle casts every listed draft as one unit.
    Cast {
        bundle_index: u32,
        drafts: Vec<CastDraft>,
        prerequisite: Option<u32>,
    },
    /// A fresh cast is due on the bundle but withheld.
    Blocked {
        bundle_index: u32,
        reason: BlockedReason,
    },
    /// A committed or on-wire unit is driven through the chain lifecycle.
    ReconcileChain {
        unit: VoteUnitId,
        bundle_index: u32,
        ordered_proposal_ids: Vec<u32>,
        /// No POST has been reserved for the unit yet: it is resumed the way
        /// a fresh cast completes, helper plans before the broadcast.
        undispatched: bool,
        tx_hash: Option<String>,
        prerequisite: Option<u32>,
    },
    /// A confirmed vote owes these helper shares: they have no row yet.
    Deliver {
        bundle_index: u32,
        proposal_id: u32,
        vc_tree_position: u64,
        share_indexes: Vec<u32>,
        prerequisite: Option<u32>,
    },
    /// A submitted helper share awaits confirmation. One no helper has
    /// accepted yet blocks the foreground; one no helper has even reached
    /// (no acceptance, no ambiguous attempt, nothing in flight) cannot be
    /// confirmed by polling and is delivered again instead.
    Confirm {
        bundle_index: u32,
        proposal_id: u32,
        share_index: u32,
        /// At least one helper definitely holds the share.
        accepted: bool,
        /// Some attempt reached a helper with an unknown outcome, or is
        /// durably reserved and still in flight. Only tracking can classify
        /// it; redelivery would exclude that helper and make no progress.
        outcome_unknown: bool,
        prerequisite: Option<u32>,
    },
}

impl Obligation {
    /// The bundle whose delegation must clear before this vote or share work.
    pub(crate) fn bundle_index(&self) -> u32 {
        match self {
            Self::Delegate { bundle_index }
            | Self::AdvanceDelegation { bundle_index, .. }
            | Self::Cast { bundle_index, .. }
            | Self::Blocked { bundle_index, .. }
            | Self::ReconcileChain { bundle_index, .. }
            | Self::Deliver { bundle_index, .. }
            | Self::Confirm { bundle_index, .. } => *bundle_index,
            Self::Retire { unit, .. } => match unit {
                VoteUnitId::Singleton { bundle_index, .. }
                | VoteUnitId::Batch { bundle_index, .. } => *bundle_index,
            },
        }
    }
}

/// The classifier's output: the obligations plus the ballot facts the plan
/// reports beside them.
#[derive(Clone, Debug)]
pub(crate) struct RoundObligations {
    pub(crate) obligations: Vec<Obligation>,
    pub(crate) choice_proposals: Vec<u32>,
    pub(crate) open_proposals: Vec<u32>,
    /// Durable intents outside the roster the host can and must clear.
    pub(crate) unrostered_intents: Vec<u32>,
    /// Votes whose stored choice the ballot no longer agrees with. They are
    /// superseded by a cast and reserve nothing.
    pub(crate) stale_vote_keys: BTreeSet<(u32, u32)>,
}

/// Classifies `units` against `ballot` (the roster and intents already
/// classified from `snapshot.intents`).
pub(crate) fn classify(
    snapshot: &RoundSnapshot,
    units: &[VoteUnit],
    ballot: BallotIntentClassification,
) -> Result<RoundObligations, VotingError> {
    let round_id = snapshot.round_id.as_str();
    let intents = &snapshot.intents;
    let delegation: BTreeMap<u32, DelegationPhase> = snapshot
        .delegations
        .iter()
        .map(|status| (status.bundle_index, status.phase))
        .collect();
    let bundles: Vec<u32> = delegation.keys().copied().collect();
    let roster = ballot.roster;
    let choice_proposals = ballot.choice_proposals;
    let open_proposals = ballot.open_proposals;

    // An unrostered intent whose vote the chain lifecycle owns cannot be
    // cleared (`clear_ballot_intent` refuses it), so it is not something the
    // host can resolve: it is neither reported nor allowed to withhold
    // casting. Helper-plan derivation applies the same rule.
    let unrostered_intents: Vec<u32> = ballot
        .unrostered_intents
        .into_iter()
        .filter(|proposal_id| {
            !snapshot.votes.iter().any(|(&(_, vote_proposal_id), vote)| {
                vote_proposal_id == *proposal_id && vote_phase_is_lifecycle_owned(vote.phase)
            })
        })
        .collect();
    // Casting derives the round's single immediate helper share from the
    // complete set of choices, so helper-plan derivation rejects a roster
    // that still holds an undecided proposal, and equally a durable intent
    // for a proposal outside the authenticated roster. The durable intents
    // must exactly match the roster before a cast is planned.
    let roster_is_terminal = open_proposals.is_empty() && unrostered_intents.is_empty();

    let stale_vote_keys: BTreeSet<(u32, u32)> = snapshot
        .votes
        .iter()
        .filter(|(&(_, proposal_id), vote)| {
            ballot_relation(intents, proposal_id, vote.choice) == BallotRelation::Conflicts
        })
        .map(|(&key, _)| key)
        .collect();

    let mut obligations = Vec::new();

    // Undispatched units a departed member retires whole; the rostered
    // members are cast again below. A retired unit reserves nothing.
    let mut retirable_vote_keys = BTreeSet::new();
    for unit in units {
        if LifecyclePosition::of(Some(unit.phase)) != LifecyclePosition::Undispatched {
            continue;
        }
        if roster_relation(unit.proposal_ids(), &roster) == RosterRelation::LeftRoster {
            let members: Vec<u32> = unit.proposal_ids().copied().collect();
            retirable_vote_keys.extend(members.iter().map(|&p| (unit.bundle_index, p)));
            obligations.push(Obligation::Retire {
                unit: unit.id,
                members,
            });
        }
    }

    let held_bundles: BTreeSet<u32> = snapshot
        .votes
        .iter()
        .filter(|(key, vote)| {
            !stale_vote_keys.contains(key)
                && !retirable_vote_keys.contains(key)
                && vote_phase_holds_bundle(vote.phase)
        })
        .map(|(&(bundle_index, _), _)| bundle_index)
        .chain(
            delegation
                .iter()
                .filter(|(_, phase)| delegation_phase_holds_bundle(**phase))
                .map(|(&bundle_index, _)| bundle_index),
        )
        .collect();

    // A conflicting intent for anything the chain may have seen is an error,
    // never a recast: the proposal authority for that bundle has moved on.
    for &(bundle_index, proposal_id) in &stale_vote_keys {
        if snapshot
            .votes
            .get(&(bundle_index, proposal_id))
            .is_some_and(|vote| vote_phase_is_lifecycle_owned(vote.phase))
        {
            return Err(VotingError::InvalidInput {
                message: format!(
                    "round {round_id} bundle {bundle_index} proposal {proposal_id} has a submitted vote that conflicts with ballot intent"
                ),
            });
        }
    }

    let unit_of = |bundle_index: u32, proposal_id: u32| -> &VoteUnit {
        units
            .iter()
            .find(|unit| unit.bundle_index == bundle_index && unit.contains(proposal_id))
            .expect("every vote row is in exactly one unit")
    };
    let mut reconciled_units = BTreeSet::new();
    let mut reconcile = |obligations: &mut Vec<Obligation>,
                         unit: &VoteUnit|
     -> Result<(), VotingError> {
        if unit.phase == VotePhase::Submitted
            && unit.members.iter().any(|member| member.recovery.is_none())
        {
            let proposal_id = unit
                .members
                .iter()
                .find(|member| member.recovery.is_none())
                .map(|member| member.proposal_id)
                .expect("checked above");
            return Err(VotingError::InvalidInput {
                message: format!(
                    "round {round_id} bundle {} proposal {proposal_id} has a submitted vote without recovery material",
                    unit.bundle_index
                ),
            });
        }
        if !reconciled_units.insert(unit.id) {
            return Ok(());
        }
        let tx_hash = match unit.ordered_batch_digest() {
            Some(digest) => {
                snapshot.vote_batch_tx_hash(unit.bundle_index, digest, unit.anchor_proposal_id())
            }
            None => snapshot.vote_tx_hash(unit.bundle_index, unit.anchor_proposal_id()),
        };
        obligations.push(Obligation::ReconcileChain {
            unit: unit.id,
            bundle_index: unit.bundle_index,
            ordered_proposal_ids: unit.proposal_ids().copied().collect(),
            undispatched: LifecyclePosition::of(Some(unit.phase))
                == LifecyclePosition::Undispatched,
            tx_hash,
            prerequisite: None,
        });
        Ok(())
    };
    let mut delivered_votes = BTreeSet::new();
    let mut deliver = |obligations: &mut Vec<Obligation>,
                       bundle_index: u32,
                       proposal_id: u32|
     -> Result<(), VotingError> {
        if !delivered_votes.insert((bundle_index, proposal_id)) {
            return Ok(());
        }
        let share_indexes = missing_share_indexes(snapshot, bundle_index, proposal_id)?;
        if share_indexes.is_empty() {
            return Ok(());
        }
        let vc_tree_position = snapshot
            .confirmed_tree_position(bundle_index, proposal_id)?
            .ok_or_else(|| VotingError::Internal {
                message: format!(
                    "submit shares step missing vc_tree_position for round={round_id}, bundle={bundle_index}, proposal={proposal_id}"
                ),
            })?;
        obligations.push(Obligation::Deliver {
            bundle_index,
            proposal_id,
            vc_tree_position,
            share_indexes,
            prerequisite: None,
        });
        Ok(())
    };

    // Fresh casts and the work of votes that agree with the ballot.
    let mut bundles_needing_delegation = BTreeSet::new();
    let mut drafts: BTreeMap<u32, Vec<CastDraft>> = BTreeMap::new();
    let mut blocked: BTreeSet<u32> = BTreeSet::new();
    for &proposal_id in &choice_proposals {
        let intent_choice = match intents.get(&proposal_id) {
            Some(Decision::Choice(choice)) => *choice,
            _ => continue,
        };
        for &bundle_index in &bundles {
            let key = (bundle_index, proposal_id);
            let vote = snapshot.votes.get(&key);
            let cast_due = match vote {
                None => true,
                Some(vote) => {
                    stale_vote_keys.contains(&key)
                        || vote.choice != intent_choice
                        || retirable_vote_keys.contains(&key)
                        || LifecyclePosition::of(Some(vote.phase)) == LifecyclePosition::Uncast
                }
            };
            if cast_due {
                if held_bundles.contains(&bundle_index) {
                    continue;
                }
                // Delegation is a prerequisite either way, so it is still
                // planned while the voter decides the rest of the roster.
                bundles_needing_delegation.insert(bundle_index);
                if roster_is_terminal {
                    drafts.entry(bundle_index).or_default().push(CastDraft {
                        proposal_id,
                        choice: intent_choice,
                    });
                } else {
                    blocked.insert(bundle_index);
                }
                continue;
            }
            let vote = vote.expect("a due cast covers the missing row");
            match LifecyclePosition::of(Some(vote.phase)) {
                LifecyclePosition::Confirmed => {
                    deliver(&mut obligations, bundle_index, proposal_id)?
                }
                LifecyclePosition::Undispatched => {
                    // A unit is dispatched whole, so it is dispatched only
                    // when the ballot agrees with every member. A batch a
                    // member of which is still undecided holds its bundle and
                    // waits; the on-wire pass below owns nothing here.
                    let unit = unit_of(bundle_index, proposal_id);
                    let every_member_agrees = unit.members.iter().all(|member| {
                        roster.contains(&member.proposal_id)
                            && snapshot
                                .votes
                                .get(&(bundle_index, member.proposal_id))
                                .is_some_and(|member_vote| {
                                    ballot_relation(intents, member.proposal_id, member_vote.choice)
                                        == BallotRelation::Agrees
                                })
                    });
                    if every_member_agrees {
                        reconcile(&mut obligations, unit)?
                    }
                }
                LifecyclePosition::OnWire => {
                    reconcile(&mut obligations, unit_of(bundle_index, proposal_id))?
                }
                LifecyclePosition::Terminal | LifecyclePosition::Uncast => {}
            }
        }
    }

    // Advancement for votes already on the wire does not depend on ballot
    // intent. A lifecycle-owned or submitted vote whose proposal has no
    // recorded intent, or whose intent survives for a proposal the roster no
    // longer lists, is still the wallet's transaction and must be driven to
    // resolution; a skipped or differing intent was rejected above as a
    // conflict, and rostered proposals were planned above, so only
    // intent-less and unrostered proposals reach this pass. A confirmed
    // vote's shares are owed to the helpers whatever the roster now says.
    for (&(bundle_index, proposal_id), vote) in &snapshot.votes {
        if intents.contains_key(&proposal_id) && roster.contains(&proposal_id) {
            continue;
        }
        match LifecyclePosition::of(Some(vote.phase)) {
            LifecyclePosition::OnWire => {
                reconcile(&mut obligations, unit_of(bundle_index, proposal_id))?
            }
            LifecyclePosition::Confirmed => deliver(&mut obligations, bundle_index, proposal_id)?,
            LifecyclePosition::Uncast
            | LifecyclePosition::Undispatched
            | LifecyclePosition::Terminal => {}
        }
    }

    for (bundle_index, drafts) in drafts {
        obligations.push(Obligation::Cast {
            bundle_index,
            drafts,
            prerequisite: None,
        });
    }
    for bundle_index in blocked {
        let reason = if !open_proposals.is_empty() {
            BlockedReason::OpenBallot(open_proposals.clone())
        } else {
            BlockedReason::UnrosteredIntents(unrostered_intents.clone())
        };
        obligations.push(Obligation::Blocked {
            bundle_index,
            reason,
        });
    }

    // Delegation: resume any mid-flight delegation; otherwise only the
    // prerequisite for a bundle that still has a vote to cast.
    let mut delegation_bundles = BTreeSet::new();
    for (&bundle_index, phase) in &delegation {
        match phase {
            DelegationPhase::Confirmed => {}
            DelegationPhase::Submitted | DelegationPhase::SubmissionManaged => {
                let imported = snapshot
                    .bundles
                    .get(&bundle_index)
                    .map(|bundle| bundle.capability_imported)
                    .ok_or_else(|| VotingError::InvalidInput {
                        message: format!(
                            "bundle not found for round {round_id} index {bundle_index}"
                        ),
                    })?;
                delegation_bundles.insert(bundle_index);
                obligations.push(Obligation::AdvanceDelegation {
                    bundle_index,
                    imported,
                    phase: *phase,
                    tx_hash: snapshot.delegation_tx_hash(bundle_index),
                });
            }
            DelegationPhase::SubmittedWithoutHash | DelegationPhase::SubmissionRejected => {}
            DelegationPhase::Prepared | DelegationPhase::PcztBuilt | DelegationPhase::Proved => {
                if bundles_needing_delegation.contains(&bundle_index) {
                    delegation_bundles.insert(bundle_index);
                    obligations.push(Obligation::Delegate { bundle_index });
                }
            }
        }
    }

    // Confirm already-submitted helper shares.
    for &(bundle_index, proposal_id, share_index, phase) in &snapshot.share_phases {
        if stale_vote_keys.contains(&(bundle_index, proposal_id)) {
            continue;
        }
        match phase {
            crate::phases::SharePhase::Submitted => {
                let row = snapshot.shares.iter().find(|share| {
                    share.bundle_index == bundle_index
                        && share.proposal_id == proposal_id
                        && share.share_index == share_index
                });
                let accepted =
                    row.is_none_or(|share| share.confirmed || !share.sent_to_urls.is_empty());
                let outcome_unknown = row.is_some_and(|share| {
                    !share.ambiguous_urls.is_empty() || !share.attempting_urls.is_empty()
                });
                obligations.push(Obligation::Confirm {
                    bundle_index,
                    proposal_id,
                    share_index,
                    accepted,
                    outcome_unknown,
                    prerequisite: None,
                });
            }
            crate::phases::SharePhase::Confirmed => {}
        }
    }

    for obligation in &mut obligations {
        let bundle_index = obligation.bundle_index();
        let blocked_by = delegation_bundles
            .contains(&bundle_index)
            .then_some(bundle_index);
        match obligation {
            Obligation::Cast { prerequisite, .. }
            | Obligation::ReconcileChain { prerequisite, .. }
            | Obligation::Deliver { prerequisite, .. }
            | Obligation::Confirm { prerequisite, .. } => *prerequisite = blocked_by,
            Obligation::Delegate { .. }
            | Obligation::AdvanceDelegation { .. }
            | Obligation::Retire { .. }
            | Obligation::Blocked { .. } => {}
        }
    }

    Ok(RoundObligations {
        obligations,
        choice_proposals,
        open_proposals,
        unrostered_intents,
        stale_vote_keys,
    })
}

/// The share indexes a confirmed vote's recovery bundle expects that have
/// no helper-share row yet.
fn missing_share_indexes(
    snapshot: &RoundSnapshot,
    bundle_index: u32,
    proposal_id: u32,
) -> Result<Vec<u32>, VotingError> {
    let round_id = snapshot.round_id.as_str();
    let Some(recovery) = snapshot
        .votes
        .get(&(bundle_index, proposal_id))
        .and_then(|vote| vote.recovery.as_ref())
    else {
        return Err(VotingError::InvalidInput {
            message: format!(
                "confirmed vote for round {round_id} bundle {bundle_index} proposal {proposal_id} is missing recovery material for helper-share submission"
            ),
        });
    };
    let expected_share_indexes = crate::share::recover_payloads(recovery)?
        .iter()
        .map(|payload| payload.enc_share.share_index)
        .collect::<BTreeSet<_>>();
    if expected_share_indexes.is_empty() {
        return Err(VotingError::InvalidInput {
            message: format!(
                "confirmed vote for round {round_id} bundle {bundle_index} proposal {proposal_id} has no recoverable helper shares"
            ),
        });
    }
    let recorded_share_indexes = snapshot
        .share_phases
        .iter()
        .filter(|(b, p, _, _)| *b == bundle_index && *p == proposal_id)
        .map(|(_, _, share_index, _)| *share_index)
        .collect::<BTreeSet<_>>();
    Ok(expected_share_indexes
        .difference(&recorded_share_indexes)
        .copied()
        .collect())
}
