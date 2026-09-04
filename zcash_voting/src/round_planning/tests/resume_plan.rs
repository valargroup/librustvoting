//! The planner's behavior, pinned end to end over a real voting database:
//! every step, flag, and error `resume_plan` reports for a round.

use std::collections::BTreeSet;

use rusqlite::named_params;

use crate::chain_submission::{
    generation_for_vote, generation_for_vote_batch, submission_identity_key,
    ChainSubmissionIdentity, ChainSubmissionTarget,
};
use crate::phases::{DelegationPhase, VotePhase};
use crate::round::{RoundParams, VotingDb};
use crate::round_planning::{blocking_prerequisite, summarize_plan_work};
use crate::session::*;
use crate::share_policy::ImmediateShareKey;
use crate::types::{EncryptedShare, NoteInfo, VotingError, MAX_PROPOSAL_ID};
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
        ChainSubmissionTarget::Vote { proposal_id } => ("vote", Some(i64::from(proposal_id)), None),
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
    db.set_ballot_intent(ROUND, 9, Decision::Choice(0), 3)
        .unwrap();

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
    assert_eq!(
        plan.next_steps,
        vec![NextStep::Delegate { bundle_index: 0 }]
    );

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
fn an_unrostered_intent_the_chain_lifecycle_owns_does_not_withhold_casting() {
    let db = db_with_bundle();
    for proposal_id in [1, 2, 3] {
        db.set_ballot_intent(ROUND, proposal_id, Decision::Choice(1), 3)
            .unwrap();
    }
    // Proposal 9 was voted and confirmed on chain, then left the roster.
    db.set_ballot_intent(ROUND, 9, Decision::Choice(1), 3)
        .unwrap();
    confirm_vote_fixture(&db, 0, 9, 1);

    let plan = resume_plan(&db, ROUND, &[1, 2, 3]).unwrap();
    assert!(
        plan.unrostered_intents.is_empty(),
        "an intent the host cannot clear is not reported as actionable: {:?}",
        plan.unrostered_intents
    );
    assert!(
        plan.next_steps
            .iter()
            .any(|step| matches!(step, NextStep::CastVote { .. })),
        "casting for the current roster must not be withheld: {:?}",
        plan.next_steps
    );
}

#[test]
fn an_unrostered_submitted_vote_is_still_advanced() {
    let db = db_with_bundle();
    db.set_ballot_intent(ROUND, 1, Decision::Choice(1), 3)
        .unwrap();
    // Proposal 9 was voted and is tracking on chain, then left the roster.
    db.set_ballot_intent(ROUND, 9, Decision::Choice(1), 3)
        .unwrap();
    crate::storage::queries::store_vote(&db.conn(), ROUND, W, 0, 9, 1, &[0xCC; 16]).unwrap();
    insert_in_flight_submission(&db, "tracking", "vote", Some(9), Some([0x5A; 32]));

    let plan = resume_plan(&db, ROUND, &[1]).unwrap();
    assert!(
        plan.next_steps.iter().any(|step| matches!(
            step,
            NextStep::AdvanceVote {
                bundle_index: 0,
                proposal_id: 9
            } | NextStep::AdvanceVoteBatch {
                bundle_index: 0,
                proposal_id: 9
            }
        )),
        "the on-chain submission for a dropped proposal must still be driven to resolution: {:?}",
        plan.next_steps
    );
}

fn db_with_two_bundles() -> VotingDb {
    let db = VotingDb::open_in_memory().unwrap();
    db.set_wallet_id(W);
    db.create_round(crate::Network::Testnet, &round_params(), None)
        .unwrap();
    db.ensure_bundles(
        ROUND,
        &[note(0), note(1), note(2), note(3), note(4), note(5)],
    )
    .unwrap();
    assert_eq!(db.delegation_phases(ROUND).unwrap().len(), 2);
    db
}

/// Proposal 9 left the roster after bundle 0's vote confirmed while
/// bundle 1's vote for it was persisted but never dispatched.
fn seed_dropped_proposal_with_mixed_siblings(db: &VotingDb) {
    db.set_ballot_intent(ROUND, 1, Decision::Choice(1), 3)
        .unwrap();
    db.set_ballot_intent(ROUND, 9, Decision::Choice(1), 3)
        .unwrap();
    confirm_vote_fixture(db, 0, 9, 1);
    crate::storage::queries::store_vote(&db.conn(), ROUND, W, 1, 9, 1, &[0xCD; 16]).unwrap();
    store_vote_recovery_fixture(db, 1, 9, 1, None);
    assert_eq!(db.vote_phase(ROUND, 1, 9).unwrap(), VotePhase::Committed);
}

