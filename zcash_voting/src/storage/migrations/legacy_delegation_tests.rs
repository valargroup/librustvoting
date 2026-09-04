use super::tests::*;
use super::*;
use crate::storage::queries;

fn valid_keystone_signature(sighash: &[u8; 32]) -> ([u8; 32], [u8; 64]) {
    use crate::backend::{
        orchard::{
            keys::{SpendAuthorizingKey, SpendingKey},
            primitives::redpallas::{SpendAuth, VerificationKey},
        },
        pasta_curves::pallas,
        zcash_keys::keys::UnifiedSpendingKey,
    };
    use zcash_protocol::consensus::TEST_NETWORK;
    use zip32::AccountId;

    let usk =
        UnifiedSpendingKey::from_seed(&TEST_NETWORK, &[0x42; 64], AccountId::try_from(0).unwrap())
            .unwrap();
    let sk: SpendingKey = *usk.orchard();
    let ask = SpendAuthorizingKey::from(&sk);
    let randomized_signing_key = ask.randomize(&pallas::Scalar::from(7));
    let rk: [u8; 32] = (&VerificationKey::<SpendAuth>::from(&randomized_signing_key)).into();
    let signature = randomized_signing_key.sign(voting_crypto_deps::rand::rngs::OsRng, sighash);
    (rk, (&signature).into())
}

fn store_legacy_delegation_setup(conn: &Connection, bundle_index: u32) {
    conn.execute(
        "UPDATE bundles
         SET van_comm_rand = ?1,
             dummy_nullifiers = ?2,
             rho_signed = ?3,
             padded_note_data = ?4,
             nf_signed = ?5,
             cmx_new = ?6,
             alpha = ?7,
             rseed_signed = ?8,
             rseed_output = ?9,
             gov_comm = ?10,
             total_note_value = 1,
             address_index = 0,
             padded_note_secrets = ?11,
             pczt_sighash = ?12,
             tx1_effects = ?13,
             rk = ?14,
             gov_nullifiers_blob = ?15
         WHERE round_id = '1111111111111111111111111111111111111111111111111111111111111111'
           AND wallet_id = 'wallet'
           AND bundle_index = ?16",
        rusqlite::params![
            vec![0x01_u8; 32],
            Vec::<u8>::new(),
            vec![0x02_u8; 32],
            Vec::<u8>::new(),
            vec![0x03_u8; 32],
            vec![0x04_u8; 32],
            vec![0x05_u8; 32],
            vec![0x06_u8; 32],
            vec![0x07_u8; 32],
            vec![0x08_u8; 32],
            Vec::<u8>::new(),
            vec![0x09_u8; 32],
            crate::tx1::placeholder_tx1_effects(),
            vec![0x0A_u8; 32],
            vec![0x0B_u8; 32 * crate::governance::BUNDLE_NOTE_SLOTS],
            bundle_index,
        ],
    )
    .unwrap();
}

