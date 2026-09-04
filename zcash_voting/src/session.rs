//! Durable ballot intent + resumable voting-session planner.
//!
//! `resume_plan` is pure and I/O-free over the wallet's voting DB: it reports
//! the ordered remaining work for a round, built on the per-artifact phase
//! APIs in `crate::phases`. The wallet executes each step with its own
//! network/proof/sign plumbing.

use rusqlite::{named_params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

use crate::chain_submission::planning::{
    delegation_is_capability_imported, delegation_transaction_hash, vote_batch_transaction_hash,
    vote_transaction_hash,
};
use crate::chain_submission::ChainSubmissionDiagnostic;
use crate::phases::{DelegationPhase, DelegationSubmissionStatus, SharePhase, VotePhase};
use crate::share_policy::{round_immediate_share_key, ImmediateShareKey};
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
    /// Removes the durable decision for one proposal.
    ///
    /// A decision that survives a roster change refers to a proposal the
    /// authenticated configuration no longer lists; the planner reports it in
    /// `RoundPlan::unrostered_intents` and withholds `CastVote` until it is
    /// cleared. Clearing an intent that does not exist is not an error. An
    /// intent whose vote is already submitted cannot be cleared.
    pub fn clear_ballot_intent(&self, round_id: &str, proposal_id: u32) -> Result<(), VotingError> {
        validate_proposal_id(proposal_id)?;
        let wallet_id = self.wallet_id();
        self.write_transaction("clear_ballot_intent transaction failed", |tx| {
            let submitted: Option<i64> = tx
                .query_row(
                    "SELECT bundle_index FROM votes
                     WHERE round_id = :round_id AND wallet_id = :wallet_id
                       AND proposal_id = :proposal_id AND tx_hash IS NOT NULL
                     LIMIT 1",
                    named_params! {
                        ":round_id": round_id,
                        ":wallet_id": wallet_id,
                        ":proposal_id": proposal_id as i64,
                    },
                    |row| row.get(0),
                )
                .optional()
                .map_err(|e| VotingError::from_sqlite("check submitted vote before clearing intent", &e))?;
            if let Some(bundle_index) = submitted {
                return Err(VotingError::InvalidInput {
                    message: format!(
                        "round {round_id} bundle {bundle_index} proposal {proposal_id} has a submitted vote; its intent cannot be cleared"
                    ),
                });
            }
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

    pub fn set_ballot_intents(
        &self,
        round_id: &str,
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
    .map_err(|e| VotingError::Internal {
        message: format!("set_ballot_intent failed: {e}"),
    })?;
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
    .map_err(|e| VotingError::Internal {
        message: format!("failed to clear stale share delegations: {e}"),
    })?;
    Ok(())
}

impl VotingDb {
    /// Load the voter's decisions for a round, sorted by proposal id.
    pub fn ballot_intents(&self, round_id: &str) -> Result<Vec<(u32, Decision)>, VotingError> {
        let conn = self.conn();
        let wallet_id = self.wallet_id();
        let mut stmt = conn
            .prepare(
                "SELECT proposal_id, skipped, choice FROM ballot_intent
                 WHERE round_id = :round_id AND wallet_id = :wallet_id
                 ORDER BY proposal_id",
            )
            .map_err(|e| VotingError::Internal {
                message: format!("prepare ballot_intents: {e}"),
            })?;
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
                        Decision::Choice(choice.unwrap_or(0) as u32)
                    };
                    Ok((pid, decision))
                },
            )
            .map_err(|e| VotingError::Internal {
                message: format!("query ballot_intents: {e}"),
            })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| VotingError::Internal {
                message: format!("collect ballot_intents: {e}"),
            })
    }
}

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
    /// complete persisted plan. After confirmation, submit through
    /// `CommittedVote::submit_prepared_shares` with the current fleet. A
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
    /// SDK-persisted complete plan, and then call
    /// `CommittedVote::submit_prepared_shares` with the current helper fleet.
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
    pub unrostered_intents: Vec<u32>,
    /// The round's single immediate helper-share submission, if designated.
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

fn step_rank(step: &NextStep) -> (u32, u32, u32, u32) {
    // Delegation is a prerequisite for fresh vote work, so keep it before
    // vote/share recovery. Vote work is proposal-primary so an interrupted
    // question finishes across all bundles before later questions resume.
    match step {
        NextStep::Delegate { bundle_index } => (0, 0, *bundle_index, 0),
        NextStep::AdvanceDelegation { bundle_index }
        | NextStep::AdvanceImportedDelegation { bundle_index } => (0, 0, *bundle_index, 0),
        NextStep::CastVote {
            bundle_index,
            proposal_id,
            choice: _,
        } => (1, *proposal_id, *bundle_index, 0),
        NextStep::AdvanceVote {
            bundle_index,
            proposal_id,
        }
        | NextStep::AdvanceVoteBatch {
            bundle_index,
            proposal_id,
        } => (1, *proposal_id, *bundle_index, 0),
        NextStep::SubmitShares {
            bundle_index,
            proposal_id,
            share_index,
        } => (1, *proposal_id, *bundle_index, *share_index),
        NextStep::ConfirmShare {
            bundle_index,
            proposal_id,
            share_index,
        } => (2, *proposal_id, *bundle_index, *share_index),
    }
}

/// The earlier step in `steps` that must clear before `step` can run.
///
/// Plans are rank-ordered, but rank is proposal-primary and says nothing about
/// per-bundle dependencies. Vote and share work for a bundle requires that
/// bundle's delegation to be confirmed, so while `steps` still holds a
/// `Delegate`, `AdvanceDelegation`, or `AdvanceImportedDelegation` for the
/// same bundle, that step is the blocking prerequisite. Delegation steps and
/// steps on other bundles have none.
pub(crate) fn blocking_prerequisite<'a>(
    steps: &'a [NextStep],
    step: &NextStep,
) -> Option<&'a NextStep> {
    let dependent_bundle = match step {
        NextStep::Delegate { .. }
        | NextStep::AdvanceDelegation { .. }
        | NextStep::AdvanceImportedDelegation { .. } => return None,
        NextStep::CastVote { bundle_index, .. }
        | NextStep::AdvanceVote { bundle_index, .. }
        | NextStep::AdvanceVoteBatch { bundle_index, .. }
        | NextStep::SubmitShares { bundle_index, .. }
        | NextStep::ConfirmShare { bundle_index, .. } => *bundle_index,
    };
    steps.iter().find(|candidate| {
        matches!(
            candidate,
            NextStep::Delegate { bundle_index }
                | NextStep::AdvanceDelegation { bundle_index }
                | NextStep::AdvanceImportedDelegation { bundle_index }
                if *bundle_index == dependent_bundle
        )
    })
}

fn missing_recovery_field(message: String) -> VotingError {
    VotingError::Internal { message }
}

fn delegation_statuses(
    db: &VotingDb,
    round_id: &str,
    delegation: &[DelegationSubmissionStatus],
) -> Result<Vec<DelegationStatus>, VotingError> {
    delegation
        .iter()
        .map(|status| {
            Ok(DelegationStatus {
                bundle_index: status.bundle_index,
                phase: status.phase,
                tx_hash: delegation_transaction_hash(db, round_id, status.bundle_index)?,
                submission_diagnostic: status.diagnostic.clone(),
                terminal: is_terminal_delegation_phase(status.phase),
            })
        })
        .collect()
}

