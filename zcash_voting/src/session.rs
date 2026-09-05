//! Durable ballot intent + resumable voting-session planner.
//!
//! `resume_plan` reads a round once, as one [`RoundSnapshot`] taken in a
//! single read transaction, and derives the ordered remaining work for the
//! round from that snapshot alone. The wallet executes each step with its
//! own network/proof/sign plumbing.

use rusqlite::named_params;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

use crate::chain_submission::ChainSubmissionDiagnostic;
use crate::phases::DelegationPhase;
use crate::share_policy::ImmediateShareKey;
use crate::storage::{queries, VotingDb};
use crate::types::{
    validate_proposal_id, validate_vote_decision, validate_vote_options, VotingError,
};
use crate::vote::{validate_draft_vote, DraftVote};

/// The voter's terminal decision for one proposal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Choice(u32),
    Skipped,
}

/// Durable ballot-intent classification shared by resume and helper planning.
pub(crate) struct BallotIntentClassification {
    pub(crate) roster: BTreeSet<u32>,
    pub(crate) choice_proposals: Vec<u32>,
    pub(crate) open_proposals: Vec<u32>,
    /// Durable intents for proposals outside the authenticated roster.
    pub(crate) unrostered_intents: Vec<u32>,
}

pub(crate) fn classify_ballot_intents(
    proposal_ids: &[u32],
    intents: &BTreeMap<u32, Decision>,
) -> Result<BallotIntentClassification, VotingError> {
    let mut roster = BTreeSet::new();
    for &proposal_id in proposal_ids {
        validate_proposal_id(proposal_id)?;
        if !roster.insert(proposal_id) {
            return Err(VotingError::InvalidInput {
                message: format!("proposal roster contains duplicate id {proposal_id}"),
            });
        }
    }

    let mut choice_proposals = Vec::new();
    let mut open_proposals = Vec::new();
    for &proposal_id in &roster {
        match intents.get(&proposal_id) {
            Some(Decision::Choice(_)) => choice_proposals.push(proposal_id),
            Some(Decision::Skipped) => {}
            None => open_proposals.push(proposal_id),
        }
    }

    let unrostered_intents = intents
        .keys()
        .copied()
        .filter(|proposal_id| !roster.contains(proposal_id))
        .collect();

    Ok(BallotIntentClassification {
        roster,
        choice_proposals,
        open_proposals,
        unrostered_intents,
    })
}

pub(crate) use crate::round_planning::vote_phase_is_lifecycle_owned;

impl VotingDb {
    /// Record (insert or replace) the voter's decision for one proposal.
    ///
    /// `num_options` must be the proposal's declared option count. Choice
    /// decisions are validated against it before any durable intent is written.
    /// Written on each selection, before any per-proposal vote artifact exists.
    pub fn set_ballot_intent(
        &self,
        round_id: &str,
        proposal_id: u32,
        decision: Decision,
        num_options: u32,
    ) -> Result<(), VotingError> {
        validate_proposal_id(proposal_id)?;
        validate_ballot_intent_decision(decision, num_options)?;

        self.write_ballot_intent(round_id, proposal_id, decision)
    }

    /// Retires committed but undispatched votes on `bundle_index` for
    /// proposals outside `roster`, returning the retired proposal ids.
    ///
    /// Such a vote can never be submitted once its proposal left the
    /// authenticated roster, but its commitment keeps the bundle's VAN
    /// reserved and blocks every later cast on the bundle; its intent cannot
    /// be cleared while a sibling bundle's vote for the same proposal is on
    /// the chain lifecycle. The executor retires them before casting. Votes
    /// the chain lifecycle owns or has finished are left untouched.
    pub fn retire_undispatched_votes_outside_roster(
        &self,
        round_id: &str,
        bundle_index: u32,
        roster: &[u32],
    ) -> Result<Vec<u32>, VotingError> {
        let wallet_id = self.wallet_id();
        self.write_transaction("retire undispatched votes transaction failed", |tx| {
            crate::vote::retire_undispatched_votes_outside_roster_with_conn(
                tx,
                &wallet_id,
                round_id,
                bundle_index,
                roster,
            )
        })
    }

