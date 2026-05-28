//! Durable ballot intent + resumable voting-session planner.
//!
//! `resume_plan` is pure and I/O-free over the wallet's voting DB: it reports
//! the ordered remaining work for a round, built on the per-artifact phase
//! APIs in `crate::phases`. The wallet executes each step with its own
//! network/proof/sign plumbing.

use rusqlite::named_params;
use std::collections::{BTreeMap, BTreeSet};

use crate::phases::{DelegationPhase, SharePhase, VotePhase};
use crate::storage::VotingDb;
use crate::types::VotingError;

/// The voter's terminal decision for one proposal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Choice(u32),
    Skipped,
}

impl VotingDb {
    /// Record (insert or replace) the voter's decision for one proposal.
    /// Written on each selection, before any per-proposal vote artifact exists.
    pub fn set_ballot_intent(
        &self,
        round_id: &str,
        proposal_id: u32,
        decision: Decision,
    ) -> Result<(), VotingError> {
        let (skipped, choice): (i64, Option<i64>) = match decision {
            Decision::Choice(c) => (0, Some(c as i64)),
            Decision::Skipped => (1, None),
        };
        let now = now_secs();
        let conn = self.conn();
        let wallet_id = self.wallet_id();
        conn.execute(
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
        .map_err(|e| VotingError::Internal { message: format!("set_ballot_intent failed: {e}") })?;
        Ok(())
    }

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
            .map_err(|e| VotingError::Internal { message: format!("prepare ballot_intents: {e}") })?;
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
            .map_err(|e| VotingError::Internal { message: format!("query ballot_intents: {e}") })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| VotingError::Internal { message: format!("collect ballot_intents: {e}") })
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NextStep {
    Delegate { bundle_index: u32 },
    PollDelegation { bundle_index: u32 },
    CastVote { bundle_index: u32, proposal_id: u32 },
    PollVote { bundle_index: u32, proposal_id: u32 },
    ConfirmShare { bundle_index: u32, proposal_id: u32, share_index: u32 },
}

/// Derived resume state for one round.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoundPlan {
    pub round_id: String,
    /// True iff any recovery step remains (`!next_steps.is_empty()`).
    pub pending_recovery: bool,
    /// Ordered remaining recovery work.
    pub next_steps: Vec<NextStep>,
    /// Proposals still open to vote (Skipped or never decided) while the round is open.
    pub open_proposals: Vec<u32>,
    /// Informational: every proposal is either a confirmed Choice or Skipped.
    pub all_decided: bool,
}

fn step_rank(step: &NextStep) -> (u32, u32, u32, u32) {
    // Delegate and PollDelegation never coexist for one bundle, and CastVote
    // and PollVote never coexist for one (bundle, proposal), so the shared sort
    // keys are unambiguous.
    match step {
        NextStep::Delegate { bundle_index } => (*bundle_index, 0, 0, 0),
        NextStep::PollDelegation { bundle_index } => (*bundle_index, 0, 0, 0),
        NextStep::CastVote { bundle_index, proposal_id } => (*bundle_index, 1, *proposal_id, 0),
        NextStep::PollVote { bundle_index, proposal_id } => (*bundle_index, 1, *proposal_id, 0),
        NextStep::ConfirmShare { bundle_index, proposal_id, share_index } => {
            (*bundle_index, 2, *proposal_id, *share_index)
        }
    }
}

