//! Exact selected-choice submission progress, derived from obligations.

use std::collections::BTreeSet;

use crate::round_planning::{Obligation, RoundObligations};

/// How much of the vote work the driver is measuring against is done.
///
/// A proposal is complete once no `Cast` and no `ReconcileChain` obligation
/// covers it any more and none of the round's choices is still waiting on a
/// cast the plan could not draw up. What the total counts is the host's
/// choice, made by
/// [`ProgressBaseline`](super::ProgressBaseline):
///
/// - `Run` (the default) is **run-relative**: the total is the vote work the
///   run's first plan owed, so a round resumed with two questions left reports
///   two.
/// - `SelectedChoices` counts every durable choice whose vote belongs to the
///   current roster or chain lifecycle. Skips and clearable stale choices are
///   excluded because they owe no vote submission.
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
    /// Obligations the round still owes and this layer can execute.
    ///
    /// `Blocked` and `Retire` are excluded: neither is ever dispatched on its
    /// own. `Blocked` is reported through the plan's `open_proposals` and
    /// `unrostered_intents`, and a `Retire` is carried out by the `Cast` that
    /// replaces the unit — counting it separately would both double count that
    /// work and, for a round whose retire has no surviving cast, report work
    /// owed beside a `NoWorkLeft` quiescence.
    pub remaining_obligations: u32,
}

/// The vote work the run's first plan owed, held so later plans can be
/// measured against it.
#[derive(Clone, Debug, Default)]
pub(super) struct VoteProgressBaseline {
    proposals: BTreeSet<u32>,
}

impl VoteProgressBaseline {
    /// Captures the proposals the run starts out owing a vote for.
    ///
    /// A withheld cast contributes nothing: a `Blocked` obligation names no
    /// proposals, and while one stands the host is resolving the ballot rather
    /// than watching a progress bar.
    pub(super) fn for_run(obligations: &RoundObligations) -> Self {
        Self {
            proposals: covered_proposals(obligations),
        }
    }

    /// Captures every durable selected choice that still belongs to this round.
    ///
    /// The measure is the same; only the total differs. A run baseline asks
    /// "how much of what this run picked up is done", which renumbers when a
    /// resume picks up less than all selected choices. This asks "how many of
    /// the selected choices have finished vote submission", preserving that
    /// denominator across a restart when the selections and roster are
    /// unchanged.
    ///
    /// Skipped proposals are excluded, because `choice_proposals` holds only
    /// roster proposals with a durable `Choice`. A skip owes no vote
    /// submission.
    ///
    /// `lifecycle_owned_choices` is added back, because a choice whose vote the
    /// chain lifecycle owns is one the host cannot clear and whose work
    /// deliberately outlives its roster seat. Dropping it when the roster drops
    /// it would move the very total this baseline exists to hold still, and
    /// would hide a selected vote still on the wire. A clearable unrostered
    /// intent is not added back: the host resolves it, and the recast that
    /// follows is planned fresh. Nor is a vote with no durable choice at all,
    /// which the wallet drives to resolution but the voter did not select.
    ///
    /// Unlike a run baseline, this total holds selected choices no obligation names:
    /// a ballot the host recorded before bundle setup, a cast withheld while
    /// the ballot is open, and a member of an undispatched batch the ballot has
    /// not finished deciding own nothing in the plan. `tally` reads
    /// `withheld_casts` so those count as owed rather than as done.
    pub(super) fn for_selected_choices(obligations: &RoundObligations) -> Self {
        Self {
            proposals: obligations
                .choice_proposals
                .iter()
                .chain(&obligations.lifecycle_owned_choices)
                .copied()
                .collect(),
        }
    }

    /// Measures `obligations` against the baseline.
    ///
    /// A proposal is complete only when it is neither covered by a vote
    /// obligation nor among the casts this plan could not draw up. Absence
    /// alone does not mean done: a round awaiting bundle setup, and a bundle
    /// whose cast is withheld while the ballot is open, plan no `Cast` for a
    /// choice that no vote has landed for.
    pub(super) fn tally(&self, obligations: &RoundObligations) -> RoundWorkTally {
        let still_covered = covered_proposals(obligations);
        let completed = self
            .proposals
            .iter()
            .filter(|proposal_id| {
                !still_covered.contains(proposal_id)
                    && !obligations.withheld_casts.contains(proposal_id)
            })
            .count();
        RoundWorkTally {
            completed_proposals: completed as u32,
            total_proposals: self.proposals.len() as u32,
            remaining_obligations: obligations
                .obligations
                .iter()
                .filter(|obligation| {
                    !matches!(
                        obligation,
                        Obligation::Blocked { .. } | Obligation::Retire { .. }
                    )
                })
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
