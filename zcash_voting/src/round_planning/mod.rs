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

#[cfg(test)]
pub(crate) use classify::BlockedReason;
pub(crate) use classify::{CastDraft, Obligation, RoundObligations};
#[cfg(test)]
pub(crate) use lifecycle::LifecyclePosition;
pub(crate) use lifecycle::{intent_is_lifecycle_owned, vote_phase_is_lifecycle_owned};
#[cfg(test)]
pub(crate) use projection::summarize_plan_work;
pub(crate) use projection::{blocking_prerequisite, resolve_step};
pub(crate) use snapshot::{load_round_snapshot, RoundSnapshot};
#[cfg(test)]
pub(crate) use snapshot::{BundleSnapshot, VoteSnapshot};
pub(crate) use vote_units::{group_vote_units, VoteUnitId};

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
    Ok(plan_round_classified(db, round_id, proposal_ids)?.plan)
}

/// A plan together with the obligations it was projected from, so an
/// executor can resolve a host-selected step to the work it executes.
pub(crate) struct ClassifiedPlan {
    pub(crate) plan: RoundPlan,
    pub(crate) obligations: RoundObligations,
}

/// [`plan_round`], keeping the obligations beside the plan.
pub(crate) fn plan_round_classified(
    db: &VotingDb,
    round_id: &str,
    proposal_ids: &[u32],
) -> Result<ClassifiedPlan, VotingError> {
    let wallet_id = db.wallet_id();
    let snapshot = db.read_transaction("plan round", |tx| {
        load_round_snapshot(tx, &wallet_id, round_id)
    })?;
    let obligations = classify_round(&snapshot, proposal_ids)?;
    let plan = projection::project(&snapshot, proposal_ids, &obligations)?;
    Ok(ClassifiedPlan { plan, obligations })
}

/// The plan derived from one snapshot. Pure over `snapshot` and
/// `proposal_ids`.
#[cfg(test)]
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