/// Whether a delegation phase schedules no further work.
///
/// Whether a delegation ended without a confirmation and will never be
/// planned again; `Confirmed` is a success, not a terminal failure.
///
/// Exhaustive on purpose: a new phase must be classified here rather than
/// defaulting into "retry", which for a hashless dispatch would resubmit a
/// transaction that may already be on the chain.
fn is_terminal_delegation_phase(phase: DelegationPhase) -> bool {
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

fn recovered_delegation_work_from_steps(
    db: &VotingDb,
    round_id: &str,
    delegation: &BTreeMap<u32, DelegationPhase>,
    steps: &[NextStep],
) -> Result<Vec<DelegationRecoveryWork>, VotingError> {
    let mut work = Vec::<DelegationRecoveryWork>::new();
    for step in steps {
        match *step {
            NextStep::Delegate { bundle_index } => {
                let phase = delegation.get(&bundle_index).copied().ok_or_else(|| {
                    missing_recovery_field(format!(
                        "delegate step missing phase for round={round_id}, bundle={bundle_index}"
                    ))
                })?;
                work.push(DelegationRecoveryWork {
                    kind: DelegationRecoveryWorkKind::Delegate,
                    bundle_index,
                    phase,
                    tx_hash: None,
                });
            }
            NextStep::AdvanceDelegation { bundle_index } => {
                let phase = delegation.get(&bundle_index).copied().ok_or_else(|| {
                    missing_recovery_field(format!(
                        "poll delegation step missing phase for round={round_id}, bundle={bundle_index}"
                    ))
                })?;
                // A reserved-but-undispatched generation has no hash yet, so
                // the hash is reported when known rather than required.
                let tx_hash = delegation_transaction_hash(db, round_id, bundle_index)?;
                work.push(DelegationRecoveryWork {
                    kind: DelegationRecoveryWorkKind::AdvanceDelegation,
                    bundle_index,
                    phase,
                    tx_hash,
                });
            }
            NextStep::AdvanceImportedDelegation { bundle_index } => {
                let phase = delegation.get(&bundle_index).copied().ok_or_else(|| {
                    missing_recovery_field(format!(
                        "imported delegation step missing phase for round={round_id}, bundle={bundle_index}"
                    ))
                })?;
                let tx_hash = delegation_transaction_hash(db, round_id, bundle_index)?;
                work.push(DelegationRecoveryWork {
                    kind: DelegationRecoveryWorkKind::AdvanceImportedDelegation,
                    bundle_index,
                    phase,
                    tx_hash,
                });
            }
            // Listed exhaustively on purpose: a new step must be classified
            // here rather than silently dropped.
            NextStep::CastVote { .. }
            | NextStep::AdvanceVote { .. }
            | NextStep::AdvanceVoteBatch { .. }
            | NextStep::SubmitShares { .. }
            | NextStep::ConfirmShare { .. } => {}
        }
    }
    Ok(work)
}

fn recovered_vote_work_from_steps(
    db: &VotingDb,
    round_id: &str,
    blocking_confirm_share_keys: &BTreeSet<(u32, u32, u32)>,
    active_vote_batches: &BTreeMap<(u32, u32), ActiveVoteBatch>,
    steps: &[NextStep],
) -> Result<Vec<VoteRecoveryWork>, VotingError> {
    let mut work = Vec::<VoteRecoveryWork>::new();
    let mut pending_vote_confirmation_keys = BTreeSet::new();
    for step in steps {
        match step {
            NextStep::AdvanceVote {
                bundle_index,
                proposal_id,
            } => {
                pending_vote_confirmation_keys.insert((*bundle_index, *proposal_id));
            }
            NextStep::AdvanceVoteBatch {
                bundle_index,
                proposal_id,
            } => {
                let batch = active_vote_batches
                    .get(&(*bundle_index, *proposal_id))
                    .ok_or_else(|| {
                        missing_recovery_field(format!(
                            "advance vote batch step missing active batch for round={round_id}, bundle={bundle_index}, proposal={proposal_id}"
                        ))
                    })?;
                pending_vote_confirmation_keys.extend(
                    active_vote_batches
                        .iter()
                        .filter(|((member_bundle_index, _), member_batch)| {
                            member_bundle_index == bundle_index
                                && member_batch.digest == batch.digest
                        })
                        .map(|(vote_key, _)| *vote_key),
                );
            }
            // Listed exhaustively on purpose: a new step must be classified
            // here rather than silently dropped.
            NextStep::Delegate { .. }
            | NextStep::AdvanceDelegation { .. }
            | NextStep::AdvanceImportedDelegation { .. }
            | NextStep::CastVote { .. }
            | NextStep::SubmitShares { .. }
            | NextStep::ConfirmShare { .. } => {}
        }
    }
    for step in steps {
        match *step {
            // A reserved-but-undispatched generation has no hash yet, so the
            // hash is reported when known rather than required.
            NextStep::AdvanceVote {
                bundle_index,
                proposal_id,
            } => {
                let tx_hash = vote_transaction_hash(db, round_id, bundle_index, proposal_id)?;
                work.push(VoteRecoveryWork {
                    kind: VoteRecoveryWorkKind::AdvanceVote,
                    bundle_index,
                    proposal_id,
                    tx_hash,
                    vc_tree_position: None,
                    share_indexes: Vec::new(),
                });
            }
            NextStep::AdvanceVoteBatch {
                bundle_index,
                proposal_id,
            } => {
                // The batch row is the authority for an in-flight batch; the
                // anchor member's own columns hold nothing until confirmation.
                let batch = active_vote_batches
                    .get(&(bundle_index, proposal_id))
                    .expect("classified above from the same step list");
                let tx_hash = vote_batch_transaction_hash(
                    db,
                    round_id,
                    bundle_index,
                    batch.digest,
                    proposal_id,
                )?;
                work.push(VoteRecoveryWork {
                    kind: VoteRecoveryWorkKind::AdvanceVoteBatch,
                    bundle_index,
                    proposal_id,
                    tx_hash,
                    vc_tree_position: None,
                    share_indexes: Vec::new(),
                });
            }
            NextStep::SubmitShares {
                bundle_index,
                proposal_id,
                share_index,
            } => push_submit_share_work(
                db,
                round_id,
                &mut work,
                bundle_index,
                proposal_id,
                share_index,
            )?,
            NextStep::ConfirmShare {
                bundle_index,
                proposal_id,
                share_index,
            } if blocking_confirm_share_keys.contains(&(
                bundle_index,
                proposal_id,
                share_index,
            )) && !pending_vote_confirmation_keys.contains(&(bundle_index, proposal_id)) =>
            {
                push_submit_share_work(
                    db,
                    round_id,
                    &mut work,
                    bundle_index,
                    proposal_id,
                    share_index,
                )?;
            }
            // Listed exhaustively on purpose: a new step must be classified
            // here rather than silently dropped.
            NextStep::Delegate { .. }
            | NextStep::AdvanceDelegation { .. }
            | NextStep::AdvanceImportedDelegation { .. }
            | NextStep::CastVote { .. }
            | NextStep::ConfirmShare { .. } => {}
        }
    }
    Ok(work)
}

fn push_submit_share_work(
    db: &VotingDb,
    round_id: &str,
    work: &mut Vec<VoteRecoveryWork>,
    bundle_index: u32,
    proposal_id: u32,
    share_index: u32,
) -> Result<(), VotingError> {
    if let Some(existing) = work.iter_mut().find(|item| {
        item.kind == VoteRecoveryWorkKind::SubmitShares
            && item.bundle_index == bundle_index
            && item.proposal_id == proposal_id
    }) {
        existing.share_indexes.push(share_index);
        existing.share_indexes.sort_unstable();
        existing.share_indexes.dedup();
        return Ok(());
    }

    let vc_tree_position = db
        .get_commitment_bundle(round_id, bundle_index, proposal_id)?
        .map(|(_, position)| position)
        .ok_or_else(|| {
            missing_recovery_field(format!(
                "submit shares step missing vc_tree_position for round={round_id}, bundle={bundle_index}, proposal={proposal_id}"
            ))
        })?;
    work.push(VoteRecoveryWork {
        kind: VoteRecoveryWorkKind::SubmitShares,
        bundle_index,
        proposal_id,
        tx_hash: None,
        vc_tree_position: Some(vc_tree_position),
        share_indexes: vec![share_index],
    });
    Ok(())
}

fn select_primary_action(
    steps: &[NextStep],
    blocking_recovery: bool,
    blocking_share_work: bool,
    completed_for_display: bool,
) -> RoundPlanAction {
    if completed_for_display {
        return RoundPlanAction::Done;
    }
    if !blocking_recovery {
        return RoundPlanAction::Idle;
    }
    if steps.iter().any(|step| {
        matches!(
            step,
            NextStep::Delegate { .. }
                | NextStep::AdvanceDelegation { .. }
                | NextStep::AdvanceImportedDelegation { .. }
        )
    }) {
        return RoundPlanAction::Delegate;
    }
    if steps.iter().any(|step| {
        matches!(
            step,
            NextStep::CastVote { .. }
                | NextStep::AdvanceVote { .. }
                | NextStep::AdvanceVoteBatch { .. }
                | NextStep::SubmitShares { .. }
        )
    }) {
        return RoundPlanAction::Vote;
    }
    if blocking_share_work {
        RoundPlanAction::SubmitShares
    } else {
        RoundPlanAction::Idle
    }
}

fn completed_vote_display(
    proposal_ids: &[u32],
    intents: &BTreeMap<u32, Decision>,
    vote_choices: &BTreeMap<(u32, u32), u32>,
    stale_vote_keys: &BTreeSet<(u32, u32)>,
    voted_at: Option<u64>,
) -> CompletedVoteDisplay {
    let choices = proposal_ids
        .iter()
        .map(|&proposal_id| {
            let proposal_choices = vote_choices
                .iter()
                .filter_map(|(&(bundle_index, vote_proposal_id), &choice)| {
                    (vote_proposal_id == proposal_id
                        && !stale_vote_keys.contains(&(bundle_index, vote_proposal_id)))
                    .then_some(choice)
                })
                .collect::<BTreeSet<_>>();
            let choice = match intents.get(&proposal_id) {
                Some(Decision::Skipped) => None,
                Some(Decision::Choice(_)) if proposal_choices.len() == 1 => {
                    proposal_choices.first().copied()
                }
                _ => None,
            };
            CompletedVoteChoice {
                proposal_id,
                choice,
            }
        })
        .collect();

    CompletedVoteDisplay { choices, voted_at }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ActiveVoteBatch {
    digest: [u8; 32],
    anchor_proposal_id: u32,
}

fn active_vote_batches_by_vote(
    db: &VotingDb,
    round_id: &str,
    votes: &BTreeMap<(u32, u32), VotePhase>,
    vote_choices: &BTreeMap<(u32, u32), u32>,
    intents: &BTreeMap<u32, Decision>,
) -> Result<BTreeMap<(u32, u32), ActiveVoteBatch>, VotingError> {
    let mut batches_by_vote = BTreeMap::new();
    let wallet_id = db.wallet_id();

    for (&(bundle_index, proposal_id), &phase) in votes {
        if !matches!(
            phase,
            VotePhase::Committed | VotePhase::Submitted | VotePhase::SubmissionManaged
        ) {
            continue;
        }
        if batches_by_vote.contains_key(&(bundle_index, proposal_id)) {
            continue;
        }
        let Some(recovery) = crate::vote::recovery_bundle(db, round_id, bundle_index, proposal_id)?
        else {
            continue;
        };
        let Some(batch) = recovery.batch else {
            continue;
        };

        let recoveries = {
            let conn = db.conn();
            crate::vote::load_vote_batch_recoveries_with_conn(
                &conn,
                &wallet_id,
                round_id,
                bundle_index,
                batch.digest,
            )?
        };
        let anchor_proposal_id = recoveries
            .first()
            .map(|recovery| recovery.proposal_id)
            .ok_or_else(|| VotingError::InvalidInput {
                message: format!(
                    "persisted atomic vote batch is empty for round={round_id}, bundle={bundle_index}"
                ),
            })?;
        let mut shared_tx_hash: Option<String> = None;

        for recovery in &recoveries {
            let vote_key = (bundle_index, recovery.proposal_id);
            let member_phase = votes.get(&vote_key).copied().ok_or_else(|| {
                VotingError::InvalidInput {
                    message: format!(
                        "persisted atomic vote batch is missing proposal {} for round={round_id}, bundle={bundle_index}",
                        recovery.proposal_id
                    ),
                }
            })?;
            if member_phase != phase {
                return Err(VotingError::InvalidInput {
                    message: format!(
                        "persisted atomic vote batch has mixed phases for round={round_id}, bundle={bundle_index}: proposal {} is {}, expected {}",
                        recovery.proposal_id,
                        member_phase.as_str(),
                        phase.as_str()
                    ),
                });
            }
            // A member with no recorded intent is not a conflict: the batch is
            // lifecycle-owned and must stay schedulable whatever the host has
            // recorded so far. Only a differing or skipped intent conflicts.
            if vote_choices.get(&vote_key) != Some(&recovery.vote_decision)
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
                if let Some(tx_hash) =
                    vote_transaction_hash(db, round_id, bundle_index, recovery.proposal_id)?
                {
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
        }

        let batch = ActiveVoteBatch {
            digest: batch.digest,
            anchor_proposal_id,
        };
        for recovery in recoveries {
            let vote_key = (bundle_index, recovery.proposal_id);
            if batches_by_vote.insert(vote_key, batch).is_some() {
                return Err(VotingError::InvalidInput {
                    message: format!(
                        "vote belongs to more than one atomic batch for round={round_id}, bundle={bundle_index}, proposal={}",
                        recovery.proposal_id
                    ),
                });
            }
        }
    }

    Ok(batches_by_vote)
}

/// Schedules the chain-advancement step for one vote that is committed,
/// submitted, or lifecycle-owned.
///
/// A batch member schedules one `AdvanceVoteBatch` per batch, keyed by the
/// anchor proposal; a singleton schedules `AdvanceVote`. A `Submitted` vote
/// must still hold its recovery material, because advancement reconstructs
/// the transaction from it.
fn push_vote_advance_step(
    db: &VotingDb,
    round_id: &str,
    steps: &mut Vec<NextStep>,
    planned_vote_batches: &mut BTreeSet<(u32, [u8; 32])>,
    active_vote_batches: &BTreeMap<(u32, u32), ActiveVoteBatch>,
    vote_key: (u32, u32),
    phase: VotePhase,
) -> Result<(), VotingError> {
    let (bundle_index, proposal_id) = vote_key;
    if phase == VotePhase::Submitted
        && !vote_has_recovery_bundle(db, round_id, bundle_index, proposal_id)?
    {
        return Err(VotingError::InvalidInput {
            message: format!(
                "round {round_id} bundle {bundle_index} proposal {proposal_id} has a submitted vote without recovery material"
            ),
        });
    }
    if let Some(batch) = active_vote_batches.get(&vote_key) {
        if planned_vote_batches.insert((bundle_index, batch.digest)) {
            steps.push(NextStep::AdvanceVoteBatch {
                bundle_index,
                proposal_id: batch.anchor_proposal_id,
            });
        }
    } else {
        steps.push(NextStep::AdvanceVote {
            bundle_index,
            proposal_id,
        });
    }
    Ok(())
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
    let delegation_submissions = db.delegation_submission_statuses(round_id)?;
    let delegation: BTreeMap<u32, DelegationPhase> = delegation_submissions
        .iter()
        .map(|status| (status.bundle_index, status.phase))
        .collect();
    let votes: BTreeMap<(u32, u32), VotePhase> = db
        .vote_phases(round_id)?
        .into_iter()
        .map(|(b, p, ph)| ((b, p), ph))
        .collect();
    let vote_choices: BTreeMap<(u32, u32), u32> = db
        .get_votes(round_id)?
        .into_iter()
        .map(|vote| ((vote.bundle_index, vote.proposal_id), vote.choice))
        .collect();
    let share_phase_rows = db.share_phases(round_id)?;
    let share_delegations = db.get_share_delegations(round_id)?;
    let share_indexes_by_vote = share_phase_rows.iter().fold(
        BTreeMap::<(u32, u32), BTreeSet<u32>>::new(),
        |mut acc, (bundle_index, proposal_id, share_index, _)| {
            acc.entry((*bundle_index, *proposal_id))
                .or_default()
                .insert(*share_index);
            acc
        },
    );
    let intents: BTreeMap<u32, Decision> = db.ballot_intents(round_id)?.into_iter().collect();
    let intent_classification = classify_ballot_intents(proposal_ids, &intents)?;

    let bundles: Vec<u32> = delegation.keys().copied().collect();
    let choice_proposals = intent_classification.choice_proposals;
    let open_proposals = intent_classification.open_proposals;
    let unrostered_intents = intent_classification.unrostered_intents;
    // Casting derives the round's single immediate helper share from the
    // complete set of choices, so `CommittedVote::prepare_share_delivery`
    // rejects a roster that still holds an undecided proposal, and equally a
    // durable intent for a proposal outside the authenticated roster.
    // Planning a `CastVote` before both hold would advertise a step that only
    // fails after proving and persisting the vote, so the durable intents must
    // exactly match the roster first.
    let roster_is_terminal = open_proposals.is_empty() && unrostered_intents.is_empty();

    if !choice_proposals.is_empty() && bundles.is_empty() {
        return Err(VotingError::InvalidInput {
            message: format!(
                "round {round_id} has ballot choice intent but no eligible bundle rows"
            ),
        });
    }

    let mut steps: Vec<NextStep> = Vec::new();
    let mut bundles_needing_delegation: BTreeSet<u32> = BTreeSet::new();
    let stale_vote_keys: BTreeSet<(u32, u32)> = vote_choices
        .iter()
        .filter_map(|(&(bundle_index, proposal_id), &stored_choice)| {
            match intents.get(&proposal_id) {
                Some(Decision::Choice(intent_choice)) if *intent_choice != stored_choice => {
                    Some((bundle_index, proposal_id))
                }
                Some(Decision::Skipped) => Some((bundle_index, proposal_id)),
                _ => None,
            }
        })
        .collect();
    let active_vote_batches =
        active_vote_batches_by_vote(db, round_id, &votes, &vote_choices, &intents)?;
    let mut planned_vote_batches = BTreeSet::new();
    let bundles_with_pending_vote_chains = votes
        .iter()
        .filter(|(key, phase)| {
            !stale_vote_keys.contains(key)
                && matches!(
                    phase,
                    VotePhase::Committed
                        | VotePhase::Submitted
                        | VotePhase::SubmissionManaged
                        | VotePhase::SubmittedWithoutHash
                )
        })
        .map(|(&(bundle_index, _), _)| bundle_index)
        .chain(delegation.iter().filter_map(|(&bundle_index, phase)| {
            matches!(
                phase,
                DelegationPhase::SubmissionManaged
                    | DelegationPhase::SubmittedWithoutHash
                    | DelegationPhase::SubmissionRejected
            )
            .then_some(bundle_index)
        }))
        .collect::<BTreeSet<_>>();

    for &(bundle_index, proposal_id) in &stale_vote_keys {
        if matches!(
            votes.get(&(bundle_index, proposal_id)),
            Some(
                VotePhase::Submitted
                    | VotePhase::SubmissionManaged
                    | VotePhase::SubmittedWithoutHash
                    | VotePhase::SubmissionRejected
                    | VotePhase::Confirmed
            )
        ) {
            return Err(VotingError::InvalidInput {
                message: format!(
                    "round {round_id} bundle {bundle_index} proposal {proposal_id} has a submitted vote that conflicts with ballot intent"
                ),
            });
        }
    }

    // Vote steps for answered proposals.
    for &pid in &choice_proposals {
        let intent_choice = match intents.get(&pid) {
            Some(Decision::Choice(choice)) => *choice,
            _ => continue,
        };
        for &b in &bundles {
            let vote_key = (b, pid);
            if stale_vote_keys.contains(&vote_key)
                || vote_choices.get(&vote_key) != Some(&intent_choice)
            {
                if bundles_with_pending_vote_chains.contains(&b) {
                    continue;
                }
                // Delegation is a prerequisite either way, so it is still
                // planned while the voter decides the rest of the roster.
                bundles_needing_delegation.insert(b);
                if roster_is_terminal {
                    steps.push(NextStep::CastVote {
                        bundle_index: b,
                        proposal_id: pid,
                        choice: intent_choice,
                    });
                }
                continue;
            }
            match votes.get(&vote_key) {
                Some(VotePhase::Confirmed) => {
                    for share_index in missing_share_indexes_for_confirmed_vote(
                        db,
                        round_id,
                        b,
                        pid,
                        share_indexes_by_vote
                            .get(&vote_key)
                            .cloned()
                            .unwrap_or_default(),
                    )? {
                        steps.push(NextStep::SubmitShares {
                            bundle_index: b,
                            proposal_id: pid,
                            share_index,
                        });
                    }
                }
                Some(
                    phase @ (VotePhase::Committed
                    | VotePhase::Submitted
                    | VotePhase::SubmissionManaged),
                ) => {
                    push_vote_advance_step(
                        db,
                        round_id,
                        &mut steps,
                        &mut planned_vote_batches,
                        &active_vote_batches,
                        vote_key,
                        *phase,
                    )?;
                }
                Some(VotePhase::SubmittedWithoutHash) => {}
                Some(VotePhase::SubmissionRejected) => {}
                // Prepared or no row yet -> still needs casting.
                _ => {
                    if bundles_with_pending_vote_chains.contains(&b) {
                        continue;
                    }
                    bundles_needing_delegation.insert(b);
                    if roster_is_terminal {
                        steps.push(NextStep::CastVote {
                            bundle_index: b,
                            proposal_id: pid,
                            choice: intent_choice,
                        });
                    }
                }
            }
        }
    }

    // Advancement for votes already on the wire does not depend on ballot
    // intent. A lifecycle-owned or submitted vote whose proposal has no
    // recorded intent is still the wallet's transaction and must be driven to
    // resolution; a skipped or differing intent was rejected above as a
    // conflict, so only intent-less proposals reach this pass.
    for (&vote_key, &phase) in &votes {
        let (b, pid) = vote_key;
        if intents.contains_key(&pid)
            || !matches!(phase, VotePhase::Submitted | VotePhase::SubmissionManaged)
        {
            continue;
        }
        push_vote_advance_step(
            db,
            round_id,
            &mut steps,
            &mut planned_vote_batches,
            &active_vote_batches,
            (b, pid),
            phase,
        )?;
    }

    // Delegation steps: resume any mid-flight delegation; otherwise only the
    // prerequisite for a bundle that still has a vote to cast.
    for &b in &bundles {
        match delegation.get(&b) {
            Some(DelegationPhase::Confirmed) => {}
            Some(DelegationPhase::Submitted | DelegationPhase::SubmissionManaged) => {
                if delegation_is_capability_imported(db, round_id, b)? {
                    steps.push(NextStep::AdvanceImportedDelegation { bundle_index: b });
                } else {
                    steps.push(NextStep::AdvanceDelegation { bundle_index: b });
                }
            }
            Some(DelegationPhase::SubmittedWithoutHash) => {}
            Some(DelegationPhase::SubmissionRejected) => {}
            // Prepared / PcztBuilt / Proved: still needs the delegate flow.
            _ => {
                if bundles_needing_delegation.contains(&b) {
                    steps.push(NextStep::Delegate { bundle_index: b });
                }
            }
        }
    }

    // Confirm already-submitted helper shares.
    for &(b, p, s, phase) in &share_phase_rows {
        if stale_vote_keys.contains(&(b, p)) {
            continue;
        }
        match phase {
            SharePhase::Submitted => {
                steps.push(NextStep::ConfirmShare {
                    bundle_index: b,
                    proposal_id: p,
                    share_index: s,
                });
            }
            SharePhase::Confirmed => {}
        }
    }

    steps.sort_by_key(step_rank);

    let confirm_share_step_keys = steps
        .iter()
        .filter_map(|step| match step {
            NextStep::ConfirmShare {
                bundle_index,
                proposal_id,
                share_index,
            } => Some((*bundle_index, *proposal_id, *share_index)),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    let blocking_confirm_share_keys = db
        .get_unconfirmed_delegations(round_id)?
        .into_iter()
        .filter(|share| share.sent_to_urls.is_empty())
        .filter(|share| {
            confirm_share_step_keys.contains(&(
                share.bundle_index,
                share.proposal_id,
                share.share_index,
            ))
        })
        .map(|share| (share.bundle_index, share.proposal_id, share.share_index))
        .collect::<BTreeSet<_>>();
    let blocking_share_work = !blocking_confirm_share_keys.is_empty();
    let submission_managed = delegation
        .values()
        .any(|phase| *phase == DelegationPhase::SubmissionManaged)
        || votes
            .values()
            .any(|phase| *phase == VotePhase::SubmissionManaged);
    // Terminal hashless dispatch keeps the foreground closed but schedules
    // no recovery step, so it contributes to `blocking_recovery` only.
    let submitted_without_hash = delegation
        .values()
        .any(|phase| *phase == DelegationPhase::SubmittedWithoutHash)
        || votes
            .values()
            .any(|phase| *phase == VotePhase::SubmittedWithoutHash);
    let submission_rejected = delegation
        .values()
        .any(|phase| *phase == DelegationPhase::SubmissionRejected)
        || votes
            .values()
            .any(|phase| *phase == VotePhase::SubmissionRejected);
    let blocking_recovery = submission_managed
        || submitted_without_hash
        || submission_rejected
        || steps.iter().any(|step| match step {
            NextStep::ConfirmShare {
                bundle_index,
                proposal_id,
                share_index,
            } => blocking_confirm_share_keys.contains(&(*bundle_index, *proposal_id, *share_index)),
            _ => true,
        });

    let delegation_statuses = delegation_statuses(db, round_id, &delegation_submissions)?;
    let hotkey_bound = delegation
        .values()
        .any(|phase| *phase != DelegationPhase::Prepared)
        || !votes.is_empty()
        || !share_phase_rows.is_empty();
    let all_decided = proposal_ids.iter().all(|&pid| match intents.get(&pid) {
        Some(Decision::Skipped) => true,
        Some(Decision::Choice(choice)) => {
            !bundles.is_empty()
                && bundles.iter().all(|&b| {
                    let vote_key = (b, pid);
                    vote_choices.get(&vote_key) == Some(choice)
                        && matches!(votes.get(&vote_key), Some(VotePhase::Confirmed))
                })
        }
        None => false,
    });
    let completed_vote_artifact =
        vote_choices
            .iter()
            .any(|(&(bundle_index, proposal_id), &stored_choice)| {
                !stale_vote_keys.contains(&(bundle_index, proposal_id))
                    && matches!(
                        intents.get(&proposal_id),
                        Some(Decision::Choice(intent_choice)) if *intent_choice == stored_choice
                    )
            })
            || share_phase_rows
                .iter()
                .any(|(bundle_index, proposal_id, _, _)| {
                    !stale_vote_keys.contains(&(*bundle_index, *proposal_id))
                        && matches!(intents.get(proposal_id), Some(Decision::Choice(_)))
                });
    let completed_for_display = completed_vote_artifact && !blocking_recovery;
    let voted_at = share_delegations
        .iter()
        .map(|share| share.created_at)
        .filter(|created_at| *created_at > 0)
        .max();
    let completed_vote_display = completed_for_display.then(|| {
        completed_vote_display(
            proposal_ids,
            &intents,
            &vote_choices,
            &stale_vote_keys,
            voted_at,
        )
    });
    // `SubmittedWithoutHash` and `SubmissionRejected` are terminal: they block
    // the foreground above but are not pending recovery work.
    let pending_recovery = submission_managed || !steps.is_empty();
    let needs_draft_setup = !blocking_recovery && !all_decided && !open_proposals.is_empty();
    let primary_action = select_primary_action(
        &steps,
        blocking_recovery,
        blocking_share_work,
        completed_for_display,
    );
    let recovered_delegation_work =
        recovered_delegation_work_from_steps(db, round_id, &delegation, &steps)?;
    let recovered_vote_work = recovered_vote_work_from_steps(
        db,
        round_id,
        &blocking_confirm_share_keys,
        &active_vote_batches,
        &steps,
    )?;

    let work_summary = summarize_plan_work(&steps, blocking_share_work);

    let has_unconfirmed_shares = share_delegations.iter().any(|share| !share.confirmed);

    let immediate_share_key =
        round_immediate_share_key(bundles.iter().copied().max(), &choice_proposals);
    let immediate_share_confirmed = immediate_share_key.as_ref().is_some_and(|key| {
        share_delegations.iter().any(|share| {
            share.bundle_index == key.bundle_index
                && share.proposal_id == key.proposal_id
                && share.share_index == key.share_index
                && share.confirmed
        })
    });

    Ok(RoundPlan {
        round_id: round_id.to_string(),
        pending_recovery,
        next_steps: steps,
        open_proposals,
        unrostered_intents,
        immediate_share_key,
        immediate_share_confirmed,
        all_decided,
        delegation_statuses,
        blocking_recovery,
        blocking_share_work,
        has_unconfirmed_shares,
        hotkey_bound,
        completed_vote_artifact,
        completed_for_display,
        completed_vote_display,
        needs_draft_setup,
        primary_action,
        needs_delegation_signing: work_summary.needs_delegation_signing,
        has_in_flight_delegation: work_summary.has_in_flight_delegation,
        needs_vote_polling: work_summary.needs_vote_polling,
        has_remaining_vote_or_share_work: work_summary.has_remaining_vote_or_share_work,
        has_recoverable_vote_or_share_work: work_summary.has_recoverable_vote_or_share_work,
        recovered_delegation_work,
        recovered_vote_work,
    })
}

/// Derived predicates describing what kind of work a plan still contains.
///
/// Every arm is listed explicitly. Adding a [`NextStep`] variant must be a
/// compile error here: a host that scans step kinds through an allowlist reads
/// an unrecognised kind as "no work", which silently strands a round, so the
/// classification has to live in one place the compiler checks.
struct PlanWorkSummary {
    needs_delegation_signing: bool,
    has_in_flight_delegation: bool,
    needs_vote_polling: bool,
    has_remaining_vote_or_share_work: bool,
    has_recoverable_vote_or_share_work: bool,
}

fn summarize_plan_work(steps: &[NextStep], blocking_share_work: bool) -> PlanWorkSummary {
    let mut summary = PlanWorkSummary {
        needs_delegation_signing: false,
        has_in_flight_delegation: false,
        needs_vote_polling: false,
        has_remaining_vote_or_share_work: false,
        has_recoverable_vote_or_share_work: false,
    };
    for step in steps {
        match step {
            NextStep::Delegate { .. } => summary.needs_delegation_signing = true,
            NextStep::AdvanceDelegation { .. } => {
                summary.needs_delegation_signing = true;
                summary.has_in_flight_delegation = true;
            }
            NextStep::AdvanceImportedDelegation { .. } => {
                summary.has_in_flight_delegation = true;
            }
            NextStep::CastVote { .. }
            | NextStep::AdvanceVote { .. }
            | NextStep::AdvanceVoteBatch { .. }
            | NextStep::SubmitShares { .. } => {
                summary.needs_vote_polling = true;
                summary.has_remaining_vote_or_share_work = true;
                summary.has_recoverable_vote_or_share_work = true;
            }
            NextStep::ConfirmShare { .. } => {
                summary.has_recoverable_vote_or_share_work = true;
                if blocking_share_work {
                    summary.has_remaining_vote_or_share_work = true;
                }
            }
        }
    }
    summary
}

fn vote_has_recovery_bundle(
    db: &VotingDb,
    round_id: &str,
    bundle_index: u32,
    proposal_id: u32,
) -> Result<bool, VotingError> {
    Ok(matches!(
        db.get_commitment_bundle_recovery_fields(round_id, bundle_index, proposal_id)?,
        Some((Some(_), _))
    ))
}

fn missing_share_indexes_for_confirmed_vote(
    db: &VotingDb,
    round_id: &str,
    bundle_index: u32,
    proposal_id: u32,
    recorded_share_indexes: BTreeSet<u32>,
) -> Result<Vec<u32>, VotingError> {
    let Some(recovery) = crate::vote::recovery_bundle(db, round_id, bundle_index, proposal_id)?
    else {
        return Err(VotingError::InvalidInput {
            message: format!(
                "confirmed vote for round {round_id} bundle {bundle_index} proposal {proposal_id} is missing recovery material for helper-share submission"
            ),
        });
    };
    let expected_share_indexes = crate::share::recover_payloads(&recovery)?
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

    Ok(expected_share_indexes
        .difference(&recorded_share_indexes)
        .copied()
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain_submission::{
        generation_for_vote, generation_for_vote_batch, submission_identity_key,
        ChainSubmissionIdentity, ChainSubmissionTarget,
    };
    use crate::round::RoundParams;
    use crate::types::{EncryptedShare, NoteInfo, MAX_PROPOSAL_ID};
    use crate::vote::{DraftVote, VoteBatchRecovery, VoteRecoveryBundle};

    const ROUND: &str = "0101010101010101010101010101010101010101010101010101010101010101";
    const W: &str = "wallet";

    fn round_params() -> RoundParams {
        RoundParams {
            vote_round_id: ROUND.to_string(),
            snapshot_height: 1000,
            ea_pk: vec![0xEA; 32],
            nc_root: vec![0xAA; 32],
            nullifier_imt_root: vec![0xBB; 32],
        }
    }

    fn note(position: u64) -> NoteInfo {
        NoteInfo {
            commitment: vec![0x01; 32],
            nullifier: vec![position as u8 + 0x02; 32],
            value: crate::governance::BALLOT_DIVISOR,
            position,
            diversifier: vec![0x03; 11],
            rho: vec![0x04; 32],
            rseed: vec![0x05; 32],
            scope: 0,
            ufvk_str: "uview1test".to_string(),
        }
    }

    fn db_with_bundle() -> VotingDb {
        let db = VotingDb::open_in_memory().unwrap();
        db.set_wallet_id(W);
        db.create_round(crate::Network::Testnet, &round_params(), None)
            .unwrap();
        db.ensure_bundles(ROUND, &[note(0)]).unwrap();
        db
    }

    fn record_rejected_submission_fixture(
        db: &VotingDb,
        target: ChainSubmissionTarget,
        generation_digest: [u8; 32],
    ) {
        let identity = submission_identity_fixture(target);
        let (kind, proposal_id, ordered_batch_digest) = match target {
            ChainSubmissionTarget::Delegation => ("delegation", None, None),
            ChainSubmissionTarget::Vote { proposal_id } => {
                ("vote", Some(i64::from(proposal_id)), None)
            }
            ChainSubmissionTarget::VoteBatch {
                ordered_batch_digest,
            } => ("vote_batch", None, Some(ordered_batch_digest.to_vec())),
        };
        db.conn()
            .execute(
                "INSERT INTO chain_submissions
                 (identity_key, round_id, wallet_id, network,
                  bundle_index, kind, proposal_id, ordered_batch_digest,
                  generation_digest, state, committed_post_reservations,
                  diagnostic_kind, diagnostic, created_at, updated_at)
                 VALUES (?1, ?2, ?3, 'testnet', 0, ?4, ?5, ?6, ?7,
                         'rejected', 1, 'chain_rejected',
                         'vote chain definitely rejected the generation', 9, 9)",
                rusqlite::params![
                    submission_identity_key(&identity),
                    ROUND,
                    W,
                    kind,
                    proposal_id,
                    ordered_batch_digest,
                    generation_digest.to_vec(),
                ],
            )
            .unwrap();
    }

    fn submission_identity_fixture(target: ChainSubmissionTarget) -> ChainSubmissionIdentity {
        ChainSubmissionIdentity::new(W, crate::Network::Testnet, [1; 32], 0, target).unwrap()
    }

    fn align_stored_commitments_with_recovery(db: &VotingDb, proposal_ids: &[u32]) {
        for &proposal_id in proposal_ids {
            let recovery = crate::vote::recovery_bundle(db, ROUND, 0, proposal_id)
                .unwrap()
                .unwrap();
            let commitment = crate::vote::stored_vote_commitment_bytes(&recovery).unwrap();
            let rows = db
                .conn()
                .execute(
                    "UPDATE votes SET commitment = ?1
                     WHERE round_id = ?2 AND wallet_id = ?3
                       AND bundle_index = 0 AND proposal_id = ?4",
                    rusqlite::params![commitment, ROUND, W, i64::from(proposal_id)],
                )
                .unwrap();
            assert_eq!(rows, 1);
        }
    }

    fn store_vote_recovery_fixture(
        db: &VotingDb,
        bundle_index: u32,
        proposal_id: u32,
        choice: u32,
        vc_tree_position: Option<u64>,
    ) {
        let recovery = recovery_bundle_fixture(
            bundle_index,
            proposal_id,
            choice,
            vc_tree_position.unwrap_or(0),
        );
        store_recovery_bundle_fixture(db, &recovery, vc_tree_position);
    }

    fn store_recovery_bundle_fixture(
        db: &VotingDb,
        recovery: &VoteRecoveryBundle,
        vc_tree_position: Option<u64>,
    ) {
        let conn = db.conn();
        let rows = conn
            .execute(
                "UPDATE votes SET commitment_bundle_json = :json, vc_tree_position = :pos
                 WHERE round_id = :round_id
                   AND wallet_id = :wallet_id
                   AND bundle_index = :bundle_index
                   AND proposal_id = :proposal_id",
                named_params! {
                    ":json": crate::vote::serialize_recovery(&recovery).unwrap(),
                    ":pos": vc_tree_position.map(|position| position as i64),
                    ":round_id": ROUND,
                    ":wallet_id": W,
                    ":bundle_index": recovery.bundle_index as i64,
                    ":proposal_id": recovery.proposal_id as i64,
                },
            )
            .unwrap();
        assert_eq!(rows, 1);
    }

    fn confirm_vote_fixture(db: &VotingDb, bundle_index: u32, proposal_id: u32, choice: u32) {
        crate::storage::queries::store_vote(
            &db.conn(),
            ROUND,
            W,
            bundle_index,
            proposal_id,
            choice,
            &[0xCC; 16],
        )
        .unwrap();
        store_vote_recovery_fixture(db, bundle_index, proposal_id, choice, Some(42));
        db.record_vote_submission(ROUND, bundle_index, proposal_id, "tx")
            .unwrap();
    }

    fn recovery_bundle_fixture(
        bundle_index: u32,
        proposal_id: u32,
        choice: u32,
        vc_tree_position: u64,
    ) -> VoteRecoveryBundle {
        VoteRecoveryBundle {
            vote_round_id: ROUND.to_string(),
            bundle_index,
            proposal_id,
            vote_decision: choice,
            anchor_height: 123,
            vc_tree_position,
            single_share: false,
            num_options: 3,
            van_nullifier: [0x10; 32],
            vote_authority_note_new: [0x11; 32],
            vote_commitment: [0x12; 32],
            proof: vec![0x13; 96],
            shares_hash: [0x14; 32],
            r_vpk: [0x15; 32],
            alpha_v: [0x16; 32],
            vote_auth_sig: [0x17; 64],
            encrypted_shares: vec![
                EncryptedShare {
                    c1: vec![0x21; 32],
                    c2: vec![0x22; 32],
                    share_index: 0,
                    plaintext_value: 5,
                    randomness: vec![0x23; 32],
                },
                EncryptedShare {
                    c1: vec![0x31; 32],
                    c2: vec![0x32; 32],
                    share_index: 1,
                    plaintext_value: 6,
                    randomness: vec![0x33; 32],
                },
            ],
            share_blinds: vec![[0x41; 32], [0x42; 32]],
            share_comms: vec![[0x51; 32], [0x52; 32]],
            batch: None,
        }
    }

    fn store_two_action_batch_recovery_fixture(db: &VotingDb) -> [u8; 32] {
        store_two_action_batch_recovery_fixture_for(db, (1, 0), (2, 1))
    }

    fn store_two_action_batch_recovery_fixture_for(
        db: &VotingDb,
        first_vote: (u32, u32),
        second_vote: (u32, u32),
    ) -> [u8; 32] {
        let mut first = recovery_bundle_fixture(0, first_vote.0, first_vote.1, 0);
        first.vote_commitment = [0x61; 32];
        let mut second = recovery_bundle_fixture(0, second_vote.0, second_vote.1, 0);
        second.van_nullifier = [0x20; 32];
        second.vote_authority_note_new = [0x21; 32];
        second.vote_commitment = [0x62; 32];
        second.r_vpk = [0x25; 32];
        let actions = [&first, &second]
            .into_iter()
            .map(
                |recovery| crate::vote_commitment::CastVoteBatchSighashAction {
                    r_vpk: &recovery.r_vpk,
                    van_nullifier: &recovery.van_nullifier,
                    vote_authority_note_new: &recovery.vote_authority_note_new,
                    vote_commitment: &recovery.vote_commitment,
                    proposal_id: recovery.proposal_id,
                },
            )
            .collect::<Vec<_>>();
        let digest = crate::vote_commitment::cast_vote_batch_sighash(
            ROUND,
            first.anchor_height as u64,
            &actions,
        )
        .unwrap();
        first.batch = Some(VoteBatchRecovery {
            digest,
            index: 0,
            size: 2,
        });
        second.batch = Some(VoteBatchRecovery {
            digest,
            index: 1,
            size: 2,
        });

        for recovery in [&first, &second] {
            crate::storage::queries::store_vote(
                &db.conn(),
                ROUND,
                W,
                recovery.bundle_index,
                recovery.proposal_id,
                recovery.vote_decision,
                &recovery.vote_commitment,
            )
            .unwrap();
            store_recovery_bundle_fixture(db, recovery, None);
        }

        digest
    }

    fn record_confirmed_share_fixture(
        db: &VotingDb,
        bundle_index: u32,
        proposal_id: u32,
        share_index: u32,
    ) {
        let nullifier = vec![share_index as u8; 32];
        db.record_share_delegation(
            ROUND,
            bundle_index,
            proposal_id,
            share_index,
            &["https://helper.example".to_string()],
            &nullifier,
            0,
        )
        .unwrap();
        db.mark_share_confirmed(ROUND, bundle_index, proposal_id, share_index)
            .unwrap();
    }

    fn record_submitted_share_fixture(
        db: &VotingDb,
        bundle_index: u32,
        proposal_id: u32,
        share_index: u32,
        sent_to_urls: &[String],
    ) {
        let nullifier = vec![share_index as u8; 32];
        db.record_share_delegation(
            ROUND,
            bundle_index,
            proposal_id,
            share_index,
            sent_to_urls,
            &nullifier,
            0,
        )
        .unwrap();
    }

    fn record_all_confirmed_share_fixtures(db: &VotingDb, bundle_index: u32, proposal_id: u32) {
        record_confirmed_share_fixture(db, bundle_index, proposal_id, 0);
        record_confirmed_share_fixture(db, bundle_index, proposal_id, 1);
    }

    #[test]
    fn ballot_intent_round_trip() {
        let db = db_with_bundle();
        db.set_ballot_intent(ROUND, 1, Decision::Choice(0), 3)
            .unwrap();
        db.set_ballot_intent(ROUND, 2, Decision::Skipped, 3)
            .unwrap();
        db.set_ballot_intent(ROUND, 1, Decision::Choice(3), 4)
            .unwrap(); // upsert

        let intents = db.ballot_intents(ROUND).unwrap();
        assert_eq!(
            intents,
            vec![(1, Decision::Choice(3)), (2, Decision::Skipped)]
        );
    }

    #[test]
    fn ballot_intent_rejects_invalid_proposal_id() {
        let db = db_with_bundle();

        assert!(db
            .set_ballot_intent(ROUND, 0, Decision::Choice(0), 3)
            .is_err());
        assert!(db
            .set_ballot_intent(ROUND, MAX_PROPOSAL_ID + 1, Decision::Skipped, 3)
            .is_err());
    }

    #[test]
    fn ballot_intent_rejects_choice_outside_proposal_option_count() {
        let db = db_with_bundle();

        db.set_ballot_intent(ROUND, 1, Decision::Choice(1), 2)
            .unwrap();
        assert!(db
            .set_ballot_intent(ROUND, 1, Decision::Choice(2), 2)
            .is_err());
        assert!(db
            .set_ballot_intent(ROUND, 1, Decision::Choice(0), 1)
            .is_err());
    }

    #[test]
    fn ballot_intent_for_draft_vote_validates_choice_and_options() {
        let db = db_with_bundle();
        let draft = DraftVote {
            proposal_id: 1,
            choice: 1,
            num_options: 2,
            single_share: false,
            vc_tree_position: 0,
        };

        db.set_ballot_intent_for_draft_vote(ROUND, &draft).unwrap();
        assert_eq!(
            db.ballot_intents(ROUND).unwrap(),
            vec![(1, Decision::Choice(1))]
        );

        assert!(db
            .set_ballot_intent_for_draft_vote(
                ROUND,
                &DraftVote {
                    choice: 2,
                    ..draft.clone()
                },
            )
            .is_err());
        assert!(db
            .set_ballot_intent_for_draft_vote(
                ROUND,
                &DraftVote {
                    num_options: 9,
                    ..draft
                },
            )
            .is_err());
    }

    #[test]
    fn resume_plan_rejects_invalid_proposal_ids() {
        let db = db_with_bundle();

        assert!(resume_plan(&db, ROUND, &[1, 0]).is_err());
        assert!(resume_plan(&db, ROUND, &[1, MAX_PROPOSAL_ID + 1]).is_err());
        assert!(resume_plan(&db, ROUND, &[1, 1]).is_err());
    }

    #[test]
    fn fresh_round_with_no_choices_has_no_recovery() {
        let db = db_with_bundle();
        let plan = resume_plan(&db, ROUND, &[1, 2, 3]).unwrap();
        assert!(!plan.pending_recovery);
        assert!(plan.next_steps.is_empty());
        assert_eq!(plan.open_proposals, vec![1, 2, 3]);
        assert!(!plan.all_decided);
        assert_eq!(
            plan.delegation_statuses,
            vec![DelegationStatus {
                bundle_index: 0,
                phase: DelegationPhase::Prepared,
                tx_hash: None,
                submission_diagnostic: None,
                terminal: false,
            }]
        );
        assert!(!plan.blocking_recovery);
        assert!(!plan.hotkey_bound);
        assert!(!plan.completed_vote_artifact);
        assert!(!plan.completed_for_display);
        assert_eq!(plan.completed_vote_display, None);
        assert!(plan.needs_draft_setup);
        assert_eq!(plan.primary_action, RoundPlanAction::Idle);
        assert!(plan.recovered_delegation_work.is_empty());
        assert!(plan.recovered_vote_work.is_empty());
    }

    #[test]
    fn a_bundles_delegation_step_blocks_its_own_vote_and_share_steps_only() {
        let delegate = NextStep::Delegate { bundle_index: 0 };
        let advance = NextStep::AdvanceDelegation { bundle_index: 1 };
        let cast_zero = NextStep::CastVote {
            bundle_index: 0,
            proposal_id: 1,
            choice: 1,
        };
        let cast_one = NextStep::CastVote {
            bundle_index: 1,
            proposal_id: 1,
            choice: 1,
        };
        let cast_two = NextStep::CastVote {
            bundle_index: 2,
            proposal_id: 1,
            choice: 1,
        };
        let share_one = NextStep::ConfirmShare {
            bundle_index: 1,
            proposal_id: 1,
            share_index: 0,
        };
        let steps = vec![
            delegate.clone(),
            advance.clone(),
            cast_zero.clone(),
            cast_one.clone(),
            cast_two.clone(),
            share_one.clone(),
        ];

        assert_eq!(blocking_prerequisite(&steps, &cast_zero), Some(&delegate));
        assert_eq!(blocking_prerequisite(&steps, &cast_one), Some(&advance));
        assert_eq!(blocking_prerequisite(&steps, &share_one), Some(&advance));
        assert_eq!(blocking_prerequisite(&steps, &cast_two), None);
        assert_eq!(blocking_prerequisite(&steps, &delegate), None);
        assert_eq!(blocking_prerequisite(&steps, &advance), None);
    }

    #[test]
    fn a_durable_intent_outside_the_roster_withholds_cast_until_cleared() {
        let db = db_with_bundle();
        for proposal_id in [1, 2, 3] {
            db.set_ballot_intent(ROUND, proposal_id, Decision::Choice(1), 3)
                .unwrap();
        }
        // A decision recorded for a proposal the roster no longer lists.
        db.set_ballot_intent(ROUND, 9, Decision::Choice(0), 3).unwrap();

        let plan = resume_plan(&db, ROUND, &[1, 2, 3]).unwrap();
        assert_eq!(plan.unrostered_intents, vec![9]);
        assert!(plan.open_proposals.is_empty());
        assert!(
            !plan
                .next_steps
                .iter()
                .any(|step| matches!(step, NextStep::CastVote { .. })),
            "casting must wait until the durable intents exactly match the roster: {:?}",
            plan.next_steps
        );
        assert_eq!(plan.next_steps, vec![NextStep::Delegate { bundle_index: 0 }]);

        db.clear_ballot_intent(ROUND, 9).unwrap();
        db.clear_ballot_intent(ROUND, 9).unwrap();
        let plan = resume_plan(&db, ROUND, &[1, 2, 3]).unwrap();
        assert!(plan.unrostered_intents.is_empty());
        assert!(plan
            .next_steps
            .iter()
            .any(|step| matches!(step, NextStep::CastVote { .. })));
    }

    #[test]
    fn answered_but_uncast_proposal_yields_cast_then_delegate_prereq() {
        let db = db_with_bundle();
        for proposal_id in [1, 2, 3] {
            db.set_ballot_intent(ROUND, proposal_id, Decision::Choice(1), 3)
                .unwrap();
        }
        let plan = resume_plan(&db, ROUND, &[1, 2, 3]).unwrap();
        assert!(plan.pending_recovery);
        // Bundle 0 is only Prepared, and every proposal needs casting on it, so
        // the delegate prerequisite is emitted, ordered before the casts.
        assert_eq!(
            plan.next_steps,
            vec![
                NextStep::Delegate { bundle_index: 0 },
                NextStep::CastVote {
                    bundle_index: 0,
                    proposal_id: 1,
                    choice: 1,
                },
                NextStep::CastVote {
                    bundle_index: 0,
                    proposal_id: 2,
                    choice: 1,
                },
                NextStep::CastVote {
                    bundle_index: 0,
                    proposal_id: 3,
                    choice: 1,
                },
            ]
        );
        assert!(plan.open_proposals.is_empty());
        assert_eq!(
            plan.recovered_delegation_work,
            vec![DelegationRecoveryWork {
                kind: DelegationRecoveryWorkKind::Delegate,
                bundle_index: 0,
                phase: DelegationPhase::Prepared,
                tx_hash: None,
            }]
        );
    }

    #[test]
    fn an_open_proposal_defers_casting_but_still_plans_the_delegate_prereq() {
        let db = db_with_bundle();
        db.set_ballot_intent(ROUND, 2, Decision::Choice(1), 3)
            .unwrap();

        // Proposals 1 and 3 are undecided, so helper-share planning could not
        // derive the round's immediate share yet. The cast is withheld, but
        // delegation is a prerequisite either way and is still planned.
        let plan = resume_plan(&db, ROUND, &[1, 2, 3]).unwrap();
        assert_eq!(
            plan.next_steps,
            vec![NextStep::Delegate { bundle_index: 0 }]
        );
        // `open_proposals` is what surfaces the remaining decisions here;
        // `needs_draft_setup` is suppressed by the pending `Delegate` step.
        assert_eq!(plan.open_proposals, vec![1, 3]);
        assert_eq!(
            plan.recovered_delegation_work,
            vec![DelegationRecoveryWork {
                kind: DelegationRecoveryWorkKind::Delegate,
                bundle_index: 0,
                phase: DelegationPhase::Prepared,
                tx_hash: None,
            }]
        );

        // A skip is terminal, so completing the roster releases the cast.
        db.set_ballot_intent(ROUND, 1, Decision::Skipped, 3)
            .unwrap();
        db.set_ballot_intent(ROUND, 3, Decision::Skipped, 3)
            .unwrap();
        let plan = resume_plan(&db, ROUND, &[1, 2, 3]).unwrap();
        assert_eq!(
            plan.next_steps,
            vec![
                NextStep::Delegate { bundle_index: 0 },
                NextStep::CastVote {
                    bundle_index: 0,
                    proposal_id: 2,
                    choice: 1,
                },
            ]
        );
        assert!(plan.open_proposals.is_empty());
    }

    #[test]
    fn submitted_legacy_vote_without_a_lifecycle_row_yields_an_advance_step() {
        let db = db_with_bundle();
        db.set_ballot_intent(ROUND, 2, Decision::Choice(1), 3)
            .unwrap();
        db.store_delegation_tx_hash(ROUND, 0, "dtx").unwrap();
        db.store_van_position(ROUND, 0, 7).unwrap();
        crate::storage::queries::store_vote(&db.conn(), ROUND, W, 0, 2, 1, &[0xCC; 16]).unwrap();
        store_vote_recovery_fixture(&db, 0, 2, 1, None);
        db.record_vote_submission(ROUND, 0, 2, "vtx").unwrap();
        let plan = resume_plan(&db, ROUND, &[1, 2, 3]).unwrap();
        assert_eq!(
            plan.next_steps,
            vec![NextStep::AdvanceVote {
                bundle_index: 0,
                proposal_id: 2
            }]
        );
        assert_eq!(
            plan.recovered_vote_work,
            vec![VoteRecoveryWork {
                kind: VoteRecoveryWorkKind::AdvanceVote,
                bundle_index: 0,
                proposal_id: 2,
                tx_hash: Some("vtx".to_string()),
                vc_tree_position: None,
                share_indexes: Vec::new(),
            }]
        );
    }

    #[test]
    fn submitted_vote_without_recovery_bundle_is_invalid() {
        let db = db_with_bundle();
        db.set_ballot_intent(ROUND, 2, Decision::Choice(1), 3)
            .unwrap();
        db.store_delegation_tx_hash(ROUND, 0, "dtx").unwrap();
        db.store_van_position(ROUND, 0, 7).unwrap();
        crate::storage::queries::store_vote(&db.conn(), ROUND, W, 0, 2, 1, &[0xCC; 16]).unwrap();
        db.record_vote_submission(ROUND, 0, 2, "vtx").unwrap();

        let err = resume_plan(&db, ROUND, &[1, 2, 3]).unwrap_err();

        assert!(
            err.to_string()
                .contains("submitted vote without recovery material"),
            "{err}"
        );
    }

    #[test]
    fn blocking_share_retry_waits_for_submitted_vote_confirmation() {
        let db = db_with_bundle();
        db.set_ballot_intent(ROUND, 2, Decision::Choice(1), 3)
            .unwrap();
        db.store_delegation_tx_hash(ROUND, 0, "dtx").unwrap();
        db.store_van_position(ROUND, 0, 7).unwrap();
        crate::storage::queries::store_vote(&db.conn(), ROUND, W, 0, 2, 1, &[0xCC; 16]).unwrap();
        store_vote_recovery_fixture(&db, 0, 2, 1, None);
        db.record_vote_submission(ROUND, 0, 2, "vtx").unwrap();
        record_submitted_share_fixture(&db, 0, 2, 0, &[]);

        let plan = resume_plan(&db, ROUND, &[1, 2, 3]).unwrap();

        assert_eq!(
            plan.next_steps,
            vec![
                NextStep::AdvanceVote {
                    bundle_index: 0,
                    proposal_id: 2,
                },
                NextStep::ConfirmShare {
                    bundle_index: 0,
                    proposal_id: 2,
                    share_index: 0,
                },
            ]
        );
        assert!(plan.blocking_recovery);
        assert!(plan.blocking_share_work);
        assert_eq!(plan.primary_action, RoundPlanAction::Vote);
        assert_eq!(
            plan.recovered_vote_work,
            vec![VoteRecoveryWork {
                kind: VoteRecoveryWorkKind::AdvanceVote,
                bundle_index: 0,
                proposal_id: 2,
                tx_hash: Some("vtx".to_string()),
                vc_tree_position: None,
                share_indexes: Vec::new(),
            }]
        );
    }

    #[test]
    fn non_anchor_share_retry_waits_for_submitted_batch_confirmation() {
        let db = db_with_bundle();
        db.set_ballot_intent(ROUND, 1, Decision::Choice(0), 3)
            .unwrap();
        db.set_ballot_intent(ROUND, 2, Decision::Choice(1), 3)
            .unwrap();
        db.store_delegation_tx_hash(ROUND, 0, "dtx").unwrap();
        db.store_van_position(ROUND, 0, 7).unwrap();
        let digest = store_two_action_batch_recovery_fixture(&db);
        crate::vote::record_batch_submission(&db, ROUND, 0, &digest, "batch-tx").unwrap();
        record_submitted_share_fixture(&db, 0, 2, 0, &[]);

        let plan = resume_plan(&db, ROUND, &[1, 2, 3]).unwrap();

        assert_eq!(
            plan.next_steps,
            vec![
                NextStep::AdvanceVoteBatch {
                    bundle_index: 0,
                    proposal_id: 1,
                },
                NextStep::ConfirmShare {
                    bundle_index: 0,
                    proposal_id: 2,
                    share_index: 0,
                },
            ]
        );
        assert!(plan.blocking_recovery);
        assert!(plan.blocking_share_work);
        assert_eq!(plan.primary_action, RoundPlanAction::Vote);
        assert_eq!(
            plan.recovered_vote_work,
            vec![VoteRecoveryWork {
                kind: VoteRecoveryWorkKind::AdvanceVoteBatch,
                bundle_index: 0,
                proposal_id: 1,
                tx_hash: Some("batch-tx".to_string()),
                vc_tree_position: None,
                share_indexes: Vec::new(),
            }]
        );
    }

    /// Inserts one in-flight `chain_submissions` row for `kind`/`proposal_id`.
    ///
    /// `candidate` is the durable candidate transaction hash the lifecycle
    /// recorded before confirmation; `Submitting` has none because intent is
    /// reserved before the POST. The version-17 projection columns stay null,
    /// which is exactly the state that used to read as "unsubmitted".
    fn insert_in_flight_submission(
        db: &VotingDb,
        state: &str,
        kind: &str,
        proposal_id: Option<u32>,
        candidate: Option<[u8; 32]>,
    ) {
        let candidate = candidate.map(|hash| hash.to_vec());
        let tracking_started_at = candidate.as_ref().map(|_| 10_i64);
        db.conn()
            .execute(
                "INSERT INTO chain_submissions
                 (identity_key, round_id, wallet_id, network, bundle_index,
                  kind, proposal_id, generation_digest, state, candidate_transaction_hash,
                  committed_post_reservations, tracking_started_at,
                  diagnostic_kind, diagnostic, created_at, updated_at)
                 VALUES (?1, ?2, ?3, 'testnet', 0, ?4, ?5, ?6,
                         ?7, ?8, 1, ?9,
                         CASE WHEN ?7 IN ('recovering','submitted_without_hash')
                              THEN 'ambiguous_attempts_exhausted' END,
                         CASE WHEN ?7 IN ('recovering','submitted_without_hash')
                              THEN 'submission has no usable hash' END,
                         9, 10)",
                rusqlite::params![
                    vec![0x51_u8; 32],
                    ROUND,
                    W,
                    kind,
                    proposal_id.map(|id| id as i64),
                    vec![0x52_u8; 32],
                    state,
                    candidate,
                    tracking_started_at
                ],
            )
            .unwrap();
    }

    #[test]
    fn a_second_network_generation_makes_the_reported_hash_ambiguous() {
        let db = db_with_bundle();
        db.set_ballot_intent(ROUND, 2, Decision::Choice(1), 3)
            .unwrap();
        crate::storage::queries::store_vote(&db.conn(), ROUND, W, 0, 2, 1, &[0xCC; 16]).unwrap();
        store_vote_recovery_fixture(&db, 0, 2, 1, None);

        let insert = |network: &str, identity: u8, digest: u8, candidate: u8| {
            db.conn()
                .execute(
                    "INSERT INTO chain_submissions
                     (identity_key, round_id, wallet_id, network, bundle_index,
                      kind, proposal_id, generation_digest, state, candidate_transaction_hash,
                      committed_post_reservations, tracking_started_at, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, 0, 'vote', 2, ?5,
                             'tracking', ?6, 1, 10, 9, 10)",
                    rusqlite::params![
                        vec![identity; 32],
                        ROUND,
                        W,
                        network,
                        vec![digest; 32],
                        vec![candidate; 32]
                    ],
                )
                .unwrap();
        };

        insert("testnet", 0x81, 0x82, 0x83);
        assert_eq!(
            crate::chain_submission::planning::vote_transaction_hash(&db, ROUND, 0, 2).unwrap(),
            Some(hex::encode([0x83_u8; 32])),
            "one generation reports its own hash"
        );

        // The same vote on a second configured network. Submission identity
        // includes the network, so both rows are legal; a planner carries no
        // network context, so attributing either hash would be a guess.
        insert("regtest", 0x91, 0x92, 0x93);
        assert_eq!(
            crate::chain_submission::planning::vote_transaction_hash(&db, ROUND, 0, 2).unwrap(),
            None,
            "an ambiguous hash must not be attributed to this vote"
        );
    }

    #[test]
    fn plan_work_summary_classifies_every_step_kind() {
        // Locally prepared delegations need the signing key on both their
        // first and later advancement passes.
        let signing = summarize_plan_work(&[NextStep::Delegate { bundle_index: 0 }], false);
        assert!(signing.needs_delegation_signing);
        assert!(!signing.has_in_flight_delegation);
        assert!(!signing.needs_vote_polling);

        let in_flight =
            summarize_plan_work(&[NextStep::AdvanceDelegation { bundle_index: 0 }], false);
        assert!(in_flight.needs_delegation_signing);
        assert!(in_flight.has_in_flight_delegation);

        let imported = summarize_plan_work(
            &[NextStep::AdvanceImportedDelegation { bundle_index: 0 }],
            false,
        );
        assert!(!imported.needs_delegation_signing);
        assert!(imported.has_in_flight_delegation);

        for step in [
            NextStep::CastVote {
                bundle_index: 0,
                proposal_id: 1,
                choice: 0,
            },
            NextStep::AdvanceVote {
                bundle_index: 0,
                proposal_id: 1,
            },
            NextStep::AdvanceVoteBatch {
                bundle_index: 0,
                proposal_id: 1,
            },
            NextStep::SubmitShares {
                bundle_index: 0,
                proposal_id: 1,
                share_index: 0,
            },
        ] {
            let kind = step.kind_view();
            let summary = summarize_plan_work(&[step], false);
            assert!(summary.needs_vote_polling, "{kind:?}");
            assert!(summary.has_remaining_vote_or_share_work, "{kind:?}");
            assert!(summary.has_recoverable_vote_or_share_work, "{kind:?}");
            assert!(!summary.needs_delegation_signing, "{kind:?}");
        }

        // Share confirmation is always recoverable work, but only counts as
        // remaining work while it is blocking; otherwise background polling
        // finishes it without holding the foreground flow open.
        let confirm = [NextStep::ConfirmShare {
            bundle_index: 0,
            proposal_id: 1,
            share_index: 0,
        }];
        let non_blocking = summarize_plan_work(&confirm, false);
        assert!(non_blocking.has_recoverable_vote_or_share_work);
        assert!(!non_blocking.has_remaining_vote_or_share_work);
        assert!(!non_blocking.needs_vote_polling);

        let blocking = summarize_plan_work(&confirm, true);
        assert!(blocking.has_remaining_vote_or_share_work);
        assert!(!blocking.needs_vote_polling);

        // An empty plan claims no work of any kind.
        let empty = summarize_plan_work(&[], true);
        assert!(!empty.needs_delegation_signing);
        assert!(!empty.has_in_flight_delegation);
        assert!(!empty.needs_vote_polling);
        assert!(!empty.has_remaining_vote_or_share_work);
        assert!(!empty.has_recoverable_vote_or_share_work);
    }

    #[test]
    fn every_managed_submission_state_yields_a_typed_advance_step() {
        for (state, candidate) in [
            ("submitting", None),
            ("tracking", Some([0x61; 32])),
            ("recovering", Some([0x62; 32])),
        ] {
            let delegation_db = db_with_bundle();
            delegation_db
                .set_ballot_intent(ROUND, 2, Decision::Choice(1), 3)
                .unwrap();
            insert_in_flight_submission(&delegation_db, state, "delegation", None, candidate);
            assert!(resume_plan(&delegation_db, ROUND, &[1, 2, 3])
                .unwrap()
                .next_steps
                .contains(&NextStep::AdvanceDelegation { bundle_index: 0 }));

            let vote_db = db_with_bundle();
            vote_db
                .set_ballot_intent(ROUND, 2, Decision::Choice(1), 3)
                .unwrap();
            vote_db.store_delegation_tx_hash(ROUND, 0, "dtx").unwrap();
            vote_db.store_van_position(ROUND, 0, 7).unwrap();
            crate::storage::queries::store_vote(&vote_db.conn(), ROUND, W, 0, 2, 1, &[0xCC; 16])
                .unwrap();
            store_vote_recovery_fixture(&vote_db, 0, 2, 1, None);
            insert_in_flight_submission(&vote_db, state, "vote", Some(2), candidate);
            assert!(resume_plan(&vote_db, ROUND, &[1, 2, 3])
                .unwrap()
                .next_steps
                .contains(&NextStep::AdvanceVote {
                    bundle_index: 0,
                    proposal_id: 2,
                }));
        }
    }

    #[test]
    fn submitted_without_hash_schedules_no_chain_recovery_step() {
        let db = db_with_bundle();
        db.set_ballot_intent(ROUND, 2, Decision::Choice(1), 3)
            .unwrap();
        db.store_delegation_tx_hash(ROUND, 0, "dtx").unwrap();
        db.store_van_position(ROUND, 0, 7).unwrap();
        crate::storage::queries::store_vote(&db.conn(), ROUND, W, 0, 2, 1, &[0xCC; 16]).unwrap();
        store_vote_recovery_fixture(&db, 0, 2, 1, None);
        insert_in_flight_submission(&db, "submitted_without_hash", "vote", Some(2), None);

        let plan = resume_plan(&db, ROUND, &[1, 2, 3]).unwrap();

        assert_eq!(
            db.vote_phase(ROUND, 0, 2).unwrap(),
            VotePhase::SubmittedWithoutHash
        );
        assert!(!plan.next_steps.iter().any(|step| matches!(
            step,
            NextStep::AdvanceVote { .. } | NextStep::AdvanceVoteBatch { .. }
        )));
        assert!(plan.blocking_recovery);
        assert!(
            !plan.pending_recovery,
            "terminal hashless dispatch is not pending recovery work"
        );
    }

    #[test]
    fn submitted_without_hash_delegation_blocks_without_pending_recovery() {
        let db = db_with_bundle();
        insert_in_flight_submission(&db, "submitted_without_hash", "delegation", None, None);

        let plan = resume_plan(&db, ROUND, &[1, 2, 3]).unwrap();

        assert_eq!(
            db.delegation_phase(ROUND, 0).unwrap(),
            DelegationPhase::SubmittedWithoutHash
        );
        assert!(plan.next_steps.is_empty(), "{:?}", plan.next_steps);
        assert!(plan.blocking_recovery);
        assert!(!plan.pending_recovery);
        // No later lifecycle call is scheduled, so the plan itself must carry
        // the stored diagnostic a wallet shows for manual handling.
        let status = &plan.delegation_statuses[0];
        assert_eq!(status.phase, DelegationPhase::SubmittedWithoutHash);
        let diagnostic = status.submission_diagnostic.as_ref().unwrap();
        assert_eq!(
            diagnostic.kind(),
            crate::chain_submission::ChainSubmissionDiagnosticKind::AmbiguousAttemptsExhausted
        );
        assert_eq!(diagnostic.message(), "submission has no usable hash");
        // The wallet-facing phase cannot distinguish this from a healthy
        // submission, so the status has to say so outright.
        assert!(status.terminal);
        assert_eq!(
            crate::wire::WorkflowPhaseView::from(
                crate::phases::WorkflowPhase::for_delegation(status.phase)
            ),
            crate::wire::WorkflowPhaseView::SubmittedDelegation
        );
    }

    #[test]
    fn terminal_delegation_status_marks_only_ended_phases() {
        let db = db_with_bundle();
        assert!(
            !resume_plan(&db, ROUND, &[1, 2, 3]).unwrap().delegation_statuses[0].terminal,
            "a fresh bundle still has delegation work"
        );

        insert_in_flight_submission(&db, "rejected", "delegation", None, None);
        let plan = resume_plan(&db, ROUND, &[1, 2, 3]).unwrap();
        let status = &plan.delegation_statuses[0];
        assert_eq!(status.phase, DelegationPhase::SubmissionRejected);
        assert!(status.terminal);
        assert!(
            plan.next_steps.is_empty(),
            "a terminal delegation schedules no work: {:?}",
            plan.next_steps
        );
    }

    #[test]
    fn in_flight_delegation_status_is_not_terminal() {
        let db = db_with_bundle();
        // Tracking carries a candidate hash by construction.
        insert_in_flight_submission(&db, "tracking", "delegation", None, Some([0x77; 32]));

        let plan = resume_plan(&db, ROUND, &[1, 2, 3]).unwrap();
        let status = &plan.delegation_statuses[0];

        assert_eq!(status.phase, DelegationPhase::SubmissionManaged);
        assert!(
            !status.terminal,
            "a submission the lifecycle still owns is not terminal"
        );
    }

    #[test]
    fn committed_vote_yields_submit_not_rebuild() {
        let db = db_with_bundle();
        db.set_ballot_intent(ROUND, 2, Decision::Choice(1), 3)
            .unwrap();
        db.store_delegation_tx_hash(ROUND, 0, "dtx").unwrap();
        db.store_van_position(ROUND, 0, 7).unwrap();
        crate::storage::queries::store_vote(&db.conn(), ROUND, W, 0, 2, 1, &[0xCC; 16]).unwrap();
        store_vote_recovery_fixture(&db, 0, 2, 1, None);

        let plan = resume_plan(&db, ROUND, &[1, 2, 3]).unwrap();

        assert_eq!(
            plan.next_steps,
            vec![NextStep::AdvanceVote {
                bundle_index: 0,
                proposal_id: 2
            }]
        );
        assert!(plan.blocking_recovery);
        assert_eq!(plan.primary_action, RoundPlanAction::Vote);
        assert_eq!(
            plan.recovered_vote_work,
            vec![VoteRecoveryWork {
                kind: VoteRecoveryWorkKind::AdvanceVote,
                bundle_index: 0,
                proposal_id: 2,
                tx_hash: None,
                vc_tree_position: None,
                share_indexes: Vec::new(),
            }]
        );
    }

    #[test]
    fn lifecycle_owned_delegation_and_vote_yield_typed_advance_steps() {
        for tx_hash in [None, Some("legacy-vtx")] {
            let db = db_with_bundle();
            db.set_ballot_intent(ROUND, 2, Decision::Choice(1), 3)
                .unwrap();
            db.store_delegation_tx_hash(ROUND, 0, "dtx").unwrap();
            db.store_van_position(ROUND, 0, 7).unwrap();
            crate::storage::queries::store_vote(&db.conn(), ROUND, W, 0, 2, 1, &[0xCC; 16])
                .unwrap();
            store_vote_recovery_fixture(&db, 0, 2, 1, None);
            align_stored_commitments_with_recovery(&db, &[2]);
            if let Some(tx_hash) = tx_hash {
                db.record_vote_submission(ROUND, 0, 2, tx_hash).unwrap();
            }
            let vote_identity =
                submission_identity_fixture(ChainSubmissionTarget::Vote { proposal_id: 2 });
            let vote_generation = generation_for_vote(&db.conn(), &vote_identity).unwrap();
            db.conn()
                .execute(
                    "INSERT INTO chain_submissions
                     (identity_key, round_id, wallet_id, network,
                      bundle_index, kind, proposal_id, generation_digest, state,
                      committed_post_reservations, diagnostic_kind, diagnostic,
                      created_at, updated_at)
                     VALUES (?1, ?2, ?3, 'testnet', 0, 'vote', 2, ?4,
                             'recovering', 0, 'reconciliation_pending',
                             'possible dispatch awaits tree recovery', 9, 9)",
                    rusqlite::params![
                        submission_identity_key(&vote_identity),
                        ROUND,
                        W,
                        vote_generation.generation().digest().as_bytes().to_vec(),
                    ],
                )
                .unwrap();
            let delegation_identity =
                submission_identity_fixture(ChainSubmissionTarget::Delegation);
            db.conn()
                .execute(
                    "INSERT INTO chain_submissions
                     (identity_key, round_id, wallet_id, network,
                      bundle_index, kind, proposal_id, generation_digest, state,
                      committed_post_reservations, diagnostic_kind, diagnostic,
                      created_at, updated_at)
                     VALUES (?1, ?2, ?3, 'testnet', 0, 'delegation', NULL,
                             ?4, 'recovering', 0, 'ambiguous_dispatch',
                             'delegation response was lost after dispatch', 9, 9)",
                    rusqlite::params![
                        submission_identity_key(&delegation_identity),
                        ROUND,
                        W,
                        vec![0x73_u8; 32],
                    ],
                )
                .unwrap();

            let plan = resume_plan(&db, ROUND, &[1, 2, 3]).unwrap();

            assert_eq!(
                plan.next_steps,
                vec![
                    NextStep::AdvanceDelegation { bundle_index: 0 },
                    NextStep::AdvanceVote {
                        bundle_index: 0,
                        proposal_id: 2,
                    },
                ]
            );
            assert_eq!(plan.recovered_vote_work.len(), 1);
            assert_eq!(plan.recovered_delegation_work.len(), 1);
            assert!(plan.pending_recovery);
            assert!(plan.blocking_recovery);
            assert_eq!(plan.primary_action, RoundPlanAction::Delegate);
            assert_eq!(
                db.delegation_phase(ROUND, 0).unwrap(),
                DelegationPhase::SubmissionManaged
            );
            assert_eq!(
                db.vote_phase(ROUND, 0, 2).unwrap(),
                VotePhase::SubmissionManaged
            );
        }
    }

    #[test]
    fn bound_hashless_recovery_yields_a_typed_advance_step() {
        let db = db_with_bundle();
        db.set_ballot_intent(ROUND, 2, Decision::Choice(1), 3)
            .unwrap();
        db.store_delegation_tx_hash(ROUND, 0, "dtx").unwrap();
        db.store_van_position(ROUND, 0, 7).unwrap();
        crate::storage::queries::store_vote(&db.conn(), ROUND, W, 0, 2, 1, &[0xCC; 16]).unwrap();
        store_vote_recovery_fixture(&db, 0, 2, 1, None);
        align_stored_commitments_with_recovery(&db, &[2]);

        let identity = submission_identity_fixture(ChainSubmissionTarget::Vote { proposal_id: 2 });
        let generation = generation_for_vote(&db.conn(), &identity).unwrap();
        db.conn()
            .execute(
                "INSERT INTO chain_submissions
                 (identity_key, round_id, wallet_id, network,
                  bundle_index, kind, proposal_id, generation_digest, state,
                  committed_post_reservations, diagnostic_kind, diagnostic,
                  created_at, updated_at)
                 VALUES (?1, ?2, ?3, 'testnet', 0, 'vote', 2, ?4,
                         'recovering', 0, 'reconciliation_pending',
                         'version-17 positions require exact tree recovery', 9, 9)",
                rusqlite::params![
                    submission_identity_key(&identity),
                    ROUND,
                    W,
                    generation.generation().digest().as_bytes().to_vec(),
                ],
            )
            .unwrap();

        let plan = resume_plan(&db, ROUND, &[1, 2, 3]).unwrap();

        assert_eq!(
            plan.next_steps,
            vec![NextStep::AdvanceVote {
                bundle_index: 0,
                proposal_id: 2,
            }]
        );
        assert_eq!(plan.recovered_vote_work.len(), 1);
        assert!(plan.pending_recovery);
        assert!(plan.blocking_recovery);
        assert_eq!(plan.primary_action, RoundPlanAction::Vote);
        assert_eq!(
            db.vote_phase(ROUND, 0, 2).unwrap(),
            VotePhase::SubmissionManaged
        );
    }

    #[test]
    fn lifecycle_owned_vote_without_ballot_intent_still_yields_an_advance_step() {
        // The host never recorded an intent for proposal 2, yet the lifecycle
        // owns a vote for it. Advancement must not wait on the ballot.
        let db = db_with_bundle();
        db.store_delegation_tx_hash(ROUND, 0, "dtx").unwrap();
        db.store_van_position(ROUND, 0, 7).unwrap();
        crate::storage::queries::store_vote(&db.conn(), ROUND, W, 0, 2, 1, &[0xCC; 16]).unwrap();
        store_vote_recovery_fixture(&db, 0, 2, 1, None);
        align_stored_commitments_with_recovery(&db, &[2]);
        let identity = submission_identity_fixture(ChainSubmissionTarget::Vote { proposal_id: 2 });
        let generation = generation_for_vote(&db.conn(), &identity).unwrap();
        db.conn()
            .execute(
                "INSERT INTO chain_submissions
                 (identity_key, round_id, wallet_id, network,
                  bundle_index, kind, proposal_id, generation_digest, state,
                  committed_post_reservations, diagnostic_kind, diagnostic,
                  created_at, updated_at)
                 VALUES (?1, ?2, ?3, 'testnet', 0, 'vote', 2, ?4,
                         'recovering', 1, 'ambiguous_dispatch',
                         'vote response was lost after dispatch', 9, 9)",
                rusqlite::params![
                    submission_identity_key(&identity),
                    ROUND,
                    W,
                    generation.generation().digest().as_bytes().to_vec(),
                ],
            )
            .unwrap();
        assert!(db.ballot_intents(ROUND).unwrap().is_empty());

        let plan = resume_plan(&db, ROUND, &[1, 2, 3]).unwrap();

        assert_eq!(
            plan.next_steps,
            vec![NextStep::AdvanceVote {
                bundle_index: 0,
                proposal_id: 2,
            }]
        );
        assert_eq!(plan.recovered_vote_work.len(), 1);
        assert_eq!(
            plan.recovered_vote_work[0].kind,
            VoteRecoveryWorkKind::AdvanceVote
        );
        assert!(plan.pending_recovery);
        assert!(plan.blocking_recovery);
        assert_eq!(plan.primary_action, RoundPlanAction::Vote);
        assert_eq!(
            db.vote_phase(ROUND, 0, 2).unwrap(),
            VotePhase::SubmissionManaged
        );
    }

    #[test]
    fn lifecycle_owned_batch_without_ballot_intent_still_yields_an_advance_step() {
        let db = db_with_bundle();
        db.store_delegation_tx_hash(ROUND, 0, "dtx").unwrap();
        db.store_van_position(ROUND, 0, 7).unwrap();
        let ordered_batch_digest = store_two_action_batch_recovery_fixture(&db);
        align_stored_commitments_with_recovery(&db, &[1, 2]);
        let identity = submission_identity_fixture(ChainSubmissionTarget::VoteBatch {
            ordered_batch_digest,
        });
        let generation_digest = {
            let conn = db.conn();
            *generation_for_vote_batch(&conn, &identity)
                .unwrap()
                .generation()
                .digest()
                .as_bytes()
        };
        db.conn()
            .execute(
                "INSERT INTO chain_submissions
                 (identity_key, round_id, wallet_id, network, bundle_index,
                  kind, ordered_batch_digest, generation_digest, state,
                  committed_post_reservations, diagnostic_kind, diagnostic,
                  created_at, updated_at)
                 VALUES (?1, ?2, ?3, 'testnet', 0, 'vote_batch', ?4, ?5,
                         'recovering', 1, 'ambiguous_dispatch',
                         'batch response was lost after dispatch', 9, 9)",
                rusqlite::params![
                    submission_identity_key(&identity),
                    ROUND,
                    W,
                    ordered_batch_digest.as_slice(),
                    generation_digest.to_vec(),
                ],
            )
            .unwrap();
        assert!(db.ballot_intents(ROUND).unwrap().is_empty());

        let plan = resume_plan(&db, ROUND, &[1, 2, 3]).unwrap();

        assert_eq!(
            plan.next_steps,
            vec![NextStep::AdvanceVoteBatch {
                bundle_index: 0,
                proposal_id: 1,
            }]
        );
        assert_eq!(plan.recovered_vote_work.len(), 1);
        assert_eq!(
            plan.recovered_vote_work[0].kind,
            VoteRecoveryWorkKind::AdvanceVoteBatch
        );
        assert!(plan.blocking_recovery);
        for proposal_id in [1, 2] {
            assert_eq!(
                db.vote_phase(ROUND, 0, proposal_id).unwrap(),
                VotePhase::SubmissionManaged
            );
        }
    }

    #[test]
    fn in_flight_batch_reports_the_batch_row_candidate_hash() {
        let db = db_with_bundle();
        db.set_ballot_intent(ROUND, 1, Decision::Choice(0), 3)
            .unwrap();
        db.set_ballot_intent(ROUND, 2, Decision::Choice(1), 3)
            .unwrap();
        db.store_delegation_tx_hash(ROUND, 0, "dtx").unwrap();
        db.store_van_position(ROUND, 0, 7).unwrap();
        let ordered_batch_digest = store_two_action_batch_recovery_fixture(&db);
        align_stored_commitments_with_recovery(&db, &[1, 2]);
        let identity = submission_identity_fixture(ChainSubmissionTarget::VoteBatch {
            ordered_batch_digest,
        });
        let generation_digest = {
            let conn = db.conn();
            *generation_for_vote_batch(&conn, &identity)
                .unwrap()
                .generation()
                .digest()
                .as_bytes()
        };
        let candidate = [0x77_u8; 32];
        db.conn()
            .execute(
                "INSERT INTO chain_submissions
                 (identity_key, round_id, wallet_id, network, bundle_index,
                  kind, ordered_batch_digest, generation_digest, state,
                  candidate_transaction_hash, committed_post_reservations,
                  tracking_started_at, created_at, updated_at)
                 VALUES (?1, ?2, ?3, 'testnet', 0, 'vote_batch', ?4, ?5,
                         'tracking', ?6, 1, 10, 9, 10)",
                rusqlite::params![
                    submission_identity_key(&identity),
                    ROUND,
                    W,
                    ordered_batch_digest.as_slice(),
                    generation_digest.to_vec(),
                    candidate.as_slice(),
                ],
            )
            .unwrap();
        // Members own no projection hash while the batch is in flight.
        for proposal_id in [1, 2] {
            assert_eq!(
                crate::chain_submission::planning::vote_transaction_hash(
                    &db,
                    ROUND,
                    0,
                    proposal_id
                )
                .unwrap(),
                None
            );
        }

        let plan = resume_plan(&db, ROUND, &[1, 2, 3]).unwrap();

        assert_eq!(
            plan.recovered_vote_work,
            vec![VoteRecoveryWork {
                kind: VoteRecoveryWorkKind::AdvanceVoteBatch,
                bundle_index: 0,
                proposal_id: 1,
                tx_hash: Some(hex::encode(candidate)),
                vc_tree_position: None,
                share_indexes: Vec::new(),
            }]
        );
        // The per-vote recovery snapshot reports the same batch hash for
        // every member.
        let snapshot = crate::recovery::round_snapshot(&db, ROUND).unwrap();
        assert_eq!(snapshot.votes.len(), 2);
        for vote in &snapshot.votes {
            assert_eq!(vote.phase, VotePhase::SubmissionManaged);
            assert_eq!(
                vote.tx_hash.as_deref(),
                Some(hex::encode(candidate).as_str())
            );
        }
        assert_eq!(snapshot.commitment_bundles.len(), 2);
        for (bundle, proposal_id) in snapshot.commitment_bundles.iter().zip([1, 2]) {
            assert_eq!(bundle.bundle_index, 0);
            assert_eq!(bundle.proposal_id, proposal_id);
            assert_eq!(bundle.vc_tree_position, 0);
        }
    }

    fn insert_batch_row_fixture(
        db: &VotingDb,
        identity_key: &[u8],
        network: &str,
        ordered_batch_digest: [u8; 32],
        generation_digest: [u8; 32],
    ) {
        db.conn()
            .execute(
                "INSERT INTO chain_submissions
                 (identity_key, round_id, wallet_id, network, bundle_index,
                  kind, ordered_batch_digest, generation_digest, state,
                  committed_post_reservations, diagnostic_kind, diagnostic,
                  created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, 0, 'vote_batch', ?5, ?6,
                         'recovering', 1, 'ambiguous_dispatch',
                         'batch response was lost after dispatch', 9, 9)",
                rusqlite::params![
                    identity_key,
                    ROUND,
                    W,
                    network,
                    ordered_batch_digest.as_slice(),
                    generation_digest.to_vec(),
                ],
            )
            .unwrap();
    }

    #[test]
    fn batch_row_with_mismatched_generation_digest_is_an_invariant_error() {
        let db = db_with_bundle();
        db.store_delegation_tx_hash(ROUND, 0, "dtx").unwrap();
        db.store_van_position(ROUND, 0, 7).unwrap();
        let ordered_batch_digest = store_two_action_batch_recovery_fixture(&db);
        align_stored_commitments_with_recovery(&db, &[1, 2]);
        let identity = submission_identity_fixture(ChainSubmissionTarget::VoteBatch {
            ordered_batch_digest,
        });
        insert_batch_row_fixture(
            &db,
            &submission_identity_key(&identity),
            "testnet",
            ordered_batch_digest,
            [0x99; 32],
        );

        let error = db.vote_phases(ROUND).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("generation digest does not match persisted members"),
            "{error}"
        );
        assert!(resume_plan(&db, ROUND, &[1, 2, 3]).is_err());
    }

    #[test]
    fn vote_claimed_by_two_batch_rows_is_an_invariant_error() {
        let db = db_with_bundle();
        db.store_delegation_tx_hash(ROUND, 0, "dtx").unwrap();
        db.store_van_position(ROUND, 0, 7).unwrap();
        let ordered_batch_digest = store_two_action_batch_recovery_fixture(&db);
        align_stored_commitments_with_recovery(&db, &[1, 2]);
        let identity = submission_identity_fixture(ChainSubmissionTarget::VoteBatch {
            ordered_batch_digest,
        });
        let generation_digest = {
            let conn = db.conn();
            *generation_for_vote_batch(&conn, &identity)
                .unwrap()
                .generation()
                .digest()
                .as_bytes()
        };
        insert_batch_row_fixture(
            &db,
            &submission_identity_key(&identity),
            "testnet",
            ordered_batch_digest,
            generation_digest,
        );
        // The identity index is per network, so a second row on another
        // network is the only way a second batch can claim the same signed
        // members; the projection reads the whole round and must refuse it.
        insert_batch_row_fixture(
            &db,
            &[0x5A; 32],
            "mainnet",
            ordered_batch_digest,
            generation_digest,
        );

        let error = db.vote_phases(ROUND).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("belongs to multiple authoritative batches"),
            "{error}"
        );
    }

    #[test]
    fn overlapping_singleton_and_batch_rows_are_an_invariant_error() {
        let db = db_with_bundle();
        db.store_delegation_tx_hash(ROUND, 0, "dtx").unwrap();
        db.store_van_position(ROUND, 0, 7).unwrap();
        let ordered_batch_digest = store_two_action_batch_recovery_fixture(&db);
        align_stored_commitments_with_recovery(&db, &[1, 2]);
        let batch_identity = submission_identity_fixture(ChainSubmissionTarget::VoteBatch {
            ordered_batch_digest,
        });
        let generation_digest = {
            let conn = db.conn();
            *generation_for_vote_batch(&conn, &batch_identity)
                .unwrap()
                .generation()
                .digest()
                .as_bytes()
        };
        insert_batch_row_fixture(
            &db,
            &submission_identity_key(&batch_identity),
            "testnet",
            ordered_batch_digest,
            generation_digest,
        );
        let singleton_identity =
            submission_identity_fixture(ChainSubmissionTarget::Vote { proposal_id: 1 });
        db.conn()
            .execute(
                "INSERT INTO chain_submissions
                 (identity_key, round_id, wallet_id, network,
                  bundle_index, kind, proposal_id, generation_digest, state,
                  committed_post_reservations, diagnostic_kind, diagnostic,
                  created_at, updated_at)
                 VALUES (?1, ?2, ?3, 'testnet', 0, 'vote', 1, ?4,
                         'recovering', 1, 'ambiguous_dispatch',
                         'vote response was lost after dispatch', 9, 9)",
                rusqlite::params![
                    submission_identity_key(&singleton_identity),
                    ROUND,
                    W,
                    vec![0x77_u8; 32],
                ],
            )
            .unwrap();

        let error = db.vote_phases(ROUND).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("overlapping authoritative singleton and batch submissions"),
            "{error}"
        );
    }

    #[test]
    fn rejected_singleton_vote_never_yields_submit_or_poll_work() {
        let db = db_with_bundle();
        db.set_ballot_intent(ROUND, 2, Decision::Choice(1), 3)
            .unwrap();
        db.store_delegation_tx_hash(ROUND, 0, "dtx").unwrap();
        db.store_van_position(ROUND, 0, 7).unwrap();
        crate::storage::queries::store_vote(&db.conn(), ROUND, W, 0, 2, 1, &[0xCC; 16]).unwrap();
        store_vote_recovery_fixture(&db, 0, 2, 1, None);
        align_stored_commitments_with_recovery(&db, &[2]);
        let target = ChainSubmissionTarget::Vote { proposal_id: 2 };
        let identity = submission_identity_fixture(target);
        let generation_digest = {
            let conn = db.conn();
            *generation_for_vote(&conn, &identity)
                .unwrap()
                .generation()
                .digest()
                .as_bytes()
        };
        record_rejected_submission_fixture(&db, target, generation_digest);

        let plan = resume_plan(&db, ROUND, &[1, 2, 3]).unwrap();

        assert_eq!(
            db.vote_phase(ROUND, 0, 2).unwrap(),
            VotePhase::SubmissionRejected
        );
        assert!(plan.next_steps.is_empty());
        assert!(plan.recovered_vote_work.is_empty());
    }

    #[test]
    fn rejected_vote_batch_never_reschedules_its_members() {
        let db = db_with_bundle();
        db.set_ballot_intent(ROUND, 1, Decision::Choice(0), 3)
            .unwrap();
        db.set_ballot_intent(ROUND, 2, Decision::Choice(1), 3)
            .unwrap();
        db.store_delegation_tx_hash(ROUND, 0, "dtx").unwrap();
        db.store_van_position(ROUND, 0, 7).unwrap();
        let ordered_batch_digest = store_two_action_batch_recovery_fixture(&db);
        align_stored_commitments_with_recovery(&db, &[1, 2]);
        record_confirmed_share_fixture(&db, 0, 1, 0);
        record_confirmed_share_fixture(&db, 0, 2, 1);
        let target = ChainSubmissionTarget::VoteBatch {
            ordered_batch_digest,
        };
        let identity = submission_identity_fixture(target);
        let generation_digest = {
            let conn = db.conn();
            *generation_for_vote_batch(&conn, &identity)
                .unwrap()
                .generation()
                .digest()
                .as_bytes()
        };
        record_rejected_submission_fixture(&db, target, generation_digest);

        db.set_ballot_intent(ROUND, 1, Decision::Choice(0), 3)
            .unwrap();
        for (proposal_id, decision) in [(1, Decision::Choice(2)), (2, Decision::Skipped)] {
            let error = db
                .set_ballot_intent(ROUND, proposal_id, decision, 3)
                .unwrap_err();
            assert!(
                error.to_string().contains("lifecycle-owned vote recovery"),
                "unexpected error for proposal {proposal_id}: {error}"
            );
        }

        assert_eq!(
            db.ballot_intents(ROUND).unwrap(),
            vec![(1, Decision::Choice(0)), (2, Decision::Choice(1))]
        );
        for proposal_id in [1, 2] {
            assert!(crate::vote::recovery_bundle(&db, ROUND, 0, proposal_id)
                .unwrap()
                .is_some());
            assert_eq!(
                db.vote_phase(ROUND, 0, proposal_id).unwrap(),
                VotePhase::SubmissionRejected
            );
        }
        let shares = db.get_share_delegations(ROUND).unwrap();
        assert_eq!(shares.len(), 2);
        assert!(shares.iter().any(|share| {
            share.bundle_index == 0 && share.proposal_id == 1 && share.share_index == 0
        }));
        assert!(shares.iter().any(|share| {
            share.bundle_index == 0 && share.proposal_id == 2 && share.share_index == 1
        }));

        let snapshot = crate::recovery::round_snapshot(&db, ROUND).unwrap();
        assert_eq!(snapshot.votes.len(), 2);
        assert!(snapshot.votes.iter().all(|vote| {
            vote.phase == VotePhase::SubmissionRejected && vote.has_commitment_bundle
        }));
        let plan = resume_plan(&db, ROUND, &[1, 2, 3]).unwrap();

        assert_eq!(
            db.vote_phases(ROUND).unwrap(),
            vec![
                (0, 1, VotePhase::SubmissionRejected),
                (0, 2, VotePhase::SubmissionRejected),
            ]
        );
        assert!(plan.next_steps.is_empty());
        assert!(plan.recovered_vote_work.is_empty());
    }

    #[test]
    fn rejected_delegation_never_yields_delegate_work() {
        let db = db_with_bundle();
        db.set_ballot_intent(ROUND, 2, Decision::Choice(1), 3)
            .unwrap();
        record_rejected_submission_fixture(&db, ChainSubmissionTarget::Delegation, [0x72; 32]);

        let plan = resume_plan(&db, ROUND, &[1, 2, 3]).unwrap();

        assert_eq!(
            db.delegation_phase(ROUND, 0).unwrap(),
            DelegationPhase::SubmissionRejected
        );
        assert!(plan.next_steps.is_empty());
        assert!(plan.recovered_delegation_work.is_empty());
    }

    #[test]
    fn lifecycle_owned_vote_locks_conflicting_intent() {
        let db = db_with_bundle();
        db.set_ballot_intent(ROUND, 2, Decision::Choice(0), 3)
            .unwrap();
        crate::storage::queries::store_vote(&db.conn(), ROUND, W, 0, 2, 0, &[0xCC; 16]).unwrap();
        store_vote_recovery_fixture(&db, 0, 2, 0, None);
        align_stored_commitments_with_recovery(&db, &[2]);
        let identity = submission_identity_fixture(ChainSubmissionTarget::Vote { proposal_id: 2 });
        let generation = generation_for_vote(&db.conn(), &identity).unwrap();
        db.conn()
            .execute(
                "INSERT INTO chain_submissions
                 (identity_key, round_id, wallet_id, network,
                  bundle_index, kind, proposal_id, generation_digest, state,
                  committed_post_reservations, diagnostic_kind, diagnostic,
                  created_at, updated_at)
                 VALUES (?1, ?2, ?3, 'testnet', 0, 'vote', 2, ?4,
                         'recovering', 0, 'reconciliation_pending',
                         'possible dispatch awaits tree recovery', 9, 9)",
                rusqlite::params![
                    submission_identity_key(&identity),
                    ROUND,
                    W,
                    generation.generation().digest().as_bytes().to_vec(),
                ],
            )
            .unwrap();

        for decision in [Decision::Choice(1), Decision::Skipped] {
            let error = db.set_ballot_intent(ROUND, 2, decision, 3).unwrap_err();
            assert!(
                error.to_string().contains("ballot intent is locked"),
                "{error}"
            );
        }
        db.set_ballot_intent(ROUND, 2, Decision::Choice(0), 3)
            .unwrap();
    }

    #[test]
    fn committed_batch_yields_one_batch_submit_step() {
        let db = db_with_bundle();
        db.set_ballot_intent(ROUND, 1, Decision::Choice(0), 3)
            .unwrap();
        db.set_ballot_intent(ROUND, 2, Decision::Choice(1), 3)
            .unwrap();
        db.store_delegation_tx_hash(ROUND, 0, "dtx").unwrap();
        db.store_van_position(ROUND, 0, 7).unwrap();
        let digest = store_two_action_batch_recovery_fixture(&db);

        let plan = resume_plan(&db, ROUND, &[1, 2, 3]).unwrap();

        assert_eq!(
            plan.next_steps,
            vec![NextStep::AdvanceVoteBatch {
                bundle_index: 0,
                proposal_id: 1,
            }]
        );
        assert_eq!(
            plan.recovered_vote_work,
            vec![VoteRecoveryWork {
                kind: VoteRecoveryWorkKind::AdvanceVoteBatch,
                bundle_index: 0,
                proposal_id: 1,
                tx_hash: None,
                vc_tree_position: None,
                share_indexes: Vec::new(),
            }]
        );
        let recovered = crate::vote::recover_atomic_vote_batch(&db, ROUND, 0, 1).unwrap();
        assert_eq!(recovered.batch_digest, digest);
        assert_eq!(recovered.commitments.len(), 2);
        assert!(recovered.batch_json.starts_with("{\"votes\":["));
    }

    #[test]
    fn changing_choice_invalidates_the_whole_unsubmitted_batch() {
        let db = db_with_bundle();
        db.set_ballot_intent(ROUND, 1, Decision::Choice(0), 3)
            .unwrap();
        db.set_ballot_intent(ROUND, 2, Decision::Choice(1), 3)
            .unwrap();
        // A terminal roster is a precondition for planning any cast.
        db.set_ballot_intent(ROUND, 3, Decision::Skipped, 3)
            .unwrap();
        db.store_delegation_tx_hash(ROUND, 0, "dtx").unwrap();
        db.store_van_position(ROUND, 0, 7).unwrap();
        store_two_action_batch_recovery_fixture(&db);

        db.set_ballot_intent(ROUND, 2, Decision::Choice(2), 3)
            .unwrap();

        assert!(crate::vote::recovery_bundle(&db, ROUND, 0, 1)
            .unwrap()
            .is_none());
        assert!(crate::vote::recovery_bundle(&db, ROUND, 0, 2)
            .unwrap()
            .is_none());
        assert_eq!(db.vote_phase(ROUND, 0, 1).unwrap(), VotePhase::Prepared);
        assert_eq!(db.vote_phase(ROUND, 0, 2).unwrap(), VotePhase::Prepared);
        assert_eq!(
            resume_plan(&db, ROUND, &[1, 2, 3]).unwrap().next_steps,
            vec![
                NextStep::CastVote {
                    bundle_index: 0,
                    proposal_id: 1,
                    choice: 0,
                },
                NextStep::CastVote {
                    bundle_index: 0,
                    proposal_id: 2,
                    choice: 2,
                },
            ]
        );
    }

    #[test]
    fn skipping_choice_invalidates_the_whole_unsubmitted_batch() {
        let db = db_with_bundle();
        db.set_ballot_intent(ROUND, 1, Decision::Choice(0), 3)
            .unwrap();
        db.set_ballot_intent(ROUND, 2, Decision::Choice(1), 3)
            .unwrap();
        // A terminal roster is a precondition for planning any cast.
        db.set_ballot_intent(ROUND, 3, Decision::Skipped, 3)
            .unwrap();
        db.store_delegation_tx_hash(ROUND, 0, "dtx").unwrap();
        db.store_van_position(ROUND, 0, 7).unwrap();
        store_two_action_batch_recovery_fixture(&db);

        db.set_ballot_intent(ROUND, 2, Decision::Skipped, 3)
            .unwrap();

        assert!(crate::vote::recovery_bundle(&db, ROUND, 0, 1)
            .unwrap()
            .is_none());
        assert!(crate::vote::recovery_bundle(&db, ROUND, 0, 2)
            .unwrap()
            .is_none());
        assert_eq!(
            resume_plan(&db, ROUND, &[1, 2, 3]).unwrap().next_steps,
            vec![NextStep::CastVote {
                bundle_index: 0,
                proposal_id: 1,
                choice: 0,
            }]
        );
    }

    #[test]
    fn skipping_an_unsubmitted_singleton_clears_its_recovery() {
        let db = db_with_bundle();
        db.set_ballot_intent(ROUND, 1, Decision::Choice(0), 3)
            .unwrap();
        db.set_ballot_intent(ROUND, 2, Decision::Choice(1), 3)
            .unwrap();
        db.store_delegation_tx_hash(ROUND, 0, "dtx").unwrap();
        db.store_van_position(ROUND, 0, 7).unwrap();
        crate::storage::queries::store_vote(&db.conn(), ROUND, W, 0, 1, 0, &[0xCC; 16]).unwrap();
        store_vote_recovery_fixture(&db, 0, 1, 0, None);

        db.set_ballot_intent(ROUND, 1, Decision::Skipped, 3)
            .unwrap();

        assert!(crate::vote::recovery_bundle(&db, ROUND, 0, 1)
            .unwrap()
            .is_none());
        assert_eq!(
            resume_plan(&db, ROUND, &[1, 2]).unwrap().next_steps,
            vec![NextStep::CastVote {
                bundle_index: 0,
                proposal_id: 2,
                choice: 1,
            }]
        );
    }

    #[test]
    fn changing_one_member_rejects_a_partially_submitted_batch_atomically() {
        let db = db_with_bundle();
        db.set_ballot_intent(ROUND, 1, Decision::Choice(0), 3)
            .unwrap();
        db.set_ballot_intent(ROUND, 2, Decision::Choice(1), 3)
            .unwrap();
        db.store_delegation_tx_hash(ROUND, 0, "dtx").unwrap();
        db.store_van_position(ROUND, 0, 7).unwrap();
        store_two_action_batch_recovery_fixture(&db);
        db.conn()
            .execute(
                "UPDATE votes SET tx_hash = 'batch-tx'
                 WHERE round_id = :round_id
                   AND wallet_id = :wallet_id
                   AND bundle_index = 0
                   AND proposal_id = 1",
                named_params! {
                    ":round_id": ROUND,
                    ":wallet_id": W,
                },
            )
            .unwrap();

        let error = db
            .set_ballot_intent(ROUND, 2, Decision::Choice(2), 3)
            .unwrap_err();

        assert!(error.to_string().contains("submitted atomic vote batch"));
        assert_eq!(
            db.ballot_intents(ROUND).unwrap(),
            vec![(1, Decision::Choice(0)), (2, Decision::Choice(1))]
        );
        assert!(crate::vote::recovery_bundle(&db, ROUND, 0, 1)
            .unwrap()
            .is_some());
        assert!(crate::vote::recovery_bundle(&db, ROUND, 0, 2)
            .unwrap()
            .is_some());
    }

    #[test]
    fn submitted_batch_yields_one_batch_poll_step() {
        let db = db_with_bundle();
        db.set_ballot_intent(ROUND, 1, Decision::Choice(0), 3)
            .unwrap();
        db.set_ballot_intent(ROUND, 2, Decision::Choice(1), 3)
            .unwrap();
        db.store_delegation_tx_hash(ROUND, 0, "dtx").unwrap();
        db.store_van_position(ROUND, 0, 7).unwrap();
        let digest = store_two_action_batch_recovery_fixture(&db);
        crate::vote::record_batch_submission(&db, ROUND, 0, &digest, "batch-tx").unwrap();

        let plan = resume_plan(&db, ROUND, &[1, 2, 3]).unwrap();

        assert_eq!(
            plan.next_steps,
            vec![NextStep::AdvanceVoteBatch {
                bundle_index: 0,
                proposal_id: 1,
            }]
        );
        assert_eq!(
            plan.recovered_vote_work,
            vec![VoteRecoveryWork {
                kind: VoteRecoveryWorkKind::AdvanceVoteBatch,
                bundle_index: 0,
                proposal_id: 1,
                tx_hash: Some("batch-tx".to_string()),
                vc_tree_position: None,
                share_indexes: Vec::new(),
            }]
        );
    }

    #[test]
    fn pending_batch_defers_a_new_lower_proposal_cast_until_confirmation() {
        let db = db_with_bundle();
        db.set_ballot_intent(ROUND, 1, Decision::Choice(0), 3)
            .unwrap();
        db.set_ballot_intent(ROUND, 2, Decision::Choice(1), 3)
            .unwrap();
        db.set_ballot_intent(ROUND, 3, Decision::Choice(2), 3)
            .unwrap();
        db.store_delegation_tx_hash(ROUND, 0, "dtx").unwrap();
        db.store_van_position(ROUND, 0, 7).unwrap();
        let digest = store_two_action_batch_recovery_fixture_for(&db, (2, 1), (3, 2));

        let committed_plan = resume_plan(&db, ROUND, &[1, 2, 3]).unwrap();
        assert_eq!(
            committed_plan.next_steps,
            vec![NextStep::AdvanceVoteBatch {
                bundle_index: 0,
                proposal_id: 2,
            }]
        );

        crate::vote::record_batch_submission(&db, ROUND, 0, &digest, "batch-tx").unwrap();

        let submitted_plan = resume_plan(&db, ROUND, &[1, 2, 3]).unwrap();

        assert_eq!(
            submitted_plan.next_steps,
            vec![NextStep::AdvanceVoteBatch {
                bundle_index: 0,
                proposal_id: 2,
            }]
        );
    }

    #[test]
    fn changed_choice_after_submission_is_invalid_recovery_state() {
        let db = db_with_bundle();
        db.store_delegation_tx_hash(ROUND, 0, "dtx").unwrap();
        db.store_van_position(ROUND, 0, 7).unwrap();

        confirm_vote_fixture(&db, 0, 2, 0);
        db.conn()
            .execute(
                "INSERT INTO ballot_intent
                    (round_id, wallet_id, proposal_id, skipped, choice, created_at, updated_at)
                 VALUES (:round_id, :wallet_id, :proposal_id, 0, 1, 1, 1)",
                named_params! {
                    ":round_id": ROUND,
                    ":wallet_id": W,
                    ":proposal_id": 2_i64,
                },
            )
            .unwrap();

        let err = resume_plan(&db, ROUND, &[1, 2, 3]).unwrap_err();

        assert!(
            err.to_string()
                .contains("submitted vote that conflicts with ballot intent"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn conflicting_intent_after_submission_is_rejected_before_share_cleanup() {
        let db = db_with_bundle();
        db.set_ballot_intent(ROUND, 2, Decision::Choice(0), 3)
            .unwrap();
        db.store_delegation_tx_hash(ROUND, 0, "dtx").unwrap();
        db.store_van_position(ROUND, 0, 7).unwrap();
        confirm_vote_fixture(&db, 0, 2, 0);
        record_all_confirmed_share_fixtures(&db, 0, 2);

        for decision in [Decision::Choice(1), Decision::Skipped] {
            let err = db.set_ballot_intent(ROUND, 2, decision, 3).unwrap_err();
            assert!(
                err.to_string()
                    .contains("submitted vote that conflicts with ballot intent"),
                "unexpected error for {decision:?}: {err}"
            );
            assert_eq!(
                db.ballot_intents(ROUND).unwrap(),
                vec![(2, Decision::Choice(0))]
            );
            let shares = db.get_share_delegations(ROUND).unwrap();
            assert_eq!(shares.len(), 2);
            let share_indexes = shares
                .iter()
                .map(|share| share.share_index)
                .collect::<BTreeSet<_>>();
            assert_eq!(share_indexes, BTreeSet::from([0, 1]));
            assert!(shares.iter().all(|share| {
                share.bundle_index == 0 && share.proposal_id == 2 && share.confirmed
            }));
        }
    }

    #[test]
    fn active_singleton_generation_locks_intent_and_recovery_material() {
        let db = db_with_bundle();
        db.set_ballot_intent(ROUND, 2, Decision::Choice(0), 3)
            .unwrap();
        crate::storage::queries::store_vote(&db.conn(), ROUND, W, 0, 2, 0, &[0xCC; 16]).unwrap();
        store_vote_recovery_fixture(&db, 0, 2, 0, None);
        record_submitted_share_fixture(&db, 0, 2, 0, &["https://helper.example".to_string()]);
        db.conn()
            .execute(
                "INSERT INTO chain_submissions
                 (identity_key, round_id, wallet_id, network, bundle_index,
                  kind, proposal_id, generation_digest, state, candidate_transaction_hash,
                  committed_post_reservations, tracking_started_at, created_at, updated_at)
                 VALUES (?1, ?2, ?3, 'testnet', 0, 'vote', 2, ?4,
                         'tracking', ?5, 1, 10, 9, 10)",
                rusqlite::params![
                    vec![0x41_u8; 32],
                    ROUND,
                    W,
                    vec![0x42_u8; 32],
                    vec![0x43_u8; 32]
                ],
            )
            .unwrap();

        let error = db
            .set_ballot_intent(ROUND, 2, Decision::Choice(1), 3)
            .unwrap_err();
        assert!(
            error.to_string().contains("lifecycle-owned vote recovery"),
            "{error}"
        );
        assert!(crate::vote::recovery_bundle(&db, ROUND, 0, 2)
            .unwrap()
            .is_some());
        assert_eq!(db.get_share_delegations(ROUND).unwrap().len(), 1);
        assert_eq!(
            db.ballot_intents(ROUND).unwrap(),
            vec![(2, Decision::Choice(0))]
        );
    }

    #[test]
    fn active_batch_generation_locks_every_member_intent() {
        let db = db_with_bundle();
        db.set_ballot_intent(ROUND, 1, Decision::Choice(0), 3)
            .unwrap();
        db.set_ballot_intent(ROUND, 2, Decision::Choice(1), 3)
            .unwrap();
        let digest = store_two_action_batch_recovery_fixture(&db);
        db.conn()
            .execute(
                "INSERT INTO chain_submissions
                 (identity_key, round_id, wallet_id, network, bundle_index,
                  kind, ordered_batch_digest, generation_digest, state,
                  candidate_transaction_hash, committed_post_reservations,
                  tracking_started_at, created_at, updated_at)
                 VALUES (?1, ?2, ?3, 'testnet', 0, 'vote_batch', ?4, ?5,
                         'tracking', ?6, 1, 10, 9, 10)",
                rusqlite::params![
                    vec![0x51_u8; 32],
                    ROUND,
                    W,
                    digest.as_slice(),
                    vec![0x52_u8; 32],
                    vec![0x53_u8; 32]
                ],
            )
            .unwrap();

        let error = db
            .set_ballot_intent(ROUND, 2, Decision::Skipped, 3)
            .unwrap_err();
        assert!(
            error.to_string().contains("lifecycle-owned vote recovery"),
            "{error}"
        );
        for proposal_id in [1, 2] {
            assert!(crate::vote::recovery_bundle(&db, ROUND, 0, proposal_id)
                .unwrap()
                .is_some());
        }
    }

    #[test]
    fn stale_vote_submission_after_choice_change_is_rejected() {
        let db = db_with_bundle();
        db.set_ballot_intent(ROUND, 2, Decision::Choice(0), 3)
            .unwrap();
        // A terminal roster is a precondition for planning any cast.
        db.set_ballot_intent(ROUND, 1, Decision::Skipped, 3)
            .unwrap();
        db.set_ballot_intent(ROUND, 3, Decision::Skipped, 3)
            .unwrap();
        db.store_delegation_tx_hash(ROUND, 0, "dtx").unwrap();
        db.store_van_position(ROUND, 0, 7).unwrap();
        crate::storage::queries::store_vote(&db.conn(), ROUND, W, 0, 2, 0, &[0xCC; 16]).unwrap();

        db.set_ballot_intent(ROUND, 2, Decision::Choice(1), 3)
            .unwrap();
        let err = db
            .record_vote_submission(ROUND, 0, 2, "old-vtx")
            .unwrap_err();

        assert!(
            err.to_string()
                .contains("vote submission conflicts with ballot intent"),
            "unexpected error: {err}"
        );
        assert_eq!(db.get_vote_tx_hash(ROUND, 0, 2).unwrap(), None);
        let plan = resume_plan(&db, ROUND, &[1, 2, 3]).unwrap();
        assert_eq!(
            plan.next_steps,
            vec![NextStep::CastVote {
                bundle_index: 0,
                proposal_id: 2,
                choice: 1,
            }]
        );
    }

    #[test]
    fn stale_vote_submission_after_skip_is_rejected() {
        let db = db_with_bundle();
        db.set_ballot_intent(ROUND, 2, Decision::Choice(0), 3)
            .unwrap();
        crate::storage::queries::store_vote(&db.conn(), ROUND, W, 0, 2, 0, &[0xCC; 16]).unwrap();

        db.set_ballot_intent(ROUND, 2, Decision::Skipped, 3)
            .unwrap();
        let err = db
            .record_vote_submission(ROUND, 0, 2, "old-vtx")
            .unwrap_err();

        assert!(
            err.to_string()
                .contains("cannot record vote submission for skipped proposal"),
            "unexpected error: {err}"
        );
        assert_eq!(db.get_vote_tx_hash(ROUND, 0, 2).unwrap(), None);
        let plan = resume_plan(&db, ROUND, &[1, 2, 3]).unwrap();
        assert!(!plan.pending_recovery);
        assert_eq!(plan.open_proposals, vec![1, 3]);
        assert!(!plan.completed_vote_artifact);
        assert!(!plan.completed_for_display);
        assert_eq!(plan.primary_action, RoundPlanAction::Idle);
    }

    #[test]
    fn round_plan_selects_share_zero_of_lowest_voted_proposal() {
        let db = db_with_bundle();
        db.set_ballot_intent(ROUND, 1, Decision::Skipped, 3)
            .unwrap();
        db.set_ballot_intent(ROUND, 3, Decision::Choice(1), 3)
            .unwrap();
        db.set_ballot_intent(ROUND, 2, Decision::Choice(0), 3)
            .unwrap();

        assert_eq!(
            resume_plan(&db, ROUND, &[1, 2, 3])
                .unwrap()
                .immediate_share_key,
            Some(ImmediateShareKey {
                bundle_index: 0,
                proposal_id: 2,
                share_index: 0,
            })
        );
    }

    #[test]
    fn round_plan_has_no_immediate_share_before_a_vote_choice() {
        let db = db_with_bundle();
        db.set_ballot_intent(ROUND, 1, Decision::Skipped, 3)
            .unwrap();

        assert_eq!(
            resume_plan(&db, ROUND, &[1, 2, 3])
                .unwrap()
                .immediate_share_key,
            None
        );
    }

    #[test]
    fn round_plan_reports_immediate_share_confirmation() {
        let db = db_with_bundle();
        db.set_ballot_intent(ROUND, 2, Decision::Choice(0), 3)
            .unwrap();
        assert!(
            !resume_plan(&db, ROUND, &[1, 2, 3])
                .unwrap()
                .immediate_share_confirmed
        );

        confirm_vote_fixture(&db, 0, 2, 0);
        record_confirmed_share_fixture(&db, 0, 2, 0);

        assert!(
            resume_plan(&db, ROUND, &[1, 2, 3])
                .unwrap()
                .immediate_share_confirmed
        );
    }

    #[test]
    fn round_plan_immediate_share_is_stable_after_vote_completion() {
        let db = db_with_bundle();
        db.set_ballot_intent(ROUND, 1, Decision::Choice(1), 3)
            .unwrap();
        db.set_ballot_intent(ROUND, 2, Decision::Choice(0), 3)
            .unwrap();
        let before = resume_plan(&db, ROUND, &[1, 2, 3])
            .unwrap()
            .immediate_share_key;

        confirm_vote_fixture(&db, 0, 1, 1);

        assert_eq!(
            resume_plan(&db, ROUND, &[1, 2, 3])
                .unwrap()
                .immediate_share_key,
            before
        );
    }

    #[test]
    fn changed_choice_ignores_stale_share_confirmations() {
        let db = db_with_bundle();
        db.store_delegation_tx_hash(ROUND, 0, "dtx").unwrap();
        db.store_van_position(ROUND, 0, 7).unwrap();

        crate::storage::queries::store_vote(&db.conn(), ROUND, W, 0, 2, 0, &[0xCC; 16]).unwrap();
        db.record_share_delegation(
            ROUND,
            0,
            2,
            0,
            &["https://helper.example".to_string()],
            &[0x44; 32],
            0,
        )
        .unwrap();

        db.set_ballot_intent(ROUND, 2, Decision::Choice(1), 3)
            .unwrap();
        // A terminal roster is a precondition for planning any cast.
        db.set_ballot_intent(ROUND, 1, Decision::Skipped, 3)
            .unwrap();
        db.set_ballot_intent(ROUND, 3, Decision::Skipped, 3)
            .unwrap();
        let plan = resume_plan(&db, ROUND, &[1, 2, 3]).unwrap();

        assert_eq!(
            plan.next_steps,
            vec![NextStep::CastVote {
                bundle_index: 0,
                proposal_id: 2,
                choice: 1,
            }]
        );
    }

    #[test]
    fn recast_choice_keeps_old_share_confirmations_suppressed() {
        let db = db_with_bundle();
        db.store_delegation_tx_hash(ROUND, 0, "dtx").unwrap();
        db.store_van_position(ROUND, 0, 7).unwrap();

        crate::storage::queries::store_vote(&db.conn(), ROUND, W, 0, 2, 0, &[0xCC; 16]).unwrap();
        db.record_share_delegation(
            ROUND,
            0,
            2,
            0,
            &["https://helper.example".to_string()],
            &[0x44; 32],
            0,
        )
        .unwrap();

        db.set_ballot_intent(ROUND, 2, Decision::Choice(1), 3)
            .unwrap();
        // A terminal roster is a precondition for planning any cast.
        db.set_ballot_intent(ROUND, 1, Decision::Skipped, 3)
            .unwrap();
        db.set_ballot_intent(ROUND, 3, Decision::Skipped, 3)
            .unwrap();
        crate::storage::queries::store_vote(&db.conn(), ROUND, W, 0, 2, 1, &[0xDD; 16]).unwrap();

        assert!(
            db.get_share_delegations(ROUND).unwrap().is_empty(),
            "recasting must clear helper-share rows for the old vote"
        );

        let plan = resume_plan(&db, ROUND, &[1, 2, 3]).unwrap();
        assert_eq!(
            plan.next_steps,
            vec![NextStep::CastVote {
                bundle_index: 0,
                proposal_id: 2,
                choice: 1,
            }]
        );
    }

    #[test]
    fn skipped_intent_clears_and_blocks_stale_share_rows() {
        let db = db_with_bundle();
        crate::storage::queries::store_vote(&db.conn(), ROUND, W, 0, 2, 0, &[0xCC; 16]).unwrap();
        db.record_share_delegation(
            ROUND,
            0,
            2,
            0,
            &["https://helper.example".to_string()],
            &[0x44; 32],
            0,
        )
        .unwrap();

        db.set_ballot_intent(ROUND, 2, Decision::Skipped, 3)
            .unwrap();

        assert!(db.get_share_delegations(ROUND).unwrap().is_empty());
        let err = db
            .record_share_delegation(
                ROUND,
                0,
                2,
                0,
                &["https://helper.example".to_string()],
                &[0x44; 32],
                0,
            )
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("cannot record share delegation for skipped proposal"),
            "{err}"
        );
    }

    #[test]
    fn skipped_proposal_is_terminal_not_recovery() {
        let db = db_with_bundle();
        db.set_ballot_intent(ROUND, 1, Decision::Skipped, 3)
            .unwrap();
        let plan = resume_plan(&db, ROUND, &[1, 2, 3]).unwrap();
        assert!(!plan.pending_recovery);
        assert_eq!(plan.open_proposals, vec![2, 3]);
    }

    #[test]
    fn choice_intent_without_bundles_is_invalid() {
        let db = VotingDb::open_in_memory().unwrap();
        db.set_wallet_id(W);
        db.create_round(crate::Network::Testnet, &round_params(), None)
            .unwrap();
        db.set_ballot_intent(ROUND, 2, Decision::Choice(1), 3)
            .unwrap();

        let err = resume_plan(&db, ROUND, &[1, 2, 3]).unwrap_err();
        assert!(
            err.to_string().contains("no eligible bundle rows"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn midflight_delegation_is_recovery_without_votes() {
        let db = db_with_bundle();
        db.store_delegation_tx_hash(ROUND, 0, "dtx").unwrap(); // Submitted, no VAN
        let plan = resume_plan(&db, ROUND, &[1]).unwrap();
        assert_eq!(
            plan.next_steps,
            vec![NextStep::AdvanceDelegation { bundle_index: 0 }]
        );
        assert!(plan.hotkey_bound);
        assert_eq!(
            plan.recovered_delegation_work,
            vec![DelegationRecoveryWork {
                kind: DelegationRecoveryWorkKind::AdvanceDelegation,
                bundle_index: 0,
                phase: DelegationPhase::Submitted,
                tx_hash: Some("dtx".to_string()),
            }]
        );
    }

    #[test]
    fn confirmed_delegation_without_votes_still_binds_hotkey() {
        let db = db_with_bundle();
        db.store_delegation_tx_hash(ROUND, 0, "dtx").unwrap();
        db.store_van_position(ROUND, 0, 7).unwrap();

        let plan = resume_plan(&db, ROUND, &[1, 2, 3]).unwrap();

        assert!(!plan.pending_recovery);
        assert!(plan.next_steps.is_empty());
        assert!(plan.hotkey_bound);
        assert!(!plan.completed_vote_artifact);
        assert!(plan.needs_draft_setup);
        assert_eq!(
            plan.delegation_statuses,
            vec![DelegationStatus {
                bundle_index: 0,
                phase: DelegationPhase::Confirmed,
                tx_hash: Some("dtx".to_string()),
                submission_diagnostic: None,
                terminal: false,
            }]
        );
        assert!(plan.recovered_delegation_work.is_empty());
    }

    #[test]
    fn multi_bundle_orders_vote_steps_by_proposal() {
        let db = VotingDb::open_in_memory().unwrap();
        db.set_wallet_id(W);
        db.create_round(crate::Network::Testnet, &round_params(), None)
            .unwrap();
        db.ensure_bundles(
            ROUND,
            &[note(0), note(1), note(2), note(3), note(4), note(5)],
        )
        .unwrap();

        // Verify the actual bundle count before asserting; 6 notes with
        // BALLOT_DIVISOR value each should produce 2 bundles (indices 0 and 1).
        let phases = db.delegation_phases(ROUND).unwrap();
        let bundle_count = phases.len();
        assert_eq!(
            bundle_count,
            2,
            "expected 6 notes → 2 bundles, got {bundle_count}; adjust this test if bundling rules changed"
        );

        db.set_ballot_intent(ROUND, 2, Decision::Choice(1), 3)
            .unwrap();
        // A terminal roster is a precondition for planning any cast.
        db.set_ballot_intent(ROUND, 1, Decision::Skipped, 3)
            .unwrap();
        db.set_ballot_intent(ROUND, 3, Decision::Skipped, 3)
            .unwrap();
        let plan = resume_plan(&db, ROUND, &[1, 2, 3]).unwrap();

        // Delegation prerequisites come first, then vote work is ordered by
        // proposal before bundle.
        assert_eq!(
            plan.next_steps,
            vec![
                NextStep::Delegate { bundle_index: 0 },
                NextStep::Delegate { bundle_index: 1 },
                NextStep::CastVote {
                    bundle_index: 0,
                    proposal_id: 2,
                    choice: 1,
                },
                NextStep::CastVote {
                    bundle_index: 1,
                    proposal_id: 2,
                    choice: 1,
                },
            ]
        );
    }

    #[test]
    fn interrupted_second_bundle_vote_defers_its_later_proposals() {
        let db = VotingDb::open_in_memory().unwrap();
        db.set_wallet_id(W);
        db.create_round(crate::Network::Testnet, &round_params(), None)
            .unwrap();
        db.ensure_bundles(
            ROUND,
            &[note(0), note(1), note(2), note(3), note(4), note(5)],
        )
        .unwrap();
        db.store_delegation_tx_hash(ROUND, 0, "dtx-0").unwrap();
        db.store_van_position(ROUND, 0, 7).unwrap();
        db.store_delegation_tx_hash(ROUND, 1, "dtx-1").unwrap();
        db.store_van_position(ROUND, 1, 8).unwrap();

        for (proposal_id, choice) in [(1, 0), (2, 1), (3, 0)] {
            db.set_ballot_intent(ROUND, proposal_id, Decision::Choice(choice), 3)
                .unwrap();
        }
        confirm_vote_fixture(&db, 0, 1, 0);
        record_all_confirmed_share_fixtures(&db, 0, 1);
        crate::storage::queries::store_vote(&db.conn(), ROUND, W, 1, 1, 0, &[0xCC; 16]).unwrap();
        store_vote_recovery_fixture(&db, 1, 1, 0, None);

        let plan = resume_plan(&db, ROUND, &[1, 2, 3]).unwrap();

        assert_eq!(
            plan.next_steps,
            vec![
                NextStep::AdvanceVote {
                    bundle_index: 1,
                    proposal_id: 1,
                },
                NextStep::CastVote {
                    bundle_index: 0,
                    proposal_id: 2,
                    choice: 1,
                },
                NextStep::CastVote {
                    bundle_index: 0,
                    proposal_id: 3,
                    choice: 0,
                },
            ]
        );
    }

    #[test]
    fn confirmed_vote_without_recorded_shares_yields_submit_shares() {
        let db = VotingDb::open_in_memory().unwrap();
        db.set_wallet_id(W);
        db.create_round(crate::Network::Testnet, &round_params(), None)
            .unwrap();
        db.ensure_bundles(
            ROUND,
            &[note(0), note(1), note(2), note(3), note(4), note(5)],
        )
        .unwrap();
        db.store_delegation_tx_hash(ROUND, 0, "dtx-0").unwrap();
        db.store_van_position(ROUND, 0, 7).unwrap();
        db.store_delegation_tx_hash(ROUND, 1, "dtx-1").unwrap();
        db.store_van_position(ROUND, 1, 8).unwrap();

        for (proposal_id, choice) in [(1, 0), (2, 1)] {
            db.set_ballot_intent(ROUND, proposal_id, Decision::Choice(choice), 3)
                .unwrap();
        }
        confirm_vote_fixture(&db, 0, 1, 0);
        record_all_confirmed_share_fixtures(&db, 0, 1);
        confirm_vote_fixture(&db, 1, 1, 0);

        let plan = resume_plan(&db, ROUND, &[1, 2]).unwrap();

        assert_eq!(
            plan.next_steps,
            vec![
                NextStep::SubmitShares {
                    bundle_index: 1,
                    proposal_id: 1,
                    share_index: 0,
                },
                NextStep::SubmitShares {
                    bundle_index: 1,
                    proposal_id: 1,
                    share_index: 1,
                },
                NextStep::CastVote {
                    bundle_index: 0,
                    proposal_id: 2,
                    choice: 1,
                },
                NextStep::CastVote {
                    bundle_index: 1,
                    proposal_id: 2,
                    choice: 1,
                },
            ]
        );
        assert_eq!(
            plan.recovered_vote_work,
            vec![VoteRecoveryWork {
                kind: VoteRecoveryWorkKind::SubmitShares,
                bundle_index: 1,
                proposal_id: 1,
                tx_hash: None,
                vc_tree_position: Some(42),
                share_indexes: vec![0, 1],
            }]
        );
    }

    #[test]
    fn confirmed_vote_with_partial_recorded_shares_yields_submit_shares() {
        let db = db_with_bundle();
        db.set_ballot_intent(ROUND, 2, Decision::Choice(1), 3)
            .unwrap();
        confirm_vote_fixture(&db, 0, 2, 1);
        record_confirmed_share_fixture(&db, 0, 2, 0);

        let plan = resume_plan(&db, ROUND, &[1, 2, 3]).unwrap();

        assert_eq!(
            plan.next_steps,
            vec![NextStep::SubmitShares {
                bundle_index: 0,
                proposal_id: 2,
                share_index: 1,
            }]
        );
    }

    #[test]
    fn confirmed_vote_with_all_recorded_shares_has_no_share_submission_step() {
        let db = db_with_bundle();
        db.set_ballot_intent(ROUND, 2, Decision::Choice(1), 3)
            .unwrap();
        confirm_vote_fixture(&db, 0, 2, 1);
        record_all_confirmed_share_fixtures(&db, 0, 2);

        let plan = resume_plan(&db, ROUND, &[1, 2, 3]).unwrap();

        assert!(plan.next_steps.is_empty(), "got {:?}", plan.next_steps);
        assert!(plan.completed_vote_artifact);
        assert!(plan.completed_for_display);
        assert_eq!(
            plan.completed_vote_display
                .as_ref()
                .map(|display| &display.choices),
            Some(&vec![
                CompletedVoteChoice {
                    proposal_id: 1,
                    choice: None,
                },
                CompletedVoteChoice {
                    proposal_id: 2,
                    choice: Some(1),
                },
                CompletedVoteChoice {
                    proposal_id: 3,
                    choice: None,
                },
            ])
        );
        assert!(plan
            .completed_vote_display
            .as_ref()
            .and_then(|display| display.voted_at)
            .is_some());
        assert!(plan.needs_draft_setup);
        assert_eq!(plan.primary_action, RoundPlanAction::Done);
        assert!(plan.recovered_vote_work.is_empty());
    }

    #[test]
    fn confirmed_single_share_vote_with_recorded_payload_has_no_share_submission_step() {
        let db = db_with_bundle();
        db.set_ballot_intent(ROUND, 2, Decision::Choice(1), 3)
            .unwrap();
        confirm_vote_fixture(&db, 0, 2, 1);
        let mut recovery = recovery_bundle_fixture(0, 2, 1, 42);
        recovery.single_share = true;
        store_recovery_bundle_fixture(&db, &recovery, Some(42));
        record_confirmed_share_fixture(&db, 0, 2, 0);

        let plan = resume_plan(&db, ROUND, &[1, 2, 3]).unwrap();

        assert!(plan.next_steps.is_empty(), "got {:?}", plan.next_steps);
    }

    #[test]
    fn delegate_suppressed_when_vote_confirmed() {
        let db = db_with_bundle();
        confirm_vote_fixture(&db, 0, 2, 1);
        record_all_confirmed_share_fixtures(&db, 0, 2);

        // Bundle 0 delegation is still only Prepared — but the vote is Confirmed,
        // so no Delegate step should appear and the plan has no work left.
        db.set_ballot_intent(ROUND, 2, Decision::Choice(1), 3)
            .unwrap();
        let plan = resume_plan(&db, ROUND, &[1, 2, 3]).unwrap();

        assert!(
            plan.next_steps.is_empty(),
            "expected no steps, got: {:?}",
            plan.next_steps
        );
        assert!(!plan.pending_recovery);
    }

    #[test]
    fn confirm_share_emitted_for_unconfirmed_share() {
        let db = db_with_bundle();
        db.record_share_delegation(
            ROUND,
            0,
            2,
            0,
            &["https://helper.example".to_string()],
            &[0x44; 32],
            0,
        )
        .unwrap();

        let plan = resume_plan(&db, ROUND, &[1, 2, 3]).unwrap();

        assert!(
            plan.next_steps.contains(&NextStep::ConfirmShare {
                bundle_index: 0,
                proposal_id: 2,
                share_index: 0
            }),
            "expected ConfirmShare in steps, got: {:?}",
            plan.next_steps
        );
    }

    #[test]
    fn unconfirmed_shares_are_reported_and_schedule_a_tracking_pass() {
        let db = db_with_bundle();
        assert!(!resume_plan(&db, ROUND, &[1, 2, 3])
            .unwrap()
            .has_unconfirmed_shares);
        assert_eq!(
            crate::share::next_tracking_delay_for_round(
                &db,
                ROUND,
                1_000,
                crate::share::ShareTimingPolicy::default()
            )
            .unwrap(),
            None
        );

        db.record_share_delegation(
            ROUND,
            0,
            2,
            0,
            &["https://helper.example".to_string()],
            &[0x44; 32],
            0,
        )
        .unwrap();

        // An accepted-but-unconfirmed share is not blocking, yet it still has
        // to keep background tracking armed.
        let plan = resume_plan(&db, ROUND, &[1, 2, 3]).unwrap();
        assert!(plan.has_unconfirmed_shares);
        assert!(!plan.blocking_share_work);
        assert!(crate::share::next_tracking_delay_for_round(
            &db,
            ROUND,
            1_000,
            crate::share::ShareTimingPolicy::default()
        )
        .unwrap()
        .is_some());
    }

    #[test]
    fn accepted_helper_share_confirmation_is_nonblocking_display_work() {
        let db = db_with_bundle();
        db.set_ballot_intent(ROUND, 2, Decision::Choice(1), 3)
            .unwrap();
        confirm_vote_fixture(&db, 0, 2, 1);
        let mut recovery = recovery_bundle_fixture(0, 2, 1, 42);
        recovery.single_share = true;
        store_recovery_bundle_fixture(&db, &recovery, Some(42));
        record_submitted_share_fixture(&db, 0, 2, 0, &["https://helper.example".to_string()]);

        let plan = resume_plan(&db, ROUND, &[1, 2, 3]).unwrap();

        assert_eq!(
            plan.next_steps,
            vec![NextStep::ConfirmShare {
                bundle_index: 0,
                proposal_id: 2,
                share_index: 0
            }]
        );
        assert!(plan.pending_recovery);
        assert!(!plan.blocking_recovery);
        assert!(!plan.blocking_share_work);
        assert!(plan.completed_vote_artifact);
        assert!(plan.completed_for_display);
        assert!(plan.needs_draft_setup);
        assert_eq!(plan.primary_action, RoundPlanAction::Done);
    }

    #[test]
    fn unaccepted_helper_share_confirmation_blocks_foreground_recovery() {
        let db = db_with_bundle();
        db.set_ballot_intent(ROUND, 2, Decision::Choice(1), 3)
            .unwrap();
        confirm_vote_fixture(&db, 0, 2, 1);
        let mut recovery = recovery_bundle_fixture(0, 2, 1, 42);
        recovery.single_share = true;
        store_recovery_bundle_fixture(&db, &recovery, Some(42));
        record_submitted_share_fixture(&db, 0, 2, 0, &[]);

        let plan = resume_plan(&db, ROUND, &[1, 2, 3]).unwrap();

        assert_eq!(
            plan.next_steps,
            vec![NextStep::ConfirmShare {
                bundle_index: 0,
                proposal_id: 2,
                share_index: 0
            }]
        );
        assert!(plan.pending_recovery);
        assert!(plan.blocking_recovery);
        assert!(plan.blocking_share_work);
        assert!(plan.completed_vote_artifact);
        assert!(!plan.completed_for_display);
        assert!(!plan.needs_draft_setup);
        assert_eq!(plan.primary_action, RoundPlanAction::SubmitShares);
        assert_eq!(
            plan.recovered_vote_work,
            vec![VoteRecoveryWork {
                kind: VoteRecoveryWorkKind::SubmitShares,
                bundle_index: 0,
                proposal_id: 2,
                tx_hash: None,
                vc_tree_position: Some(42),
                share_indexes: vec![0],
            }]
        );
    }

    #[test]
    fn all_decided_true_with_confirmed_choice_and_skip() {
        let db = db_with_bundle();

        // Proposal 2: drive to Confirmed.
        confirm_vote_fixture(&db, 0, 2, 1);
        record_all_confirmed_share_fixtures(&db, 0, 2);
        db.set_ballot_intent(ROUND, 2, Decision::Choice(1), 3)
            .unwrap();

        // Proposal 1: skipped.
        db.set_ballot_intent(ROUND, 1, Decision::Skipped, 3)
            .unwrap();

        let plan = resume_plan(&db, ROUND, &[1, 2]).unwrap();

        assert!(plan.all_decided, "expected all_decided == true");
        assert!(!plan.pending_recovery, "expected pending_recovery == false");
    }
}
