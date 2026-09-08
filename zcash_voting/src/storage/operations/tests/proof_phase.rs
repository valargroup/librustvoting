//! Round-phase behavior when bundles finish delegation proving out of order.

use super::*;

#[test]
fn a_late_bundle_proof_preserves_vote_ready_round_phase() {
    let db = test_db();
    db.init_round(Network::Testnet, &test_params(), None)
        .unwrap();

    let delegation_proof = DelegationProofResult {
        proof: vec![0xA0; 96],
        public_inputs: Vec::new(),
        nf_signed: vec![0x30; 32],
        cmx_new: vec![0x40; 32],
        gov_nullifiers: vec![vec![0x20; 32]; BUNDLE_NOTE_SLOTS],
        van_comm: vec![0x50; 32],
        rk: vec![0x10; 32],
    };
    {
        let conn = db.conn();
        queries::insert_bundle(&conn, ROUND_ID, W, 0, &[0]).unwrap();
        queries::store_delegation_data_with_pczt_fields(
            &conn,
            ROUND_ID,
            W,
            0,
            &[0x01; 32],
            &[],
            &[0x02; 32],
            &[],
            &delegation_proof.nf_signed,
            &delegation_proof.cmx_new,
            &[0x03; 32],
            &[0x04; 32],
            &[0x05; 32],
            &delegation_proof.van_comm,
            1,
            0,
            &[],
            &[0x06; 32],
            &crate::tx1::placeholder_tx1_effects(),
            &[],
            &delegation_proof.rk,
            &delegation_proof.gov_nullifiers,
        )
        .unwrap();
    }
    db.advance_round_phase(ROUND_ID, RoundPhase::VoteReady)
        .unwrap();

    {
        let mut conn = db.conn();
        persist_delegation_proof_result(&mut conn, ROUND_ID, W, 0, &[0x06; 32], &delegation_proof)
            .unwrap();
        let stored: (Vec<u8>, i64) = conn
            .query_row(
                "SELECT proof, success FROM proofs
                 WHERE round_id = ?1 AND wallet_id = ?2 AND bundle_index = 0",
                rusqlite::params![ROUND_ID, W],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(stored, (delegation_proof.proof, 1));
    }
    assert_eq!(
        db.get_round_state(ROUND_ID).unwrap().phase,
        RoundPhase::VoteReady
    );
}

/// A round at `phase` for the helper's calls below.
fn round_at(phase: RoundPhase) -> VotingDb {
    let db = test_db();
    db.init_round(Network::Testnet, &test_params(), None)
        .unwrap();
    if phase != RoundPhase::Initialized {
        db.advance_round_phase(ROUND_ID, phase).unwrap();
    }
    db
}

#[test]
fn advancing_to_at_least_raises_a_lower_phase() {
    let db = round_at(RoundPhase::HotkeyGenerated);

    queries::advance_round_phase_to_at_least(&db.conn(), ROUND_ID, W, RoundPhase::DelegationProved)
        .unwrap();

    assert_eq!(
        db.get_round_state(ROUND_ID).unwrap().phase,
        RoundPhase::DelegationProved
    );
}

#[test]
fn advancing_to_at_least_leaves_a_later_phase_alone() {
    let db = round_at(RoundPhase::VoteReady);

    queries::advance_round_phase_to_at_least(&db.conn(), ROUND_ID, W, RoundPhase::DelegationProved)
        .unwrap();

    assert_eq!(
        db.get_round_state(ROUND_ID).unwrap().phase,
        RoundPhase::VoteReady
    );
}

#[test]
fn advancing_to_at_least_is_idempotent_at_the_same_phase() {
    let db = round_at(RoundPhase::DelegationProved);

    for _ in 0..2 {
        queries::advance_round_phase_to_at_least(
            &db.conn(),
            ROUND_ID,
            W,
            RoundPhase::DelegationProved,
        )
        .unwrap();
    }

    assert_eq!(
        db.get_round_state(ROUND_ID).unwrap().phase,
        RoundPhase::DelegationProved
    );
}

#[test]
fn advancing_to_at_least_reports_an_unknown_round() {
    let db = round_at(RoundPhase::Initialized);

    let error = queries::advance_round_phase_to_at_least(
        &db.conn(),
        "no-such-round",
        W,
        RoundPhase::DelegationProved,
    )
    .unwrap_err();

    // A missing round is a caller mistake, not a phase already satisfied.
    assert!(
        matches!(error, VotingError::InvalidInput { ref message } if message.contains("no-such-round")),
        "unexpected error: {error:?}"
    );
}

#[test]
fn advance_round_phase_still_rejects_a_regression() {
    // The at-least helper accepts a later phase; the ordinary transition must
    // keep rejecting one, so a caller that has lost track of the lifecycle is
    // still told rather than silently ignored.
    let db = round_at(RoundPhase::VoteReady);

    let error = queries::advance_round_phase(&db.conn(), ROUND_ID, W, RoundPhase::DelegationProved)
        .unwrap_err();

    assert!(
        matches!(error, VotingError::InvalidInput { ref message } if message.contains("refusing to regress")),
        "unexpected error: {error:?}"
    );
}
