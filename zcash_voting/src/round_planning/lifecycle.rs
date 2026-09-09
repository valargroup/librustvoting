//! Where a vote stands relative to the chain lifecycle, the roster, and the
//! ballot. These three relations are the only inputs the classifier's rule
//! table takes, so every "is this work still the wallet's to plan" decision
//! is spelled here once.

use std::collections::{BTreeMap, BTreeSet};

use crate::phases::{DelegationPhase, VotePhase};
use crate::session::Decision;

/// Coarse position of a vote in the chain lifecycle, an exhaustive
/// coarsening of [`VotePhase`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LifecyclePosition {
    /// No row, or a row without recovery material: nothing durable the chain
    /// could have seen.
    Uncast,
    /// `Committed`: proof, recovery bundle and signature are durable and no
    /// POST is reserved. The wallet owns it; it may be retired or recast.
    Undispatched,
    /// `Submitted` or `SubmissionManaged`: a POST was reserved or dispatched.
    /// The chain lifecycle owns it and it is driven to resolution whatever
    /// the ballot or roster now say.
    OnWire,
    /// `SubmittedWithoutHash` or `SubmissionRejected`: the lifecycle ended
    /// without a confirmation. No step is planned.
    Terminal,
    /// `Confirmed`: the vote has a tree position and its shares are owed.
    Confirmed,
}

impl LifecyclePosition {
    /// The position of a vote in `phase`, or of a vote with no row.
    ///
    /// Exhaustive on purpose: a new phase must be placed here rather than
    /// defaulting into a position the planner would then act on.
    pub(crate) fn of(phase: Option<VotePhase>) -> Self {
        match phase {
            None | Some(VotePhase::Prepared) => Self::Uncast,
            Some(VotePhase::Committed) => Self::Undispatched,
            Some(VotePhase::Submitted | VotePhase::SubmissionManaged) => Self::OnWire,
            Some(VotePhase::SubmittedWithoutHash | VotePhase::SubmissionRejected) => Self::Terminal,
            Some(VotePhase::Confirmed) => Self::Confirmed,
        }
    }

    /// Whether the chain lifecycle owns or has finished the vote: it may be
    /// on chain, so the ballot intent behind it is locked and its work
    /// survives a roster change.
    pub(crate) fn is_lifecycle_owned(self) -> bool {
        match self {
            Self::Uncast | Self::Undispatched => false,
            Self::OnWire | Self::Terminal | Self::Confirmed => true,
        }
    }
}

/// Whether a vote in `phase` is owned or finished by the chain lifecycle;
/// see [`LifecyclePosition::is_lifecycle_owned`].
pub(crate) fn vote_phase_is_lifecycle_owned(phase: VotePhase) -> bool {
    LifecyclePosition::of(Some(phase)).is_lifecycle_owned()
}

/// Whether any vote for `proposal_id` is lifecycle-owned, read on `conn` so
/// a write path can decide inside its own transaction.
pub(crate) fn intent_is_lifecycle_owned(
    conn: &rusqlite::Connection,
    wallet_id: &str,
    round_id: &str,
    proposal_id: u32,
) -> Result<bool, crate::types::VotingError> {
    Ok(
        crate::phases::vote_phases_for_proposal(conn, wallet_id, round_id, proposal_id)?
            .into_iter()
            .any(|(_, phase)| vote_phase_is_lifecycle_owned(phase)),
    )
}

/// Whether a live vote in `phase` reserves its bundle against a fresh cast:
/// anything the chain may still act on, plus a hashless dispatch that may
/// have landed. A rejected vote spent nothing and reserves nothing.
pub(crate) fn vote_phase_holds_bundle(phase: VotePhase) -> bool {
    match phase {
        VotePhase::Committed
        | VotePhase::Submitted
        | VotePhase::SubmissionManaged
        | VotePhase::SubmittedWithoutHash => true,
        VotePhase::Prepared | VotePhase::SubmissionRejected | VotePhase::Confirmed => false,
    }
}

