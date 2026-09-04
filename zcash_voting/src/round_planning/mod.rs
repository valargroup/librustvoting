//! Round planning: one consistent snapshot of a round's durable state, the
//! obligations classified from it, and the plan projected for the host.
//!
//! ```text
//! load_round_snapshot   one deferred read transaction, plain data out
//!   > group_vote_units  singleton | atomic batch, batch invariants checked
//!   > classify          pure: snapshot x units x roster -> obligations
//!   > project           obligations -> RoundPlan / NextStep, ordered
//! ```
//!
//! The rules are specified in `docs/round_orchestration_invariants.md`. The
//! classifier is the only place that decides whether a unit of work is still
//! the wallet's to plan; the projection only presents what it found.

mod classify;
mod lifecycle;
mod projection;
mod snapshot;
mod vote_units;

pub(crate) use classify::RoundObligations;
pub(crate) use lifecycle::vote_phase_is_lifecycle_owned;
pub(crate) use projection::blocking_prerequisite;
pub(crate) use snapshot::{load_round_snapshot, RoundSnapshot};
pub(crate) use vote_units::group_vote_units;
// Executor dispatch moves onto obligations in the next change; until then
// only tests read them.
#[cfg(test)]
pub(crate) use classify::{BlockedReason, Obligation};
#[cfg(test)]
pub(crate) use lifecycle::LifecyclePosition;
#[cfg(test)]
pub(crate) use projection::{resolve_step, summarize_plan_work};
#[cfg(test)]
pub(crate) use snapshot::{BundleSnapshot, VoteSnapshot};
#[cfg(test)]
pub(crate) use vote_units::VoteUnitId;

use crate::session::{classify_ballot_intents, RoundPlan};
use crate::storage::VotingDb;
use crate::types::VotingError;

/// The plan for `round_id` against the authenticated `proposal_ids`, from
/// one snapshot of the wallet's durable state.
pub(crate) fn plan_round(
    db: &VotingDb,
    round_id: &str,
    proposal_ids: &[u32],
) -> Result<RoundPlan, VotingError> {
    let wallet_id = db.wallet_id();
    let snapshot = db.read_transaction("plan round", |tx| {
        load_round_snapshot(tx, &wallet_id, round_id)
    })?;
    plan_from_snapshot(&snapshot, proposal_ids)
}

/// The plan derived from one snapshot. Pure over `snapshot` and
/// `proposal_ids`.
pub(crate) fn plan_from_snapshot(
    snapshot: &RoundSnapshot,
    proposal_ids: &[u32],
) -> Result<RoundPlan, VotingError> {
    let obligations = classify_round(snapshot, proposal_ids)?;
    projection::project(snapshot, proposal_ids, &obligations)
}

/// The obligations of one snapshot: ballot classification, unit grouping,
/// then the rule table.
pub(crate) fn classify_round(
    snapshot: &RoundSnapshot,
    proposal_ids: &[u32],
) -> Result<RoundObligations, VotingError> {
    let ballot = classify_ballot_intents(proposal_ids, &snapshot.intents)?;
    if !ballot.choice_proposals.is_empty() && snapshot.delegations.is_empty() {
        return Err(VotingError::InvalidInput {
            message: format!(
                "round {} has ballot choice intent but no eligible bundle rows",
                snapshot.round_id
            ),
        });
    }
    let units = group_vote_units(snapshot, &snapshot.intents)?;
    classify::classify(snapshot, &units, ballot)
}

#[cfg(test)]
mod tests;