    /// Removes the durable decision for one proposal.
    ///
    /// A decision that survives a roster change refers to a proposal the
    /// authenticated configuration no longer lists; the planner reports it in
    /// `RoundPlan::unrostered_intents` and withholds `CastVote` until it is
    /// cleared. Clearing an intent that does not exist is not an error.
    ///
    /// The canonical vote phase, read inside the same write transaction as
    /// the deletion, decides whether clearing is allowed. A vote
    /// for the proposal that the chain lifecycle owns or has finished
    /// (submitted, managed, hashless, rejected, or confirmed) may already be
    /// on chain, so its intent cannot be cleared. A vote that is signed but
    /// not dispatched has its unsubmitted recovery invalidated, exactly as a
    /// changed intent does.
    ///
    /// # Errors
    ///
    /// Returns [`VotingError::InvalidInput`] for an out-of-range proposal id
    /// or a proposal whose vote is on or past the chain lifecycle.
    pub fn clear_ballot_intent(&self, round_id: &str, proposal_id: u32) -> Result<(), VotingError> {
        validate_proposal_id(proposal_id)?;
        let wallet_id = self.wallet_id();
        self.write_transaction("clear_ballot_intent transaction failed", |tx| {
            // Evaluated under the write transaction, so a submission that
            // becomes terminal between a read and the delete cannot slip past.
            if let Some((bundle_index, phase)) =
                crate::phases::vote_phases_for_proposal(tx, &wallet_id, round_id, proposal_id)?
                    .into_iter()
                    .find(|(_, phase)| vote_phase_is_lifecycle_owned(*phase))
            {
                return Err(VotingError::InvalidInput {
                    message: format!(
                        "round {round_id} bundle {bundle_index} proposal {proposal_id} vote is {} on the chain lifecycle; its intent cannot be cleared",
                        phase.as_str()
                    ),
                });
            }
            crate::vote::invalidate_unsubmitted_vote_recoveries_for_intent(
                tx,
                &wallet_id,
                round_id,
                proposal_id,
                None,
            )?;
            tx.execute(
                "DELETE FROM ballot_intent
                 WHERE round_id = :round_id AND wallet_id = :wallet_id AND proposal_id = :proposal_id",
                named_params! {
                    ":round_id": round_id,
                    ":wallet_id": wallet_id,
                    ":proposal_id": proposal_id as i64,
                },
            )
            .map_err(|e| VotingError::from_sqlite("clear ballot intent", &e))?;
            Ok(())
        })
    }

    /// Record the voter's decisions for several proposals atomically.
    ///
    /// Each entry is `(proposal_id, decision, num_options)`, where
    /// `num_options` is that proposal's declared option count. Every entry is
    /// validated, and the batch is rejected if it names one proposal twice,
    /// before any durable intent is written; the writes then share one
    /// transaction, so a conflict raised by a later entry — a submitted vote
    /// that contradicts it, for instance — leaves none of the batch applied.
    ///
    /// # Errors
    ///
    /// Returns [`VotingError::InvalidInput`] for an out-of-range proposal id,
    /// a decision that does not fit its option count, or a repeated proposal
    /// id.
    /// A round already stored under `round_id` must be stored for
    /// `expected_network`; the check runs inside the write transaction, and
    /// a mismatch is [`VotingError::InvalidInput`] with no intent written.
    pub fn set_ballot_intents(
        &self,
        round_id: &str,
        expected_network: crate::types::Network,
        intents: &[(u32, Decision, u32)],
    ) -> Result<(), VotingError> {
        let mut seen = BTreeSet::new();
        for &(proposal_id, decision, num_options) in intents {
            validate_proposal_id(proposal_id)?;
            validate_ballot_intent_decision(decision, num_options)?;
            if !seen.insert(proposal_id) {
                return Err(VotingError::InvalidInput {
                    message: format!("ballot intent batch decides proposal {proposal_id} twice"),
                });
            }
        }

        let now = now_secs();
        let wallet_id = self.wallet_id();
        self.write_transaction("set_ballot_intents transaction failed", |tx| {
            // Checked under the write lock so a round created for another
            // network between the caller's check and this write cannot take
            // choices resolved against a different network's roster.
            if crate::storage::queries::has_round(tx, round_id, &wallet_id)? {
                let stored = crate::storage::queries::load_round_network(tx, round_id, &wallet_id)?;
                if stored != expected_network {
                    return Err(VotingError::InvalidInput {
                        message: format!(
                            "round {round_id} is stored for network {stored:?} but the ballot intents are for {expected_network:?}"
                        ),
                    });
                }
            }
            for &(proposal_id, decision, _) in intents {
                write_ballot_intent_in_tx(tx, &wallet_id, round_id, proposal_id, decision, now)?;
            }
            Ok(())
        })
    }