/// Build the resume plan for `round_id`.
///
/// `proposal_ids` is the round's full set of proposal ids (from the wallet's
/// round config); the crate cannot enumerate "never decided" proposals on its
/// own. The plan is a best-effort snapshot over the durable phase tables.
pub fn resume_plan(
    db: &VotingDb,
    round_id: &str,
    proposal_ids: &[u32],
) -> Result<RoundPlan, VotingError> {
    let delegation: BTreeMap<u32, DelegationPhase> =
        db.delegation_phases(round_id)?.into_iter().collect();
    let votes: BTreeMap<(u32, u32), VotePhase> = db
        .vote_phases(round_id)?
        .into_iter()
        .map(|(b, p, ph)| ((b, p), ph))
        .collect();
    let shares = db.share_phases(round_id)?;
    let intents: BTreeMap<u32, Decision> = db.ballot_intents(round_id)?.into_iter().collect();

    let bundles: Vec<u32> = delegation.keys().copied().collect();

    let mut choice_proposals: Vec<u32> = Vec::new();
    let mut open_proposals: Vec<u32> = Vec::new();
    for &pid in proposal_ids {
        match intents.get(&pid) {
            Some(Decision::Choice(_)) => choice_proposals.push(pid),
            _ => open_proposals.push(pid), // Skipped or never decided -> open, votable later
        }
    }
    choice_proposals.sort_unstable();
    open_proposals.sort_unstable();

    let mut steps: Vec<NextStep> = Vec::new();
    let mut bundles_needing_delegation: BTreeSet<u32> = BTreeSet::new();

    // Vote steps for answered proposals.
    for &pid in &choice_proposals {
        for &b in &bundles {
            match votes.get(&(b, pid)) {
                Some(VotePhase::Confirmed) => {}
                Some(VotePhase::Submitted) => {
                    steps.push(NextStep::PollVote { bundle_index: b, proposal_id: pid });
                }
                // Prepared, Committed, or no row yet -> still needs casting.
                _ => {
                    steps.push(NextStep::CastVote { bundle_index: b, proposal_id: pid });
                    bundles_needing_delegation.insert(b);
                }
            }
        }
    }

    // Delegation steps: resume any mid-flight delegation; otherwise only the
    // prerequisite for a bundle that still has a vote to cast.
    for &b in &bundles {
        match delegation.get(&b) {
            Some(DelegationPhase::Confirmed) => {}
            Some(DelegationPhase::Submitted) => {
                steps.push(NextStep::PollDelegation { bundle_index: b });
            }
            // Prepared / PcztBuilt / Proved: still needs the delegate flow.
            _ => {
                if bundles_needing_delegation.contains(&b) {
                    steps.push(NextStep::Delegate { bundle_index: b });
                }
            }
        }
    }

    // Confirm already-submitted helper shares.
    for (b, p, s, phase) in shares {
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

    let all_decided = proposal_ids.iter().all(|&pid| match intents.get(&pid) {
        Some(Decision::Skipped) => true,
        Some(Decision::Choice(_)) => {
            !bundles.is_empty()
                && bundles
                    .iter()
                    .all(|&b| votes.get(&(b, pid)) == Some(&VotePhase::Confirmed))
        }
        None => false,
    });

    Ok(RoundPlan {
        round_id: round_id.to_string(),
        pending_recovery: !steps.is_empty(),
        next_steps: steps,
        open_proposals,
        all_decided,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::round::RoundParams;
    use crate::types::NoteInfo;

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
            nullifier: vec![0x02; 32],
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
        db.create_round(&round_params()).unwrap();
        db.ensure_bundles(ROUND, &[note(0)]).unwrap();
        db
    }

    #[test]
    fn ballot_intent_round_trip() {
        let db = db_with_bundle();
        db.set_ballot_intent(ROUND, 1, Decision::Choice(0)).unwrap();
        db.set_ballot_intent(ROUND, 2, Decision::Skipped).unwrap();
        db.set_ballot_intent(ROUND, 1, Decision::Choice(3)).unwrap(); // upsert

        let intents = db.ballot_intents(ROUND).unwrap();
        assert_eq!(intents, vec![(1, Decision::Choice(3)), (2, Decision::Skipped)]);
    }

    #[test]
    fn fresh_round_with_no_choices_has_no_recovery() {
        let db = db_with_bundle();
        let plan = resume_plan(&db, ROUND, &[1, 2, 3]).unwrap();
        assert!(!plan.pending_recovery);
        assert!(plan.next_steps.is_empty());
        assert_eq!(plan.open_proposals, vec![1, 2, 3]);
        assert!(!plan.all_decided);
    }

    #[test]
    fn answered_but_uncast_proposal_yields_cast_then_delegate_prereq() {
        let db = db_with_bundle();
        db.set_ballot_intent(ROUND, 2, Decision::Choice(1)).unwrap();
        let plan = resume_plan(&db, ROUND, &[1, 2, 3]).unwrap();
        assert!(plan.pending_recovery);
        // Bundle 0 is only Prepared, and proposal 2 needs casting on it, so the
        // delegate prerequisite is emitted, ordered before the cast.
        assert_eq!(
            plan.next_steps,
            vec![
                NextStep::Delegate { bundle_index: 0 },
                NextStep::CastVote { bundle_index: 0, proposal_id: 2 },
            ]
        );
        assert_eq!(plan.open_proposals, vec![1, 3]);
    }

    #[test]
    fn submitted_but_unconfirmed_vote_yields_poll() {
        let db = db_with_bundle();
        db.set_ballot_intent(ROUND, 2, Decision::Choice(1)).unwrap();
        db.store_delegation_tx_hash(ROUND, 0, "dtx").unwrap();
        db.store_van_position(ROUND, 0, 7).unwrap();
        crate::storage::queries::store_vote(&db.conn(), ROUND, W, 0, 2, 1, &[0xCC; 16]).unwrap();
        db.store_vote_tx_hash(ROUND, 0, 2, "vtx").unwrap();
        db.mark_vote_submitted(ROUND, 0, 2).unwrap();
        let plan = resume_plan(&db, ROUND, &[1, 2, 3]).unwrap();
        assert_eq!(plan.next_steps, vec![NextStep::PollVote { bundle_index: 0, proposal_id: 2 }]);
    }

    #[test]
    fn skipped_proposal_is_open_not_recovery() {
        let db = db_with_bundle();
        db.set_ballot_intent(ROUND, 1, Decision::Skipped).unwrap();
        let plan = resume_plan(&db, ROUND, &[1, 2, 3]).unwrap();
        assert!(!plan.pending_recovery);
        assert!(plan.open_proposals.contains(&1));
    }

    #[test]
    fn midflight_delegation_is_recovery_without_votes() {
        let db = db_with_bundle();
        db.store_delegation_tx_hash(ROUND, 0, "dtx").unwrap(); // Submitted, no VAN
        let plan = resume_plan(&db, ROUND, &[1]).unwrap();
        assert_eq!(plan.next_steps, vec![NextStep::PollDelegation { bundle_index: 0 }]);
    }

    #[test]
    fn multi_bundle_orders_steps_by_bundle() {
        let db = VotingDb::open_in_memory().unwrap();
        db.set_wallet_id(W);
        db.create_round(&round_params()).unwrap();
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

        db.set_ballot_intent(ROUND, 2, Decision::Choice(1)).unwrap();
        let plan = resume_plan(&db, ROUND, &[1, 2, 3]).unwrap();

        // Bundle-primary ordering: all of bundle 0's steps come before bundle 1's.
        // Within a bundle, Delegate (prereq) precedes CastVote.
        assert_eq!(
            plan.next_steps,
            vec![
                NextStep::Delegate { bundle_index: 0 },
                NextStep::CastVote { bundle_index: 0, proposal_id: 2 },
                NextStep::Delegate { bundle_index: 1 },
                NextStep::CastVote { bundle_index: 1, proposal_id: 2 },
            ]
        );
    }

    #[test]
    fn delegate_suppressed_when_vote_confirmed() {
        let db = db_with_bundle();
        // Drive proposal 2 on bundle 0 to VotePhase::Confirmed:
        // store_vote → store_commitment_bundle → store_vote_tx_hash → mark_vote_submitted.
        crate::storage::queries::store_vote(&db.conn(), ROUND, W, 0, 2, 1, &[0xCC; 16]).unwrap();
        crate::storage::queries::store_commitment_bundle(
            &db.conn(),
            ROUND,
            W,
            0,
            2,
            r#"{"format":"zcash_voting_vote_recovery_v1"}"#,
            42,
        )
        .unwrap();
        db.store_vote_tx_hash(ROUND, 0, 2, "tx").unwrap();
        db.mark_vote_submitted(ROUND, 0, 2).unwrap();

        // Bundle 0 delegation is still only Prepared — but the vote is Confirmed,
        // so no Delegate step should appear and the plan has no work left.
        db.set_ballot_intent(ROUND, 2, Decision::Choice(1)).unwrap();
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
            plan.next_steps
                .contains(&NextStep::ConfirmShare { bundle_index: 0, proposal_id: 2, share_index: 0 }),
            "expected ConfirmShare in steps, got: {:?}",
            plan.next_steps
        );
    }

    #[test]
    fn all_decided_true_with_confirmed_choice_and_skip() {
        let db = db_with_bundle();

        // Proposal 2: drive to Confirmed.
        crate::storage::queries::store_vote(&db.conn(), ROUND, W, 0, 2, 1, &[0xCC; 16]).unwrap();
        crate::storage::queries::store_commitment_bundle(
            &db.conn(),
            ROUND,
            W,
            0,
            2,
            r#"{"format":"zcash_voting_vote_recovery_v1"}"#,
            42,
        )
        .unwrap();
        db.store_vote_tx_hash(ROUND, 0, 2, "tx").unwrap();
        db.mark_vote_submitted(ROUND, 0, 2).unwrap();
        db.set_ballot_intent(ROUND, 2, Decision::Choice(1)).unwrap();

        // Proposal 1: skipped.
        db.set_ballot_intent(ROUND, 1, Decision::Skipped).unwrap();

        let plan = resume_plan(&db, ROUND, &[1, 2]).unwrap();

        assert!(plan.all_decided, "expected all_decided == true");
        assert!(!plan.pending_recovery, "expected pending_recovery == false");
    }
}