#[test]
fn a_committed_vote_for_a_dropped_proposal_does_not_hold_its_bundle() {
    let db = db_with_two_bundles();
    seed_dropped_proposal_with_mixed_siblings(&db);

    let plan = resume_plan(&db, ROUND, &[1]).unwrap();
    assert!(
        plan.unrostered_intents.is_empty(),
        "{:?}",
        plan.unrostered_intents
    );
    assert!(
        plan.next_steps.iter().any(|step| matches!(
            step,
            NextStep::CastVote {
                bundle_index: 1,
                proposal_id: 1,
                ..
            }
        )),
        "bundle 1 must still be castable for the current roster: {:?}",
        plan.next_steps
    );
}

#[test]
fn a_committed_batch_with_a_dropped_member_is_retired_whole_and_recast() {
    let db = db_with_two_bundles();
    db.set_ballot_intent(ROUND, 1, Decision::Choice(0), 3)
        .unwrap();
    db.set_ballot_intent(ROUND, 9, Decision::Choice(1), 3)
        .unwrap();
    // Bundle 1's vote for proposal 9 confirmed; bundle 0 holds a
    // committed, undispatched batch {1, 9}; then 9 left the roster.
    confirm_vote_fixture(&db, 1, 9, 1);
    db.store_delegation_tx_hash(ROUND, 0, "dtx").unwrap();
    db.store_van_position(ROUND, 0, 7).unwrap();
    store_two_action_batch_recovery_fixture_for(&db, (1, 0), (9, 1));
    assert_eq!(db.vote_phase(ROUND, 0, 1).unwrap(), VotePhase::Committed);

    let plan = resume_plan(&db, ROUND, &[1]).unwrap();
    assert!(
        !plan.next_steps.iter().any(|step| matches!(
            step,
            NextStep::AdvanceVoteBatch {
                bundle_index: 0,
                ..
            }
        )),
        "the batch carrying a removed proposal must not be advanced: {:?}",
        plan.next_steps
    );
    assert!(
        plan.next_steps.iter().any(|step| matches!(
            step,
            NextStep::CastVote {
                bundle_index: 0,
                proposal_id: 1,
                choice: 0,
            }
        )),
        "the rostered member is cast again: {:?}",
        plan.next_steps
    );

    // The cast step's retirement clears both members.
    assert_eq!(
        db.retire_undispatched_votes_outside_roster(ROUND, 0, &[1])
            .unwrap(),
        vec![9]
    );
    assert_ne!(db.vote_phase(ROUND, 0, 1).unwrap(), VotePhase::Committed);
    assert_ne!(db.vote_phase(ROUND, 0, 9).unwrap(), VotePhase::Committed);
}

#[test]
fn a_batch_whose_every_member_left_the_roster_is_retired_once_and_recast_from_nothing() {
    let db = db_with_two_bundles();
    db.set_ballot_intent(ROUND, 5, Decision::Choice(0), 3)
        .unwrap();
    db.store_delegation_tx_hash(ROUND, 0, "dtx").unwrap();
    db.store_van_position(ROUND, 0, 7).unwrap();
    // An undispatched batch {1, 9} on bundle 0; both proposals then left the
    // roster, which now lists only 5.
    store_two_action_batch_recovery_fixture_for(&db, (1, 0), (9, 1));
    assert_eq!(db.vote_phase(ROUND, 0, 1).unwrap(), VotePhase::Committed);
    assert_eq!(db.vote_phase(ROUND, 0, 9).unwrap(), VotePhase::Committed);

    // The second departed member finds its batch already cleared by the
    // first; retirement must report both instead of failing the whole
    // transaction and stranding every later cast on the bundle.
    assert_eq!(
        db.retire_undispatched_votes_outside_roster(ROUND, 0, &[5])
            .unwrap(),
        vec![1, 9]
    );
    assert_ne!(db.vote_phase(ROUND, 0, 1).unwrap(), VotePhase::Committed);
    assert_ne!(db.vote_phase(ROUND, 0, 9).unwrap(), VotePhase::Committed);
    assert!(db
        .retire_undispatched_votes_outside_roster(ROUND, 0, &[5])
        .unwrap()
        .is_empty());
}

#[test]
fn retiring_undispatched_votes_outside_the_roster_leaves_siblings_alone() {
    let db = db_with_two_bundles();
    seed_dropped_proposal_with_mixed_siblings(&db);

    assert_eq!(
        db.retire_undispatched_votes_outside_roster(ROUND, 1, &[1])
            .unwrap(),
        vec![9]
    );
    assert_ne!(db.vote_phase(ROUND, 1, 9).unwrap(), VotePhase::Committed);
    assert_eq!(db.vote_phase(ROUND, 0, 9).unwrap(), VotePhase::Confirmed);
    // Idempotent, and a rostered proposal is never touched.
    assert!(db
        .retire_undispatched_votes_outside_roster(ROUND, 1, &[1])
        .unwrap()
        .is_empty());
    assert!(db
        .retire_undispatched_votes_outside_roster(ROUND, 0, &[9])
        .unwrap()
        .is_empty());
}