    /// Record a choice intent from a fully validated draft vote.
    pub fn set_ballot_intent_for_draft_vote(
        &self,
        round_id: &str,
        draft: &DraftVote,
    ) -> Result<(), VotingError> {
        validate_draft_vote(draft)?;
        self.write_ballot_intent(round_id, draft.proposal_id, Decision::Choice(draft.choice))
    }

    fn write_ballot_intent(
        &self,
        round_id: &str,
        proposal_id: u32,
        decision: Decision,
    ) -> Result<(), VotingError> {
        let now = now_secs();
        let wallet_id = self.wallet_id();
        self.write_transaction("set_ballot_intent transaction failed", |tx| {
            write_ballot_intent_in_tx(tx, &wallet_id, round_id, proposal_id, decision, now)
        })
    }
}

/// Writes one durable ballot intent inside a caller-owned transaction.
///
/// Resolves the conflicting-vote and stale-share side effects of the decision
/// alongside the intent row, so a batch of intents can commit atomically.
fn write_ballot_intent_in_tx(
    tx: &rusqlite::Transaction<'_>,
    wallet_id: &str,
    round_id: &str,
    proposal_id: u32,
    decision: Decision,
    now: i64,
) -> Result<(), VotingError> {
    let (skipped, choice): (i64, Option<i64>) = match decision {
        Decision::Choice(c) => (0, Some(c as i64)),
        Decision::Skipped => (1, None),
    };
    let skipped_bool = skipped != 0;
    let choice_u32 = choice.map(|c| c as u32);
    queries::ensure_no_submitted_vote_conflict_for_intent(
        tx,
        round_id,
        wallet_id,
        proposal_id,
        skipped_bool,
        choice_u32,
    )?;
    crate::vote::invalidate_unsubmitted_vote_recoveries_for_intent(
        tx,
        wallet_id,
        round_id,
        proposal_id,
        choice_u32,
    )?;
    tx.execute(
        "INSERT INTO ballot_intent
            (round_id, wallet_id, proposal_id, skipped, choice, created_at, updated_at)
         VALUES (:round_id, :wallet_id, :proposal_id, :skipped, :choice, :now, :now)
         ON CONFLICT(round_id, wallet_id, proposal_id)
         DO UPDATE SET skipped = :skipped, choice = :choice, updated_at = :now",
        named_params! {
            ":round_id": round_id,
            ":wallet_id": wallet_id,
            ":proposal_id": proposal_id as i64,
            ":skipped": skipped,
            ":choice": choice,
            ":now": now,
        },
    )
    .map_err(|e| VotingError::from_sqlite("set_ballot_intent", &e))?;
    if skipped_bool {
        tx.execute(
            "DELETE FROM share_delegations
             WHERE round_id = :round_id
               AND wallet_id = :wallet_id
               AND proposal_id = :proposal_id",
            named_params! {
                ":round_id": round_id,
                ":wallet_id": wallet_id,
                ":proposal_id": proposal_id as i64,
            },
        )
    } else if let Some(choice) = choice_u32 {
        tx.execute(
            "DELETE FROM share_delegations
             WHERE round_id = :round_id
               AND wallet_id = :wallet_id
               AND proposal_id = :proposal_id
               AND NOT EXISTS (
                   SELECT 1 FROM votes
                   WHERE votes.round_id = share_delegations.round_id
                     AND votes.wallet_id = share_delegations.wallet_id
                     AND votes.bundle_index = share_delegations.bundle_index
                     AND votes.proposal_id = share_delegations.proposal_id
                     AND votes.choice = :choice
               )",
            named_params! {
                ":round_id": round_id,
                ":wallet_id": wallet_id,
                ":proposal_id": proposal_id as i64,
                ":choice": choice as i64,
            },
        )
    } else {
        Ok(0)
    }
    .map_err(|e| VotingError::from_sqlite("clear stale share delegations", &e))?;
    Ok(())
}

