use super::*;

#[test]
fn preparation_keeps_selection_and_failed_witness_work_in_one_report() {
    let mut connection = Connection::open_in_memory().unwrap();
    let (account_uuid, fvk) = setup_test_account_for_network(&mut connection, Network::Regtest);
    let account_ref = account_internal_id(&connection, &account_uuid);
    insert_ironwood_note(
        &connection,
        account_ref,
        &fvk,
        1,
        8,
        crate::governance::BALLOT_DIVISOR,
        0,
    );
    connection.execute("DELETE FROM scan_queue", []).unwrap();
    connection.execute("INSERT INTO scan_queue (block_range_start, block_range_end, priority) VALUES (0, 11, 10)", []).unwrap();
    for height in 0u32..=10 {
        connection.execute(
            "INSERT INTO blocks (height, hash, time, sapling_tree, sapling_commitment_tree_size, orchard_commitment_tree_size, sapling_output_count, orchard_action_count) VALUES (?1, ?2, ?1, ?3, 0, 0, 0, 0)",
            rusqlite::params![height, [height as u8; 32], Vec::<u8>::new()],
        ).unwrap();
    }
    let wallet = WalletDb::from_connection(
        &connection,
        Network::Regtest,
        SystemClock,
        voting_crypto_deps::rand::rngs::OsRng,
    );
    let db = VotingDb::open_in_memory().unwrap();
    db.set_wallet_id("observed-preparation");
    let hotkey = test_regtest_voting_hotkey();
    let round_id = "01".repeat(32);

    let report = prepare_delegation_bundle_with_report(
        &db,
        &wallet,
        PrepareDelegationBundleParams {
            lwd: DelegationLwdInputs {
                network: Network::Regtest,
                round_params: crate::VotingRoundParams {
                    vote_round_id: round_id.clone(),
                    snapshot_height: 10,
                    ea_pk: vec![1; 32],
                    nc_root: vec![2; 32],
                    nullifier_imt_root: vec![3; 32],
                },
                resolved_round_name: "Observed round".into(),
                // Witness validation must fail after wallet selection, retaining
                // both phases in the same diagnostic snapshot.
                anchor_tree_state_bytes: vec![0xAA],
                branch_id_provider: LightwalletdBranchIdProvider::resolved(u32::from(
                    BranchId::Nu6_3,
                )),
            },
            session_json: None,
            account_uuid: &account_uuid.expose_uuid().to_string(),
            voting_hotkey: &hotkey,
            bundle_index: 0,
            bundle_policy: crate::BundlePolicy::default(),
        },
        Some(crate::ObservabilityOptions::default()),
    );
    assert!(report.result.is_err());
    let diagnostics = report.observability.unwrap();
    assert_eq!(diagnostics.round_id.as_deref(), Some(round_id.as_str()));
    for expected in [
        "select_snapshot_note_infos",
        "ensure_witnesses",
        "note_witnesses",
    ] {
        assert!(
            diagnostics
                .records
                .iter()
                .any(|record| record.stage.ends_with(expected)),
            "missing {expected}: {:?}",
            diagnostics.records
        );
    }
    assert_eq!(diagnostics.outcome, crate::ObservationOutcome::Failed);
    assert!(diagnostics
        .records
        .iter()
        .skip(1)
        .all(|record| record.parent_id.is_some()));
}