#[test]
fn an_unrostered_confirmed_vote_still_schedules_its_missing_shares() {
    let db = db_with_bundle();
    db.set_ballot_intent(ROUND, 1, Decision::Skipped, 3)
        .unwrap();
    // Proposal 9 was voted and confirmed, then left the roster before
    // its helper shares were delivered.
    db.set_ballot_intent(ROUND, 9, Decision::Choice(1), 3)
        .unwrap();
    confirm_vote_fixture(&db, 0, 9, 1);

    let plan = resume_plan(&db, ROUND, &[1]).unwrap();
    let share_steps = plan
        .next_steps
        .iter()
        .filter(|step| {
            matches!(
                step,
                NextStep::SubmitShares {
                    bundle_index: 0,
                    proposal_id: 9,
                    ..
                }
            )
        })
        .count();
    assert_eq!(
        share_steps, 2,
        "the confirmed vote's missing shares must still be scheduled: {:?}",
        plan.next_steps
    );
}

#[test]
fn the_plan_reports_the_persisted_immediate_share_after_its_proposal_leaves_the_roster() {
    let db = db_with_bundle();
    db.set_ballot_intent(ROUND, 1, Decision::Choice(0), 3)
        .unwrap();
    db.set_ballot_intent(ROUND, 2, Decision::Choice(1), 3)
        .unwrap();
    // Proposal 1 is the lowest choice: its confirmed vote's plan carries
    // the round-immediate designation.
    confirm_vote_fixture(&db, 0, 1, 0);
    let helpers = vec!["https://helper.example".to_string()];
    let fleet =
        crate::helper::client::HelperFleetPreflight::from_readiness(&helpers, &helpers).unwrap();
    crate::vote::CommittedVote::recover(&db, ROUND, 0, 1)
        .unwrap()
        .prepare_share_delivery(
            &db,
            crate::share_tracking::ShareDeliveryPlanningParams {
                fleet: &fleet,
                now_seconds: 10,
                vote_end_time_seconds: 100_000,
                last_moment_buffer_seconds: None,
                proposal_ids: &[1, 2],
            },
        )
        .unwrap();
    let persisted = ImmediateShareKey {
        bundle_index: 0,
        proposal_id: 1,
        share_index: 0,
    };
    assert_eq!(
        resume_plan(&db, ROUND, &[1, 2])
            .unwrap()
            .immediate_share_key,
        Some(persisted)
    );

    // The roster then drops proposal 1; the plans still designate it.
    let plan = resume_plan(&db, ROUND, &[2]).unwrap();
    assert_eq!(
        plan.immediate_share_key,
        Some(persisted),
        "the plan must report the designation delivery will execute"
    );
    assert!(!plan.immediate_share_confirmed);
}

#[test]
fn an_intent_whose_vote_the_chain_lifecycle_owns_cannot_be_cleared() {
    let db = db_with_bundle();
    db.set_ballot_intent(ROUND, 9, Decision::Choice(1), 3)
        .unwrap();
    crate::storage::queries::store_vote(&db.conn(), ROUND, W, 0, 9, 1, &[0xCC; 16]).unwrap();
    // Tracking on the chain lifecycle: no tx_hash projection yet, but the
    // signed generation may already be on chain.
    insert_in_flight_submission(&db, "tracking", "vote", Some(9), Some([0x5A; 32]));

    let error = db
        .clear_ballot_intent(ROUND, 9)
        .expect_err("a tracked vote's intent must survive");
    assert!(matches!(error, VotingError::InvalidInput { .. }), "{error}");
    assert!(error.to_string().contains("submission_managed"), "{error}");
    assert_eq!(
        db.ballot_intents(ROUND).unwrap(),
        vec![(9, Decision::Choice(1))]
    );
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

    let in_flight = summarize_plan_work(&[NextStep::AdvanceDelegation { bundle_index: 0 }], false);
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
        crate::wire::WorkflowPhaseView::from(crate::phases::WorkflowPhase::for_delegation(
            status.phase
        )),
        crate::wire::WorkflowPhaseView::SubmittedDelegation
    );
}

#[test]
fn terminal_delegation_status_marks_only_ended_phases() {
    let db = db_with_bundle();
    assert!(
        !resume_plan(&db, ROUND, &[1, 2, 3])
            .unwrap()
            .delegation_statuses[0]
            .terminal,
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
        crate::storage::queries::store_vote(&db.conn(), ROUND, W, 0, 2, 1, &[0xCC; 16]).unwrap();
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
        let delegation_identity = submission_identity_fixture(ChainSubmissionTarget::Delegation);
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
            crate::chain_submission::planning::vote_transaction_hash(&db, ROUND, 0, proposal_id)
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
    assert!(snapshot
        .votes
        .iter()
        .all(|vote| { vote.phase == VotePhase::SubmissionRejected && vote.has_commitment_bundle }));
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
        assert!(shares
            .iter()
            .all(|share| { share.bundle_index == 0 && share.proposal_id == 2 && share.confirmed }));
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
    assert!(
        !resume_plan(&db, ROUND, &[1, 2, 3])
            .unwrap()
            .has_unconfirmed_shares
    );
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