impl VotingDb {
    /// Load the voter's decisions for a round, sorted by proposal id.
    pub fn ballot_intents(&self, round_id: &str) -> Result<Vec<(u32, Decision)>, VotingError> {
        let conn = self.conn();
        let wallet_id = self.wallet_id();
        load_ballot_intents(&conn, round_id, &wallet_id)
    }
}

/// [`VotingDb::ballot_intents`] on `conn`, so a caller can read it inside one
/// transaction with the rest of a round's state.
pub(crate) fn load_ballot_intents(
    conn: &rusqlite::Connection,
    round_id: &str,
    wallet_id: &str,
) -> Result<Vec<(u32, Decision)>, VotingError> {
    {
        let mut stmt = conn
            .prepare(
                "SELECT proposal_id, skipped, choice FROM ballot_intent
                 WHERE round_id = :round_id AND wallet_id = :wallet_id
                 ORDER BY proposal_id",
            )
            .map_err(|e| VotingError::from_sqlite("prepare ballot_intents", &e))?;
        let rows = stmt
            .query_map(
                named_params! { ":round_id": round_id, ":wallet_id": wallet_id },
                |row| {
                    let pid = row.get::<_, i64>(0)? as u32;
                    let skipped: i64 = row.get(1)?;
                    let choice: Option<i64> = row.get(2)?;
                    let decision = if skipped != 0 {
                        Decision::Skipped
                    } else {
                        Decision::Choice(choice.ok_or_else(|| {
                            rusqlite::Error::InvalidQuery
                        })? as u32)
                    };
                    Ok((pid, decision))
                },
            )
            .map_err(|e| VotingError::from_sqlite("query ballot_intents", &e))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| VotingError::from_sqlite("collect ballot_intents", &e))
    }
}

impl VotingDb {}

