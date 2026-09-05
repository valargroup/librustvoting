//! Exact ballot progress for one run, derived from obligations.

use std::collections::BTreeSet;

use crate::round_planning::{Obligation, RoundObligations};

/// How much of what this run set out to do is done.
///
/// Progress is **run-relative**: the total is the vote work the run's first
/// plan owed, and a proposal is complete once no `Cast` and no
/// `ReconcileChain` obligation covers it any more. A round resumed with two
/// questions left reports two, not the whole ballot, which is what a host
/// showing "question N of M" for this run wants.
///
/// The counts are exact for atomic batches. Obligation membership names every
/// ordered member, where a host counting `NextStep`s sees one
/// `AdvanceVoteBatch` carrying only its first member's id — a six-proposal
/// batch that reads as one question.
///
/// Helper-share work is deliberately outside this tally. A confirmed vote's
/// proposal is complete whether or not its shares have landed; hosts show
/// share delivery separately.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RoundWorkTally {
    pub completed_proposals: u32,
    pub total_proposals: u32,
    /// Obligations the round still owes. `Blocked` is excluded: it is never
    /// dispatched, and the plan reports it through `open_proposals` and
    /// `unrostered_intents` instead.
    pub remaining_obligations: u32,
}

/// The vote work the run's first plan owed, held so later plans can be
/// measured against it.
#[derive(Clone, Debug, Default)]
pub(super) struct BallotBaseline {
    proposals: BTreeSet<u32>,
}

impl BallotBaseline {
    /// Captures the proposals the run starts out owing a vote for.
    ///
    /// A withheld cast contributes nothing: a `Blocked` obligation names no
    /// proposals, and while one stands the host is resolving the ballot rather
    /// than watching a progress bar.
    pub(super) fn capture(obligations: &RoundObligations) -> Self {
        Self {
            proposals: covered_proposals(obligations),
        }
    }

    /// Measures `obligations` against the baseline.
    pub(super) fn tally(&self, obligations: &RoundObligations) -> RoundWorkTally {
        let still_covered = covered_proposals(obligations);
        let completed = self
            .proposals
            .iter()
            .filter(|proposal_id| !still_covered.contains(proposal_id))
            .count();
        RoundWorkTally {
            completed_proposals: completed as u32,
            total_proposals: self.proposals.len() as u32,
            remaining_obligations: obligations
                .obligations
                .iter()
                .filter(|obligation| !matches!(obligation, Obligation::Blocked { .. }))
                .count() as u32,
        }
    }
}

/// Every proposal a vote obligation still owes work for.
///
/// Only the two vote obligations count. `Retire` clears a unit the cast pass
/// replaces, so its members are owed through the `Cast` that follows and
/// counting both would double count. `Deliver` and `Confirm` are share work on
/// a proposal whose vote has already landed.
fn covered_proposals(obligations: &RoundObligations) -> BTreeSet<u32> {
    let mut covered = BTreeSet::new();
    for obligation in &obligations.obligations {
        match obligation {
            Obligation::Cast { drafts, .. } => {
                covered.extend(drafts.iter().map(|draft| draft.proposal_id));
            }
            Obligation::ReconcileChain {
                ordered_proposal_ids,
                ..
            } => covered.extend(ordered_proposal_ids.iter().copied()),
            Obligation::Blocked { .. }
            | Obligation::Delegate { .. }
            | Obligation::AdvanceDelegation { .. }
            | Obligation::Retire { .. }
            | Obligation::Deliver { .. }
            | Obligation::Confirm { .. } => {}
        }
    }
    covered
}
