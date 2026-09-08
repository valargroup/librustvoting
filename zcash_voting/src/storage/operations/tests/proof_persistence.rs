//! Reset and replacement ordering at the proof persistence boundary.

use super::{fixtures::*, *};

struct InFlightProof {
    prover: VotingDb,
    other_connection: VotingDb,
    notes: [NoteInfo; 1],
    keys: DelegationKeys,
    setup: GovernancePczt,
}

fn with_in_flight_proof(check: impl FnOnce(&InFlightProof)) {
    let (_, note, fvk_bytes) = ironwood_setup_fixture();
    let path = std::env::temp_dir().join(format!("proof-reset-{}.sqlite", uuid::Uuid::new_v4()));
    let prover = VotingDb::open(path.to_str().unwrap()).unwrap();
    prover.set_wallet_id(W);
    prover
        .init_round(Network::Regtest, &test_params_nu6_3(), None)
        .unwrap();
    let notes = [note];
    prover.ensure_bundles(ROUND_ID, &notes).unwrap();
    let keys = keys_for_hotkey_byte(&fvk_bytes, 0x43);
    let setup = prover
        .build_governance_pczt(ROUND_ID, 0, &notes, &keys, nu6_3_branch_id())
        .unwrap();
    let other_connection = VotingDb::open(path.to_str().unwrap()).unwrap();
    other_connection.set_wallet_id(W);
    assert!(!prover.shares_connection_with(&other_connection));
    let fixture = InFlightProof {
        prover,
        other_connection,
        notes,
        keys,
        setup,
    };
    check(&fixture);
    drop(fixture);
    std::fs::remove_file(path).unwrap();
}

// Exercise the real post-proving transaction without expensive Halo2 work.
fn proof_for_setup(setup: &GovernancePczt, marker: u8) -> DelegationProofResult {
    DelegationProofResult {
        proof: vec![marker; 96],
        public_inputs: Vec::new(),
        nf_signed: setup.nf_signed.clone(),
        cmx_new: setup.cmx_new.clone(),
        gov_nullifiers: setup.gov_nullifiers.clone(),
        van_comm: setup.van.clone(),
        rk: setup.rk.clone(),
    }
}

#[test]
fn a_proof_finishing_after_reset_cannot_restore_partial_setup() {
    with_in_flight_proof(|fixture| {
        let proof = proof_for_setup(&fixture.setup, 0xA0);
        // The prover already captured these inputs. Another connection resets
        // the unsigned setup before the completed proof reaches persistence.
        crate::precompute::reset_voting_session_state(&fixture.other_connection, ROUND_ID).unwrap();
        let phase_before = fixture.prover.get_round_state(ROUND_ID).unwrap().phase;
        let mut conn = fixture.prover.conn();
        let error = persist_delegation_proof_result(
            &mut conn,
            ROUND_ID,
            W,
            0,
            &fixture.setup.pczt_sighash,
            &proof,
        )
        .unwrap_err();
        assert!(
            matches!(error, VotingError::InvalidInput { message } if message.contains("no pczt_sighash"))
        );
        let (proof_count, setup_cleared): (i64, bool) = conn
            .query_row(
                "SELECT (SELECT COUNT(*) FROM proofs),
                        pczt_sighash IS NULL AND delegation_pczt IS NULL AND rk IS NULL
                 FROM bundles",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(proof_count, 0);
        assert!(setup_cleared);
        drop(conn);
        assert_eq!(
            fixture.prover.get_round_state(ROUND_ID).unwrap().phase,
            phase_before
        );
        // A retry can still build a complete setup and persist its proof.
        let retry = fixture
            .prover
            .build_governance_pczt(
                ROUND_ID,
                0,
                &fixture.notes,
                &fixture.keys,
                nu6_3_branch_id(),
            )
            .unwrap();
        persist_delegation_proof_result(
            &mut fixture.prover.conn(),
            ROUND_ID,
            W,
            0,
            &retry.pczt_sighash,
            &proof_for_setup(&retry, 0xB0),
        )
        .unwrap();
    });
}

#[test]
fn a_proof_for_replaced_setup_cannot_overwrite_the_replacement_proof() {
    with_in_flight_proof(|fixture| {
        let replacement_keys = keys_for_hotkey_byte(&fixture.keys.fvk_bytes, 0x44);
        let replacement = fixture
            .other_connection
            .build_governance_pczt(
                ROUND_ID,
                0,
                &fixture.notes,
                &replacement_keys,
                nu6_3_branch_id(),
            )
            .unwrap();
        let replacement_proof = proof_for_setup(&replacement, 0xB0);
        persist_delegation_proof_result(
            &mut fixture.other_connection.conn(),
            ROUND_ID,
            W,
            0,
            &replacement.pczt_sighash,
            &replacement_proof,
        )
        .unwrap();
        let mut conn = fixture.prover.conn();
        let error = persist_delegation_proof_result(
            &mut conn,
            ROUND_ID,
            W,
            0,
            &fixture.setup.pczt_sighash,
            &proof_for_setup(&fixture.setup, 0xA0),
        )
        .unwrap_err();
        assert!(
            matches!(error, VotingError::InvalidInput { message } if message.contains("setup changed during proof generation"))
        );
        let retained: (Vec<u8>, Vec<u8>) = conn.query_row(
            "SELECT b.delegation_pczt, p.proof FROM bundles b JOIN proofs p USING (round_id, wallet_id, bundle_index)",
            [], |row| Ok((row.get(0)?, row.get(1)?)),
        ).unwrap();
        assert_eq!(retained, (replacement.pczt_bytes, replacement_proof.proof));
    });
}

#[test]
fn a_proof_finishing_before_reset_keeps_its_complete_setup() {
    with_in_flight_proof(|fixture| {
        let proof = proof_for_setup(&fixture.setup, 0xA0);
        persist_delegation_proof_result(
            &mut fixture.prover.conn(),
            ROUND_ID,
            W,
            0,
            &fixture.setup.pczt_sighash,
            &proof,
        )
        .unwrap();
        crate::precompute::reset_voting_session_state(&fixture.other_connection, ROUND_ID).unwrap();
        let retained =
            queries::load_delegation_pczt_fields(&fixture.prover.conn(), ROUND_ID, W, 0).unwrap();
        assert_eq!(retained.0, fixture.setup.pczt_bytes);
        assert_eq!(retained.1, fixture.setup.pczt_sighash);
        assert_eq!(retained.2, fixture.setup.rk);
        assert!(fixture
            .prover
            .delegation_phase(ROUND_ID, 0)
            .unwrap()
            .has_persisted_proof());
    });
}