fn validate_ballot_intent_decision(
    decision: Decision,
    num_options: u32,
) -> Result<(), VotingError> {
    match decision {
        Decision::Choice(choice) => validate_vote_decision(choice, num_options),
        Decision::Skipped => validate_vote_options(num_options),
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// One unit of remaining work for a round. Ordered deterministically within a
/// `RoundPlan`, so a restart yields the same sequence.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NextStep {
    /// Run the delegation flow: this bundle still needs a signed delegation
    /// before anything can be dispatched.
    Delegate { bundle_index: u32 },
    /// Advance one delegation that is already durably in flight.
    ///
    /// Call `ChainSubmissionClient::advance_delegation_with_recovery` with
    /// `ChainRecoveryMode::ExactTree`. Emitted while the authoritative
    /// generation is `Submitting`, `Tracking`, or `Recovering`, and requires
    /// the wallet to restore signing material for the locked generation.
    AdvanceDelegation { bundle_index: u32 },
    /// Poll one already-broadcast delegation imported from a capability.
    ///
    /// Call `ChainSubmissionClient::advance_imported_delegation`. The lifecycle
    /// adopts the package hash on the first pass and never asks the voter for a
    /// signer or dispatches the transaction again.
    AdvanceImportedDelegation { bundle_index: u32 },
    /// Cast a vote using the recorded ballot intent choice.
    ///
    /// Only planned once every proposal in the roster has a terminal decision,
    /// a choice or a skip. Casting persists the vote and then derives the
    /// round's immediate helper share from the complete set of choices, so an
    /// undecided proposal would make the step fail after the proof was
    /// generated and the vote written. While the ballot is still open the plan
    /// reports the remaining proposals through `RoundPlan::open_proposals` and
    /// plans the bundle's `Delegate` prerequisite instead.
    ///
    /// A changed choice is recoverable only until a cast-vote transaction has
    /// been submitted. Once submitted, the proposal authority for that bundle
    /// has moved on-chain and `resume_plan` reports the conflict as invalid
    /// state instead of returning a non-actionable recast step.
    CastVote {
        bundle_index: u32,
        proposal_id: u32,
        choice: u32,
    },
    /// Advance one singleton vote's chain submission by one bounded pass.
    ///
    /// Call `ChainSubmissionClient::advance_vote_with_recovery` with
    /// `ChainRecoveryMode::ExactTree`. The lifecycle owns transaction
    /// construction, dispatch, polling, recovery, and confirmation, so
    /// reserving, submitting, and reconciling are one host call rather than
    /// separate submit and poll steps. Re-invoke while the result is pending.
    ///
    /// Recover the `CommittedVote`, preflight the complete helper fleet, and
    /// call `CommittedVote::prepare_share_delivery` to create or reload its
    /// complete persisted plan. After confirmation, convert it with
    /// `CommittedVote::confirmed` and submit through
    /// `ConfirmedVote::submit_prepared_shares` with the current fleet. A
    /// reloaded plan retains its original target while delivery excludes
    /// helpers removed from the current fleet. The typed method rebuilds
    /// payloads with the confirmed commitment-tree position and journals each
    /// POST before dispatch.
    AdvanceVote { bundle_index: u32, proposal_id: u32 },
    /// Advance one atomic vote batch's chain submission by one bounded pass.
    ///
    /// `proposal_id` identifies the batch's first ordered action and is only a
    /// recovery anchor. Call
    /// `ChainSubmissionClient::advance_vote_batch_with_recovery` with
    /// `ChainRecoveryMode::ExactTree`; the lifecycle confirms every member
    /// atomically.
    AdvanceVoteBatch { bundle_index: u32, proposal_id: u32 },
    /// Resume helper-share submission for a committed vote.
    ///
    /// This covers the crash boundary after the cast-vote transaction confirms
    /// and before every helper-share row has been durably recorded. The
    /// `share_index` identifies one missing share for stable recovery UI and
    /// FFI routing. The host should recover the `CommittedVote`, call
    /// `CommittedVote::prepare_share_delivery` to load or create the
    /// SDK-persisted complete plan, convert it with `CommittedVote::confirmed`,
    /// and then call `ConfirmedVote::submit_prepared_shares` with the current
    /// helper fleet.
    /// Submission validates the immutable plan against its persisted planning
    /// fleet, contacts only current helpers, validates the whole batch before
    /// network I/O, journals every attempt and outcome, and resumes only the
    /// remaining definite-delivery deficits.
    SubmitShares {
        bundle_index: u32,
        proposal_id: u32,
        share_index: u32,
    },
    ConfirmShare {
        bundle_index: u32,
        proposal_id: u32,
        share_index: u32,
    },
}

impl NextStep {
    /// Typed discriminator exposed to hosts through [`crate::wire::NextStepView`].
    ///
    /// Hosts match on the enum rather than a string, so a new step kind is a
    /// compile-time event for them instead of a silently unmatched label:
    ///
    /// ```compile_fail
    /// let step = zcash_voting::session::NextStep::Delegate { bundle_index: 0 };
    /// let _: &str = step.kind();
    /// ```
    pub fn kind_view(&self) -> crate::wire::NextStepKind {
        use crate::wire::NextStepKind as Kind;
        match self {
            Self::Delegate { .. } => Kind::Delegate,
            Self::AdvanceDelegation { .. } => Kind::AdvanceDelegation,
            Self::AdvanceImportedDelegation { .. } => Kind::AdvanceImportedDelegation,
            Self::CastVote { .. } => Kind::CastVote,
            Self::AdvanceVote { .. } => Kind::AdvanceVote,
            Self::AdvanceVoteBatch { .. } => Kind::AdvanceVoteBatch,
            Self::SubmitShares { .. } => Kind::SubmitShares,
            Self::ConfirmShare { .. } => Kind::ConfirmShare,
        }
    }
}

/// High-level work area a wallet should show or resume for a round.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RoundPlanAction {
    /// No crate-owned recovery or submitted vote artifact is present.
    Idle,
    /// Delegation must be submitted or polled before fresh vote work can finish.
    Delegate,
    /// Cast-vote, vote-submission, vote-polling, or helper-share submission work remains.
    Vote,
    /// Only blocking helper-share confirmation work remains.
    SubmitShares,
    /// A vote artifact exists and no blocking recovery work remains.
    Done,
}