#[test]
fn migrate_v17_adds_pczt_then_reconciles_legacy_proof() {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(&v17_schema()).unwrap();
    queries::insert_round(
        &conn,
        "wallet",
        crate::Network::Testnet,
        &test_params(),
        None,
    )
    .unwrap();
    queries::insert_bundle(
        &conn,
        "1111111111111111111111111111111111111111111111111111111111111111",
        "wallet",
        0,
        &[1],
    )
    .unwrap();
    conn.execute(
        "UPDATE bundles
         SET van_comm_rand = X'AA', pczt_sighash = X'BB', rk = X'CC'
         WHERE round_id = '1111111111111111111111111111111111111111111111111111111111111111' AND wallet_id = 'wallet' AND bundle_index = 0",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO proofs
             (round_id, wallet_id, bundle_index, proof, success, created_at)
         VALUES ('1111111111111111111111111111111111111111111111111111111111111111', 'wallet', 0, X'DD', 1, 1)",
        [],
    )
    .unwrap();
    queries::store_keystone_signature(
        &conn,
        "1111111111111111111111111111111111111111111111111111111111111111",
        "wallet",
        0,
        &[0x11; 64],
        &[0x12; 32],
        &[0x13; 32],
    )
    .unwrap();
    conn.pragma_update(None, "user_version", 17).unwrap();

    migrate(&mut conn).unwrap();

    assert!(table_columns(&conn, "bundles").contains(&"delegation_pczt".to_string()));
    let preserved_setup: (Vec<u8>, Vec<u8>, Vec<u8>, Option<Vec<u8>>) = conn
        .query_row(
            "SELECT van_comm_rand, pczt_sighash, rk, delegation_pczt
             FROM bundles
             WHERE round_id = '1111111111111111111111111111111111111111111111111111111111111111'
               AND wallet_id = 'wallet'
               AND bundle_index = 0",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(preserved_setup, (vec![0xAA], vec![0xBB], vec![0xCC], None));
    let preserved_proof: (Vec<u8>, i64) = conn
        .query_row(
            "SELECT proof, success
             FROM proofs
             WHERE round_id = '1111111111111111111111111111111111111111111111111111111111111111'
               AND wallet_id = 'wallet'
               AND bundle_index = 0",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(preserved_proof, (vec![0xDD], 0));
    assert!(queries::get_keystone_signatures(
        &conn,
        "1111111111111111111111111111111111111111111111111111111111111111",
        "wallet"
    )
    .unwrap()
    .is_empty());
}

#[test]
fn migrate_v21_repairs_legacy_setup_proofs_and_signatures() {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(include_str!("001_init.sql")).unwrap();
    queries::insert_round(
        &conn,
        "wallet",
        crate::Network::Testnet,
        &test_params(),
        None,
    )
    .unwrap();
    for bundle_index in 0..4 {
        queries::insert_bundle(
            &conn,
            "1111111111111111111111111111111111111111111111111111111111111111",
            "wallet",
            bundle_index,
            &[bundle_index as u64],
        )
        .unwrap();
        store_legacy_delegation_setup(&conn, bundle_index);
    }

    // Bundle 0 has an invalid signature for matching setup. Once that
    // signature is removed, its proofless setup is safe to rebuild.
    queries::store_keystone_signature(
        &conn,
        "1111111111111111111111111111111111111111111111111111111111111111",
        "wallet",
        0,
        &[0; 64],
        &[0x09; 32],
        &[0x0A; 32],
    )
    .unwrap();

    // Bundle 1 may have reached the chain before its hash was recorded.
    // Demote the proof, but retain its bytes and setup for reconciliation.
    queries::store_proof(
        &conn,
        "1111111111111111111111111111111111111111111111111111111111111111",
        "wallet",
        1,
        &[0xA1; 96],
    )
    .unwrap();

    // Bundle 2 models proof A followed by setup B and a valid signature for
    // B. Without the original PCZT, the proof cannot be bound to setup B.
    queries::store_proof(
        &conn,
        "1111111111111111111111111111111111111111111111111111111111111111",
        "wallet",
        2,
        &[0xA2; 96],
    )
    .unwrap();
    let signed_sighash = [0x09; 32];
    let (signed_rk, signed_signature) = valid_keystone_signature(&signed_sighash);
    conn.execute(
        "UPDATE bundles SET rk = ?1
         WHERE round_id = '1111111111111111111111111111111111111111111111111111111111111111'
           AND wallet_id = 'wallet'
           AND bundle_index = 2",
        [signed_rk.as_slice()],
    )
    .unwrap();
    queries::store_keystone_signature(
        &conn,
        "1111111111111111111111111111111111111111111111111111111111111111",
        "wallet",
        2,
        &signed_signature,
        &signed_sighash,
        &signed_rk,
    )
    .unwrap();

    // Submitted state is immutable migration evidence and stays untouched.
    conn.execute(
        "UPDATE bundles SET delegation_tx_hash = 'submitted'
         WHERE round_id = '1111111111111111111111111111111111111111111111111111111111111111'
           AND wallet_id = 'wallet'
           AND bundle_index = 3",
        [],
    )
    .unwrap();
    conn.pragma_update(None, "user_version", 21).unwrap();

    migrate(&mut conn).unwrap();

    let setup_presence = |bundle_index| {
        conn.query_row(
            "SELECT pczt_sighash IS NOT NULL,
                    rk IS NOT NULL,
                    delegation_pczt IS NOT NULL
             FROM bundles
             WHERE round_id = '1111111111111111111111111111111111111111111111111111111111111111'
               AND wallet_id = 'wallet'
               AND bundle_index = ?1",
            [bundle_index],
            |row| {
                Ok((
                    row.get::<_, i64>(0)? != 0,
                    row.get::<_, i64>(1)? != 0,
                    row.get::<_, i64>(2)? != 0,
                ))
            },
        )
        .unwrap()
    };
    assert_eq!(setup_presence(0), (false, false, false));
    assert_eq!(setup_presence(1), (true, true, false));
    assert_eq!(setup_presence(2), (true, true, false));
    assert_eq!(setup_presence(3), (true, true, false));

    for (bundle_index, expected_proof) in [(1, 0xA1), (2, 0xA2)] {
        let repaired_proof: (Vec<u8>, i64) = conn
            .query_row(
                "SELECT proof, success FROM proofs
                 WHERE round_id = '1111111111111111111111111111111111111111111111111111111111111111'
                   AND wallet_id = 'wallet'
                   AND bundle_index = ?1",
                [bundle_index],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(repaired_proof, (vec![expected_proof; 96], 0));
    }

    let signatures = queries::get_keystone_signatures(
        &conn,
        "1111111111111111111111111111111111111111111111111111111111111111",
        "wallet",
    )
    .unwrap();
    assert_eq!(signatures.len(), 1);
    assert_eq!(signatures[0].bundle_index, 2);
    assert_eq!(signatures[0].sig, signed_signature);

    // The demoted proof can be replaced without discarding preserved setup.
    queries::store_proof(
        &conn,
        "1111111111111111111111111111111111111111111111111111111111111111",
        "wallet",
        2,
        &[0xB2; 96],
    )
    .unwrap();
    let reproved = queries::load_delegation_submission_data(
        &conn,
        "1111111111111111111111111111111111111111111111111111111111111111",
        "wallet",
        2,
    )
    .unwrap();
    assert_eq!(reproved.proof, vec![0xB2; 96]);

    // Cleared proofless state can be rebuilt against a durable PCZT.
    store_current_delegation_setup(&conn, 0);
    assert_eq!(setup_presence(0), (true, true, true));
}

fn store_current_delegation_setup(conn: &Connection, bundle_index: u32) {
    let gov_nullifiers = vec![vec![0x0B; 32]; crate::governance::BUNDLE_NOTE_SLOTS];
    queries::store_delegation_data_with_pczt_fields(
        conn,
        "1111111111111111111111111111111111111111111111111111111111111111",
        "wallet",
        bundle_index,
        &[0x01; 32],
        &[],
        &[0x02; 32],
        &[],
        &[0x03; 32],
        &[0x04; 32],
        &[0x05; 32],
        &[0x06; 32],
        &[0x07; 32],
        &[0x08; 32],
        1,
        0,
        &[],
        &[0x09; 32],
        &crate::tx1::placeholder_tx1_effects(),
        &[0x0C],
        &[0x0A; 32],
        &gov_nullifiers,
    )
    .unwrap();
}

#[test]
fn migration_preserves_every_submission_state_and_nonlocal_bundle() {
    for state in [
        "submitting",
        "tracking",
        "recovering",
        "submitted_without_hash",
        "confirmed",
        "rejected",
    ] {
        for with_proof in [false, true] {
            let mut conn = Connection::open_in_memory().unwrap();
            conn.execute_batch(include_str!("001_init.sql")).unwrap();
            queries::insert_round(
                &conn,
                "wallet",
                crate::Network::Testnet,
                &test_params(),
                None,
            )
            .unwrap();
            for bundle in 0..5 {
                queries::insert_bundle(&conn, ROUND, "wallet", bundle, &[1]).unwrap();
                store_legacy_delegation_setup(&conn, bundle);
                queries::store_keystone_signature(
                    &conn, ROUND, "wallet", bundle, &[0; 64], &[0; 32], &[0; 32],
                )
                .unwrap();
                if with_proof {
                    queries::store_proof(&conn, ROUND, "wallet", bundle, &[0xA1; 96]).unwrap();
                }
            }
            conn.execute(
                "INSERT INTO chain_submissions
                (identity_key, round_id, wallet_id, network, bundle_index, kind,
                 generation_digest, state, candidate_transaction_hash, tracking_started_at,
                 diagnostic_kind, diagnostic, confirmation_source, final_van_position,
                 vote_commitment_positions, created_at, updated_at)
                VALUES (zeroblob(32), ?1, 'wallet', 'testnet', 0, 'delegation', zeroblob(32), ?2,
                    CASE WHEN ?2='tracking' THEN zeroblob(32) END,
                    CASE WHEN ?2='tracking' THEN 1 END,
                    CASE WHEN ?2='submitted_without_hash' THEN 'missing_hash' END,
                    CASE WHEN ?2='submitted_without_hash' THEN 'unknown' END,
                    CASE WHEN ?2='confirmed' THEN 'tree' END,
                    CASE WHEN ?2='confirmed' THEN 7 END,
                    CASE WHEN ?2='confirmed' THEN X'00' END, 1, 1)",
                rusqlite::params![ROUND, state],
            )
            .unwrap();
            conn.execute(
                "UPDATE bundles SET note_positions_blob=NULL WHERE bundle_index=1",
                [],
            )
            .unwrap();
            conn.execute(
                "UPDATE bundles SET delegation_tx_hash='submitted' WHERE bundle_index=2",
                [],
            )
            .unwrap();
            conn.execute(
                "UPDATE bundles SET van_leaf_position=7 WHERE bundle_index=3",
                [],
            )
            .unwrap();
            conn.execute(
                "UPDATE bundles SET delegation_pczt=X'AA' WHERE bundle_index=4",
                [],
            )
            .unwrap();
            let tables = [
                "bundles",
                "proofs",
                "keystone_signatures",
                "chain_submissions",
            ];
            let before = tables.map(|table| dump_table(&conn, table));
            conn.pragma_update(None, "user_version", 21).unwrap();
            migrate(&mut conn).unwrap();
            assert_eq!(
                tables.map(|table| dump_table(&conn, table)),
                before,
                "{state}, proof={with_proof}"
            );
            migrate(&mut conn).unwrap();
            assert_eq!(tables.map(|table| dump_table(&conn, table)), before);
        }
    }
}
