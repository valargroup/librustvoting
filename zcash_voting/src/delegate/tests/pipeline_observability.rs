use super::*;
use crate::delegation_pipeline::{DelegationPipeline, SqliteWalletDbOpener};
use std::sync::Arc;

struct NoPirTransport;
impl pir_client::Transport for NoPirTransport {
    fn get<'a>(&'a self, _: &'a str) -> pir_client::TransportFuture<'a> {
        panic!("cached proof must not contact PIR")
    }
    fn post<'a>(&'a self, _: &'a str, _: Vec<u8>) -> pir_client::TransportFuture<'a> {
        panic!("cached proof must not contact PIR")
    }
}

#[test]
fn pipeline_cached_proof_reports_reused_at_both_boundaries() {
    let (db, round_params, hotkey, prepared) = prepared_wallet_delegation_fixture();
    let db = Arc::new(db);
    {
        let conn = db.conn();
        let witnesses: Vec<_> = prepared
            .bundle_note_infos
            .iter()
            .map(|note| crate::WitnessData {
                note_commitment: note.commitment.clone(),
                position: note.position,
                auth_path: vec![vec![0; 32]; 32],
                root: round_params.nc_root.clone(),
            })
            .collect();
        crate::storage::queries::store_witnesses(
            &conn,
            &prepared.round_id,
            &db.wallet_id(),
            0,
            &witnesses,
        )
        .unwrap();
    }
    // Keep this connection alive while the pipeline opens independent read handles.
    let path = "file:observed-pipeline-cache?mode=memory&cache=shared";
    let mut conn = Connection::open(path).unwrap();
    let (account, fvk) = setup_test_account_for_network(&mut conn, Network::Regtest);
    let account_ref = account_internal_id(&conn, &account);
    let height = REGTEST_NU6_3_SNAPSHOT_HEIGHT as u32;
    for (tag, mined_height, multiple, position) in [
        (1, height - 2, 1, 7),
        (2, height - 2, 2, 3),
        (3, height + 4, 3, 11),
    ] {
        insert_ironwood_note(
            &conn,
            account_ref,
            &fvk,
            tag,
            mined_height,
            crate::governance::BALLOT_DIVISOR * multiple,
            position,
        );
    }
    conn.execute("DELETE FROM scan_queue", []).unwrap();
    conn.execute(
        "INSERT INTO scan_queue (block_range_start, block_range_end, priority) VALUES (0, ?1, 10)",
        [height + 1],
    )
    .unwrap();
    for block_height in 0..=height {
        conn.execute("INSERT INTO blocks (height, hash, time, sapling_tree, sapling_commitment_tree_size, orchard_commitment_tree_size, sapling_output_count, orchard_action_count) VALUES (?1, ?2, ?1, ?3, 0, 0, 0, 0)", params![block_height, [block_height as u8; 32], Vec::<u8>::new()]).unwrap();
    }
    let lwd = DelegationLwdInputs {
        network: Network::Regtest,
        round_params,
        resolved_round_name: prepared.round_name.clone(),
        anchor_tree_state_bytes: vec![],
        branch_id_provider: prepared.branch_id_provider.clone(),
    };
    let pipeline = DelegationPipeline::new(
        Arc::clone(&db),
        SqliteWalletDbOpener::new(path, Network::Regtest),
        lwd,
        &account.expose_uuid().to_string(),
        Some(hotkey),
        crate::BundlePolicy::default(),
        None,
    )
    .unwrap();
    let eligibility =
        pipeline.eligibility_with_report(Some(crate::ObservabilityOptions::default()));
    assert!(eligibility.result.is_ok());
    assert_eq!(
        eligibility.observability.unwrap().outcome,
        crate::ObservationOutcome::Succeeded
    );
    let keystone =
        pipeline.keystone_request_with_report(0, Some(crate::ObservabilityOptions::default()));
    assert!(keystone.result.is_ok(), "{:?}", keystone.result);
    assert_eq!(
        keystone.observability.unwrap().outcome,
        crate::ObservationOutcome::Succeeded
    );
    {
        let conn = db.conn();
        // Seed the durable proof boundary, as in the proof-coordination fixtures.
        // This test exercises reuse and never asks the cryptographic prover to run.
        crate::storage::queries::store_proof(
            &conn,
            &prepared.round_id,
            &db.wallet_id(),
            0,
            &[0xA1; 96],
        )
        .unwrap();
    }
    let pir = crate::pir::PirFleet::new(
        &[],
        crate::config::PirLayout {
            pir_depth: pir_types::COMPILED_PIR_LAYOUT.pir_depth as u32,
            tier0_layers: pir_types::COMPILED_PIR_LAYOUT.tier0_layers as u32,
            tier1_layers: pir_types::COMPILED_PIR_LAYOUT.tier1_layers as u32,
            poly_len: pir_types::DEFAULT_YPIR_POLY_LEN as u32,
        },
        Arc::new(NoPirTransport),
    )
    .unwrap();
    let report = pipeline.ensure_proof_with_report(
        0,
        &pir,
        &crate::types::NoopProgressReporter,
        Some(crate::ObservabilityOptions::default()),
    );
    assert_eq!(report.result.unwrap(), DelegationProofStatus::Reused);
    let diagnostics = report.observability.unwrap();
    assert_eq!(diagnostics.outcome, crate::ObservationOutcome::Reused);
    let stage = diagnostics
        .records
        .iter()
        .find(|record| record.stage.as_ref() == "delegation::ensure_proof")
        .unwrap();
    assert_eq!(stage.outcome, crate::ObservationOutcome::Reused);
    assert_eq!(stage.attribution.bundle_index, Some(0));
}