impl From<RoundPlanAction> for crate::wire::RoundPlanActionKind {
    fn from(action: RoundPlanAction) -> Self {
        match action {
            RoundPlanAction::Idle => Self::Idle,
            RoundPlanAction::Delegate => Self::Delegate,
            RoundPlanAction::Vote => Self::Vote,
            RoundPlanAction::SubmitShares => Self::SubmitShares,
            RoundPlanAction::Done => Self::Done,
        }
    }
}

/// Kind of delegation recovery work grouped from `NextStep`s.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DelegationRecoveryWorkKind {
    /// Run the delegation flow for this bundle.
    Delegate,
    /// Advance a delegation transaction that is already in flight.
    AdvanceDelegation,
    /// Advance an imported, already-broadcast delegation without signing.
    AdvanceImportedDelegation,
}

impl From<DelegationRecoveryWorkKind> for crate::wire::DelegationRecoveryWorkKindView {
    fn from(kind: DelegationRecoveryWorkKind) -> Self {
        match kind {
            DelegationRecoveryWorkKind::Delegate => Self::Delegate,
            DelegationRecoveryWorkKind::AdvanceDelegation => Self::AdvanceDelegation,
            DelegationRecoveryWorkKind::AdvanceImportedDelegation => {
                Self::AdvanceImportedDelegation
            }
        }
    }
}

/// Grouped delegation recovery work for one bundle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DelegationRecoveryWork {
    pub kind: DelegationRecoveryWorkKind,
    pub bundle_index: u32,
    pub phase: DelegationPhase,
    /// Present for either delegation-advancement kind when a hash is known.
    pub tx_hash: Option<String>,
}

/// Durable delegation state for one eligible bundle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DelegationStatus {
    pub bundle_index: u32,
    pub phase: DelegationPhase,
    pub tx_hash: Option<String>,
    /// Diagnostic stored on the bundle's authoritative lifecycle row.
    ///
    /// Always present for `SubmittedWithoutHash` and `SubmissionRejected`.
    /// Those phases are terminal and schedule no `next_steps`, so after a
    /// restart this field is how a host surfaces why the delegation needs
    /// manual handling. May also be present while `SubmissionManaged`
    /// recovers from an ambiguous dispatch.
    pub submission_diagnostic: Option<ChainSubmissionDiagnostic>,
    /// True when this bundle's delegation ended without a confirmation and no
    /// further delegation step will ever be planned for it: a terminal
    /// failure, not a terminal state.
    ///
    /// Such a bundle needs manual handling, not a retry: it was either
    /// rejected, or dispatched without a usable transaction hash and must not
    /// be resubmitted. `submission_diagnostic` says which. A `Confirmed`
    /// bundle is not terminal in this sense: it succeeded, and `phase` says
    /// so. Hosts cannot derive the failure from `phase` alone, because the
    /// wallet-facing phase reports a hashless dispatch and a healthy
    /// submission the same way.
    pub terminal: bool,
}