/// Whether a delegation in `phase` reserves its bundle against a fresh cast:
/// it is managed by the lifecycle or ended without a confirmation.
pub(crate) fn delegation_phase_holds_bundle(phase: DelegationPhase) -> bool {
    match phase {
        DelegationPhase::SubmissionManaged
        | DelegationPhase::SubmittedWithoutHash
        | DelegationPhase::SubmissionRejected => true,
        DelegationPhase::Prepared
        | DelegationPhase::PcztBuilt
        | DelegationPhase::Proved
        | DelegationPhase::Submitted
        | DelegationPhase::Confirmed => false,
    }
}

/// Consecutive chain rejections of one bundle's combined batch, counted
/// against a single delegation generation, after which the wallet stops
/// planning that cast on its own.
///
/// Every blocked-through attempt re-proves every member of the batch and
/// re-POSTs the identical delegation. The POST retry budget is already spent
/// inside one invocation, so retrying across runs only helps for a cause that
/// is transient *between* runs. One extra run covers that; a second identical
/// refusal is evidence the envelope itself is at fault.
pub(crate) const MAX_CONSECUTIVE_COMBINED_REJECTIONS: u32 = 2;

/// Whether `streak` consecutive rejections stop the wallet planning a further
/// combined cast for the bundle. Advisory, not a terminal delegation phase: a
/// host can clear the streak and the bundle plans again.
pub(crate) fn combined_rejections_block_bundle(streak: u32) -> bool {
    streak >= MAX_CONSECUTIVE_COMBINED_REJECTIONS
}

/// Whether a delegation ended without a confirmation and will never be
/// planned again; `Confirmed` is a success, not a terminal failure.
///
/// Exhaustive on purpose: a new phase must be classified here rather than
/// defaulting into "retry", which for a hashless dispatch would resubmit a
/// transaction that may already be on the chain.
pub(crate) fn is_terminal_delegation_phase(phase: DelegationPhase) -> bool {
    match phase {
        DelegationPhase::SubmittedWithoutHash | DelegationPhase::SubmissionRejected => true,
        DelegationPhase::Prepared
        | DelegationPhase::PcztBuilt
        | DelegationPhase::Proved
        | DelegationPhase::Submitted
        | DelegationPhase::SubmissionManaged
        | DelegationPhase::Confirmed => false,
    }
}

/// A vote unit's relation to the authenticated roster. It is per unit, not
/// per member, because an atomic batch is indivisible.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RosterRelation {
    /// Every member's proposal is in the roster.
    Rostered,
    /// Some member's proposal is not.
    LeftRoster,
}

/// The relation of `proposal_ids` to `roster`.
pub(crate) fn roster_relation<'a>(
    proposal_ids: impl IntoIterator<Item = &'a u32>,
    roster: &BTreeSet<u32>,
) -> RosterRelation {
    if proposal_ids
        .into_iter()
        .all(|proposal_id| roster.contains(proposal_id))
    {
        RosterRelation::Rostered
    } else {
        RosterRelation::LeftRoster
    }
}

/// One stored vote's relation to the durable ballot intent for its
/// proposal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BallotRelation {
    /// The intent is `Choice` of the stored choice.
    Agrees,
    /// No intent is recorded.
    Unrecorded,
    /// The intent is `Skipped` or a different `Choice`: the stored vote no
    /// longer says what the voter wants.
    Conflicts,
}

/// The relation of a vote storing `stored_choice` for `proposal_id` to the
/// recorded `intents`.
pub(crate) fn ballot_relation(
    intents: &BTreeMap<u32, Decision>,
    proposal_id: u32,
    stored_choice: u32,
) -> BallotRelation {
    match intents.get(&proposal_id) {
        None => BallotRelation::Unrecorded,
        Some(Decision::Choice(choice)) if *choice == stored_choice => BallotRelation::Agrees,
        Some(Decision::Choice(_) | Decision::Skipped) => BallotRelation::Conflicts,
    }
}