/// Kind of vote recovery work grouped from `NextStep`s.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum VoteRecoveryWorkKind {
    /// Advance a committed singleton vote using persisted recovery material.
    AdvanceVote,
    /// Advance a committed atomic vote batch using persisted recovery material.
    AdvanceVoteBatch,
    /// Submit one or more missing helper shares for a confirmed vote.
    SubmitShares,
}

impl From<VoteRecoveryWorkKind> for crate::wire::VoteRecoveryWorkKindView {
    fn from(kind: VoteRecoveryWorkKind) -> Self {
        match kind {
            VoteRecoveryWorkKind::AdvanceVote => Self::AdvanceVote,
            VoteRecoveryWorkKind::AdvanceVoteBatch => Self::AdvanceVoteBatch,
            VoteRecoveryWorkKind::SubmitShares => Self::SubmitShares,
        }
    }
}

/// Grouped vote recovery work keyed by one singleton action or batch anchor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoteRecoveryWork {
    pub kind: VoteRecoveryWorkKind,
    pub bundle_index: u32,
    /// Proposal key for singleton work, or the first ordered action used as the
    /// recovery anchor for batch work.
    pub proposal_id: u32,
    /// Transaction hash the lifecycle already knows for `AdvanceVote` or
    /// `AdvanceVoteBatch`: the confirmed hash, else the candidate hash of the
    /// in-flight generation. Absent before the first accepted POST, after a
    /// hashless recovery, and always for `SubmitShares`.
    pub tx_hash: Option<String>,
    /// Present only when `kind == VoteRecoveryWorkKind::SubmitShares`.
    pub vc_tree_position: Option<u64>,
    /// Empty unless `kind == VoteRecoveryWorkKind::SubmitShares`.
    pub share_indexes: Vec<u32>,
}

/// Display choice for one proposal in a completed round.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletedVoteChoice {
    pub proposal_id: u32,
    /// `None` when the proposal was skipped or bundles disagree.
    pub choice: Option<u32>,
}

/// Read-only display summary for a locally completed vote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletedVoteDisplay {
    pub choices: Vec<CompletedVoteChoice>,
    /// Latest helper-share delegation timestamp, in Unix seconds.
    pub voted_at: Option<u64>,
}

/// Derived resume state for one round.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoundPlan {
    pub round_id: String,
    /// True iff any recovery step remains (`!next_steps.is_empty()`).
    pub pending_recovery: bool,
    /// Ordered remaining recovery work.
    pub next_steps: Vec<NextStep>,
    /// Proposals with no terminal decision yet.
    ///
    /// `Decision::Skipped` is terminal for this plan, so skipped proposals are
    /// not returned here.
    pub open_proposals: Vec<u32>,
    /// Durable ballot intents for proposals the authenticated roster does not
    /// list, such as a decision recorded before a proposal was removed.
    /// Helper-plan derivation rejects them, so `CastVote` is withheld until
    /// the host clears them with `VotingDb::clear_ballot_intent`.
    ///
    /// An unrostered intent whose vote the chain lifecycle owns or has
    /// finished is omitted: it cannot be cleared, its vote is still driven to
    /// resolution, and it does not withhold casting for the current roster.
    pub unrostered_intents: Vec<u32>,
    /// The round's single immediate helper-share submission, if designated.
    ///
    /// Reports the round's durable designation whenever one exists, since
    /// that is what delivery executes; only while none exists is it derived
    /// from the current ballot choices.
    pub immediate_share_key: Option<ImmediateShareKey>,
    /// True when the designated immediate share has durable helper-quorum confirmation.
    pub immediate_share_confirmed: bool,
    /// Informational: every proposal is either a confirmed Choice or Skipped.
    pub all_decided: bool,
    /// Durable delegation status for every eligible bundle in the round.
    pub delegation_statuses: Vec<DelegationStatus>,
    /// True when the plan contains work that should keep the foreground vote flow open.
    ///
    /// Unconfirmed helper shares that were already accepted by at least one helper
    /// remain in `next_steps` as `ConfirmShare`, but they are non-blocking because
    /// background polling can complete them later.
    pub blocking_recovery: bool,
    /// True when an unconfirmed helper-share row has no accepted helper URL yet.
    pub blocking_share_work: bool,
    /// True when any helper-share row is still unconfirmed, whether or not a
    /// helper has already accepted it.
    ///
    /// Hosts schedule background share tracking from this rather than holding
    /// share rows themselves; `blocking_share_work` is the stricter subset
    /// that must block the foreground vote flow.
    pub has_unconfirmed_shares: bool,
    /// True once round artifacts require the same voting hotkey to be reused.
    pub hotkey_bound: bool,
    /// True once the local DB contains a vote or helper-share artifact for the round.
    pub completed_vote_artifact: bool,
    /// True when a vote artifact exists and no blocking recovery work remains.
    pub completed_for_display: bool,
    /// Display data for a completed vote, present only when `completed_for_display`.
    pub completed_vote_display: Option<CompletedVoteDisplay>,
    /// True when a wallet should collect or restore draft choices for open proposals.
    pub needs_draft_setup: bool,
    /// True when the round holds a ballot choice but no bundle rows yet.
    ///
    /// Eligibility checks do not persist bundles, so a host that records a
    /// ballot before running bundle setup sees this instead of an error. The
    /// remedy is to persist the bundle plan for the round (`ensure_bundles`,
    /// or the pipeline's setup/precompute entry points) and plan again; no
    /// vote work can be planned until then.
    pub needs_bundle_setup: bool,
    /// Primary work area derived from the crate-owned recovery state.
    pub primary_action: RoundPlanAction,
    /// True when delegation work needs fresh or restored wallet signing material.
    ///
    /// Hosts should read this instead of scanning `next_steps` for a kind.
    /// Every derived predicate below is computed from an exhaustive match over
    /// [`NextStep`], so a new step variant is a compile error here rather than
    /// silently reading as "no work" in a downstream allowlist.
    pub needs_delegation_signing: bool,
    /// True when a delegation is durably in flight. Consult
    /// `needs_delegation_signing` to learn whether its next pass also needs
    /// signing material.
    pub has_in_flight_delegation: bool,
    /// True when vote or helper-share work remains that the host should keep
    /// driving: casting, advancing a chain submission, or submitting shares.
    ///
    /// Excludes helper-share confirmation, which background polling completes.
    pub needs_vote_polling: bool,
    /// True when any vote or share work remains, counting share confirmation
    /// only when it is blocking.
    pub has_remaining_vote_or_share_work: bool,
    /// True when any vote or share work remains that is actionable from
    /// persisted state, counting share confirmation unconditionally.
    pub has_recoverable_vote_or_share_work: bool,
    /// Delegation recovery work grouped from `next_steps` for wallet orchestration.
    pub recovered_delegation_work: Vec<DelegationRecoveryWork>,
    /// Vote recovery work grouped from `next_steps` for wallet orchestration.
    pub recovered_vote_work: Vec<VoteRecoveryWork>,
}

/// Build the resume plan for `round_id`.
///
/// `proposal_ids` is the round's full set of proposal ids (from the wallet's
/// round config); the crate cannot enumerate "never decided" proposals on its
/// own. The plan is a best-effort snapshot over the durable phase tables.
/// Wallets should execute one returned step, persist that step's result, then
/// call `resume_plan` again; later steps may depend on earlier on-chain
/// confirmations.
///
/// A proposal in `proposal_ids` with no durable ballot intent is reported in
/// `RoundPlan::open_proposals` and suppresses [`NextStep::CastVote`] for the
/// whole round; see that variant for why the ballot must be terminal first.
/// Delegation and the advancement of votes already on the wire are planned
/// regardless.
pub fn resume_plan(
    db: &VotingDb,
    round_id: &str,
    proposal_ids: &[u32],
) -> Result<RoundPlan, VotingError> {
    crate::round_planning::plan_round(db, round_id, proposal_ids)
}
