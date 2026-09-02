use super::*;
use crate::storage::queries;
use crate::VotingRoundParams;
use rusqlite::OptionalExtension;

fn pre_v8_schema() -> String {
    include_str!("001_init.sql").replace("    note_identity_hashes_blob BLOB,\n", "")
}

/// Strips the `pir_proof_cache` table (added at version 15) from a schema.
fn without_pir_proof_cache(schema: &str) -> String {
    let start = schema
        .find("CREATE TABLE pir_proof_cache")
        .expect("schema must contain the table added at version 15");
    let end = start
        + schema[start..]
            .find(");")
            .expect("pir_proof_cache DDL must be terminated")
        + ");".len();
    format!("{}{}", &schema[..start], &schema[end..])
}

/// Strips helper-delivery columns added at version 16.
fn without_durable_ambiguous_deliveries(schema: &str) -> String {
    schema
        .replace("    ambiguous_urls  TEXT NOT NULL DEFAULT '[]',\n", "")
        .replace("    attempting_urls TEXT NOT NULL DEFAULT '[]',\n", "")
        .replace("    target_count    INTEGER NOT NULL DEFAULT 0,\n", "")
}

/// Strips the helper plan table and trigger added at version 17.
fn without_helper_share_plans(schema: &str) -> String {
    let start = schema
        .find("CREATE TABLE helper_share_plans")
        .expect("schema must contain the table added at version 17");
    let next = schema[start..]
        .find("CREATE TABLE share_delegations")
        .expect("helper plan DDL must precede share delegations");
    format!("{}{}", &schema[..start], &schema[start + next..])
}

/// Strips the authoritative lifecycle table added at version 18.
fn without_chain_submissions(schema: &str) -> String {
    let start = schema
        .find("-- Authoritative SDK-owned vote-chain submission lifecycle")
        .expect("schema must contain the table added at version 18");
    schema[..start].to_string()
}

fn v16_schema() -> String {
    without_helper_share_plans(&without_chain_submissions(include_str!("001_init.sql")))
}

fn v17_schema() -> String {
    without_chain_submissions(include_str!("001_init.sql"))
}

fn v15_schema() -> String {
    without_durable_ambiguous_deliveries(&v16_schema())
}

/// The bundle-scoped `imt_proofs` table that version 15 replaced with
/// `pir_proof_cache`, exactly as `001_init.sql` created it through v14.
const V14_IMT_PROOFS_SQL: &str = "CREATE TABLE imt_proofs (
    round_id       TEXT NOT NULL,
    wallet_id      TEXT NOT NULL DEFAULT '',
    bundle_index   INTEGER NOT NULL,
    nullifier      BLOB NOT NULL,
    root           BLOB NOT NULL,
    nf_bounds      BLOB NOT NULL,
    leaf_pos       INTEGER NOT NULL,
    path           BLOB NOT NULL,
    created_at     INTEGER NOT NULL,
    PRIMARY KEY (round_id, wallet_id, bundle_index, nullifier),
    FOREIGN KEY (round_id, wallet_id, bundle_index) REFERENCES bundles(round_id, wallet_id, bundle_index) ON DELETE CASCADE
);";

/// The version-14 schema: no `pir_proof_cache` yet, `imt_proofs` still present.
fn v14_schema() -> String {
    let v16 = v16_schema();
    let schema = without_durable_ambiguous_deliveries(&v16);
    format!(
        "{}\n{}\n",
        without_pir_proof_cache(&schema),
        V14_IMT_PROOFS_SQL
    )
}

/// The launch schema, before `bundle_policy_json` and `pir_proof_cache` were added.
fn launch_schema() -> String {
    let schema = v14_schema();
    let stripped = schema.replace("    bundle_policy_json  TEXT,\n", "");
    assert_ne!(
        stripped, schema,
        "launch_schema must actually drop the column added at version 14"
    );
    stripped
}

/// A canonical 32-byte Pallas round id, so fixtures form real lifecycle
/// identities exactly as a released version-17 database does.
const ROUND: &str = "1111111111111111111111111111111111111111111111111111111111111111";

fn test_params() -> VotingRoundParams {
    VotingRoundParams {
        vote_round_id: ROUND.to_string(),
        snapshot_height: 1000,
        ea_pk: vec![0xEA; 32],
        nc_root: vec![0xAA; 32],
        nullifier_imt_root: vec![0xBB; 32],
    }
}

#[test]
fn test_migrate_fresh_database() {
    let mut conn = Connection::open_in_memory().unwrap();
    migrate(&mut conn).unwrap();

    let version: u32 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(version, CURRENT_VERSION);
}

#[test]
fn test_migrate_idempotent() {
    let mut conn = Connection::open_in_memory().unwrap();
    migrate(&mut conn).unwrap();
    migrate(&mut conn).unwrap();

    let version: u32 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(version, CURRENT_VERSION);
}

#[test]
fn v17_recovery_free_vote_evidence_is_classified_by_completeness() {
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
    for bundle in 0..=2 {
        queries::insert_bundle(&conn, ROUND, "wallet", bundle, &[1]).unwrap();
    }
    for proposal in 1..=3 {
        queries::store_vote(
            &conn,
            ROUND,
            "wallet",
            proposal - 1,
            proposal,
            1,
            &[proposal as u8; 32],
        )
        .unwrap();
    }
    conn.execute(
        "UPDATE bundles SET van_leaf_position=7
              WHERE round_id=?1 AND wallet_id='wallet' AND bundle_index=0",
        [ROUND],
    )
    .unwrap();
    conn.execute(
        "UPDATE votes SET vc_tree_position=8
              WHERE round_id=?1 AND wallet_id='wallet' AND proposal_id=1",
        [ROUND],
    )
    .unwrap();
    conn.execute(
        "UPDATE votes SET tx_hash='NOT-CANONICAL'
              WHERE round_id=?1 AND wallet_id='wallet' AND proposal_id=2",
        [ROUND],
    )
    .unwrap();
    conn.pragma_update(None, "user_version", 17).unwrap();

    migrate(&mut conn).unwrap();

    type GuardRow = (i64, String, Option<String>, Option<Vec<u8>>, i64);
    let rows: Vec<GuardRow> = conn
        .prepare(
            "SELECT proposal_id, state, confirmation_source, generation_digest,
                        committed_post_reservations
                   FROM chain_submissions WHERE kind='vote' ORDER BY proposal_id",
        )
        .unwrap()
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(rows.len(), 2, "the empty vote must not create a submission");
    // Complete positions with no recovery material: terminal, honest about
    // provenance, and carrying no derived generation.
    assert_eq!(rows[0].0, 1);
    assert_eq!(rows[0].1, "confirmed");
    assert_eq!(rows[0].2.as_deref(), Some("legacy_projection"));
    assert!(rows[0].3.is_none());
    assert_eq!(rows[0].4, 0);
    // Incomplete evidence with no recovery material stays permanently
    // unbound: this is the only remaining digestless class.
    assert_eq!(rows[1].0, 2);
    assert_eq!(rows[1].1, "recovering");
    assert!(rows[1].2.is_none());
    assert!(rows[1].3.is_none());
    assert_eq!(rows[1].4, 0);
    let delegation_guards: i64 = conn
        .query_row(
            "SELECT count(*) FROM chain_submissions WHERE kind='delegation'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        delegation_guards, 0,
        "the terminal vote successor makes the earlier delegation evidence obsolete"
    );
}

/// Opens a version-17 database holding one recovery-backed singleton vote.
///
/// `positions` optionally records the version-17 domain VAN and vote
/// commitment positions that a completed pre-upgrade submission left behind.
fn v17_recovery_backed_singleton(positions: Option<(i64, i64)>) -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(&v17_schema()).unwrap();
    queries::insert_round(
        &conn,
        "wallet",
        crate::Network::Testnet,
        &test_params(),
        None,
    )
    .unwrap();
    queries::insert_bundle(&conn, ROUND, "wallet", 0, &[1]).unwrap();
    let recovery = positions
        .map(|(_, vc)| {
            recovery_json_with_tree_position(&released_singleton_recovery_json(ROUND), vc)
        })
        .unwrap_or_else(|| released_singleton_recovery_json(ROUND));
    let stored_commitment =
        crate::vote::stored_vote_commitment_bytes(&crate::vote::parse_recovery(&recovery).unwrap())
            .unwrap();
    queries::store_vote(&conn, ROUND, "wallet", 0, 1, 2, &stored_commitment).unwrap();
    conn.execute(
        "UPDATE votes SET commitment_bundle_json=?1
              WHERE round_id=?2 AND wallet_id='wallet' AND bundle_index=0 AND proposal_id=1",
        rusqlite::params![recovery, ROUND],
    )
    .unwrap();
    if let Some((van, vc)) = positions {
        conn.execute(
            "UPDATE bundles SET van_leaf_position=?1
                  WHERE round_id=?2 AND wallet_id='wallet' AND bundle_index=0",
            rusqlite::params![van, ROUND],
        )
        .unwrap();
        conn.execute(
            "UPDATE votes SET vc_tree_position=?1
                  WHERE round_id=?2 AND wallet_id='wallet' AND bundle_index=0 AND proposal_id=1",
            rusqlite::params![vc, ROUND],
        )
        .unwrap();
    }
    conn.pragma_update(None, "user_version", 17).unwrap();
    conn
}

fn vote_submission_row(conn: &Connection) -> (String, Option<String>, bool, Option<Vec<u8>>) {
    conn.query_row(
        "SELECT state, confirmation_source, generation_digest IS NOT NULL,
                    candidate_transaction_hash
               FROM chain_submissions WHERE kind='vote'",
        [],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get::<_, bool>(2)?,
                row.get(3)?,
            ))
        },
    )
    .unwrap()
}

/// Recovery material determines the generation, so the row is bound.
///
/// This is the class that version 17 could not represent: it has everything
/// needed to re-derive the exact output layout, and only the configured
/// vote-chain id was missing. Bound means tree recovery can now resolve it
/// instead of the row being blocked forever.
#[test]
fn v17_recovery_backed_vote_migrates_to_a_bound_generation() {
    let mut conn = v17_recovery_backed_singleton(None);
    migrate(&mut conn).unwrap();

    let (state, source, has_digest, candidate) = vote_submission_row(&conn);
    assert_eq!(state, "recovering");
    assert_eq!(source, None);
    assert!(has_digest, "recovery material determines the generation");
    assert_eq!(candidate, None, "no historical hash is imported");
}

/// Recorded positions that match the derived layout are a confirmation.
#[test]
fn v17_recovery_backed_vote_with_matching_positions_confirms_as_legacy_import() {
    let mut conn = v17_recovery_backed_singleton(Some((7, 8)));
    migrate(&mut conn).unwrap();

    let (state, source, has_digest, candidate) = vote_submission_row(&conn);
    assert_eq!(state, "confirmed");
    assert_eq!(source.as_deref(), Some("legacy_import"));
    assert!(has_digest);
    assert_eq!(candidate, None);
    let (van, positions): (i64, Vec<u8>) = conn
        .query_row(
            "SELECT final_van_position, vote_commitment_positions
                   FROM chain_submissions WHERE kind='vote'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(van, 7);
    assert_eq!(positions, encode_positions(&[8]).unwrap());
}

/// Complete but non-adjacent positions cannot confirm a bound generation.
#[test]
fn v17_recovery_backed_vote_with_mismatched_layout_stays_recovering() {
    let mut conn = v17_recovery_backed_singleton(Some((7, 0)));
    migrate(&mut conn).unwrap();

    let (state, source, has_digest, candidate) = vote_submission_row(&conn);
    assert_eq!(state, "recovering");
    assert_eq!(source, None);
    assert!(has_digest);
    assert_eq!(candidate, None);
    let positions: (Option<i64>, Option<Vec<u8>>) = conn
        .query_row(
            "SELECT final_van_position, vote_commitment_positions
                   FROM chain_submissions WHERE kind='vote'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(positions, (None, None));
}

#[test]
fn legacy_import_layout_requires_contiguous_ordered_positions() {
    let batch = ExpectedTreeLayout::VoteBatch {
        final_successor_van: [1; 32],
        vote_commitments: vec![[2; 32], [3; 32]],
    };
    assert!(positions_match_expected_layout(&batch, 7, &[8, 9]));
    assert!(!positions_match_expected_layout(&batch, 7, &[9, 8]));
    assert!(!positions_match_expected_layout(&batch, 7, &[8, 10]));
    assert!(!positions_match_expected_layout(&batch, 7, &[8]));

    let vote = ExpectedTreeLayout::Vote {
        successor_van: [1; 32],
        vote_commitment: [2; 32],
    };
    assert!(!positions_match_expected_layout(&vote, u64::MAX, &[0]));
}

/// A partially recorded position set is never accepted as a confirmation.
#[test]
fn v17_recovery_backed_vote_without_a_van_stays_bound_and_recovering() {
    let mut conn = v17_recovery_backed_singleton(None);
    conn.execute(
        "UPDATE votes SET vc_tree_position=0
              WHERE round_id=?1 AND wallet_id='wallet' AND bundle_index=0 AND proposal_id=1",
        [ROUND],
    )
    .unwrap();
    migrate(&mut conn).unwrap();

    let (state, source, has_digest, _) = vote_submission_row(&conn);
    assert_eq!(state, "recovering");
    assert_eq!(source, None);
    assert!(has_digest);
}

/// A round id that cannot form a canonical identity creates no row.
///
/// Such a round can never be the subject of runtime submission work either,
/// so there is nothing to guard, and the round stays prunable and
/// explicitly deletable.
#[test]
fn noncanonical_v17_round_ids_create_no_submission() {
    for round_id in ["legacy-round".to_string(), "ff".repeat(32)] {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&v17_schema()).unwrap();
        let mut params = test_params();
        params.vote_round_id = round_id.clone();
        queries::insert_round(&conn, "wallet", crate::Network::Testnet, &params, None).unwrap();
        queries::insert_bundle(&conn, &round_id, "wallet", 0, &[1]).unwrap();
        queries::store_vote(&conn, &round_id, "wallet", 0, 1, 1, &[1; 32]).unwrap();
        conn.execute(
            "UPDATE bundles SET van_leaf_position=7 WHERE round_id=?1",
            [&round_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE votes SET vc_tree_position=8 WHERE round_id=?1",
            [&round_id],
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 17).unwrap();

        migrate(&mut conn).unwrap();

        assert_eq!(
            conn.query_row("SELECT count(*) FROM chain_submissions", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0,
            "{round_id}"
        );
        assert_eq!(
            conn.query_row(
                "SELECT van_leaf_position FROM bundles WHERE round_id=?1",
                [&round_id],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            7,
            "version-17 domain evidence is left untouched for {round_id}"
        );
    }
}

/// Invalid proposal identities cannot own authoritative lifecycle protection.
#[test]
fn invalid_v17_proposal_identities_abort_atomically() {
    for (proposal_id, expected_error) in [
        (0, "proposal_id must be between 1 and 15, got 0"),
        (16, "proposal_id must be between 1 and 15, got 16"),
    ] {
        let wallet_id = "wallet";
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&v17_schema()).unwrap();
        queries::insert_round(
            &conn,
            wallet_id,
            crate::Network::Testnet,
            &test_params(),
            None,
        )
        .unwrap();
        queries::insert_bundle(&conn, ROUND, wallet_id, 0, &[1]).unwrap();
        queries::store_vote(&conn, ROUND, wallet_id, 0, proposal_id, 1, &[1; 32]).unwrap();
        conn.execute(
            "UPDATE bundles SET van_leaf_position=7
                  WHERE round_id=?1 AND wallet_id=?2",
            rusqlite::params![ROUND, wallet_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE votes SET vc_tree_position=8
                  WHERE round_id=?1 AND wallet_id=?2 AND proposal_id=?3",
            rusqlite::params![ROUND, wallet_id, proposal_id],
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 17).unwrap();

        let error = migrate(&mut conn).unwrap_err();

        assert!(error.to_string().contains(expected_error), "{error}");
        assert_eq!(
            conn.pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                .unwrap(),
            17,
            "identity corruption must roll back the complete migration"
        );
        assert!(!table_names(&conn).contains(&"chain_submissions".to_string()));
        assert_eq!(
            conn.query_row(
                "SELECT b.van_leaf_position, v.vc_tree_position
                       FROM bundles b
                       JOIN votes v USING (round_id, wallet_id, bundle_index)",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
            )
            .unwrap(),
            (7, 8),
            "rollback must preserve the original chain evidence"
        );
    }
}

/// A confirmed successor makes earlier delegation evidence obsolete.
///
/// Importing a delegation row for it would permanently block every later
/// generation in that bundle for no gain, so migration creates none. This
/// holds for an atomic batch successor exactly as for a singleton.
#[test]
fn confirmed_successor_suppresses_the_obsolete_delegation_guard() {
    for confirm_positions in [true, false] {
        let mut conn = v17_recovery_backed_singleton(if confirm_positions {
            Some((7, 8))
        } else {
            None
        });
        conn.execute(
            "UPDATE bundles SET delegation_tx_hash='dtx'
                  WHERE round_id=?1 AND wallet_id='wallet' AND bundle_index=0",
            [ROUND],
        )
        .unwrap();
        migrate(&mut conn).unwrap();

        let delegation_rows: i64 = conn
            .query_row(
                "SELECT count(*) FROM chain_submissions WHERE kind='delegation'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        if confirm_positions {
            assert_eq!(delegation_rows, 0, "a confirmed successor is terminal");
        } else {
            assert_eq!(
                delegation_rows, 1,
                "an unresolved successor does not retire delegation evidence"
            );
        }
    }
}

/// Without delegation setup material there is no generation to bind.
#[test]
fn v17_delegation_without_setup_material_remains_a_digestless_guard() {
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
    queries::insert_bundle(&conn, ROUND, "wallet", 0, &[1]).unwrap();
    conn.execute(
        "UPDATE bundles SET delegation_tx_hash='dtx'
              WHERE round_id=?1 AND wallet_id='wallet' AND bundle_index=0",
        [ROUND],
    )
    .unwrap();
    conn.pragma_update(None, "user_version", 17).unwrap();

    migrate(&mut conn).unwrap();

    let (state, has_digest, kind) = conn
        .query_row(
            "SELECT state, generation_digest IS NOT NULL, diagnostic_kind
                   FROM chain_submissions WHERE kind='delegation'",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, bool>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(state, "recovering");
    assert!(!has_digest);
    assert_eq!(kind.as_deref(), Some("recovery_unavailable"));
}

fn insert_v17_delegation_with_complete_setup(
    conn: &Connection,
    round_id: &str,
    wallet_id: &str,
    bundle_index: u32,
    van_position: Option<i64>,
) {
    queries::insert_bundle(conn, round_id, wallet_id, bundle_index, &[1]).unwrap();
    conn.execute(
        "UPDATE bundles SET note_identity_hashes_blob=?1
              WHERE round_id=?2 AND wallet_id=?3 AND bundle_index=?4",
        rusqlite::params![vec![0x01_u8; 32], round_id, wallet_id, bundle_index],
    )
    .unwrap();

    let padded_values = vec![vec![0x02; 32]; crate::governance::BUNDLE_NOTE_SLOTS - 1];
    let padded_secrets =
        vec![(vec![0x03; 32], vec![0x04; 32]); crate::governance::BUNDLE_NOTE_SLOTS - 1];
    let gov_nullifiers = vec![vec![0x05; 32]; crate::governance::BUNDLE_NOTE_SLOTS];
    queries::store_delegation_data_with_pczt_fields(
        conn,
        round_id,
        wallet_id,
        bundle_index,
        &[0x06; 32],
        &padded_values,
        &[0x07; 32],
        &padded_values,
        &[0x08; 32],
        &[0x09; 32],
        &[0x0a; 32],
        &[0x0b; 32],
        &[0x0c; 32],
        &[0x0d; 32],
        1,
        0,
        &padded_secrets,
        &[0x0e; 32],
        &crate::tx1::placeholder_tx1_effects(),
        &[0x0f; 32],
        &gov_nullifiers,
    )
    .unwrap();
    queries::store_proof(conn, round_id, wallet_id, bundle_index, &[0x10; 96]).unwrap();
    conn.execute(
        "UPDATE bundles
                SET delegation_tx_hash='dtx', van_leaf_position=?1
              WHERE round_id=?2 AND wallet_id=?3 AND bundle_index=?4",
        rusqlite::params![van_position, round_id, wallet_id, bundle_index],
    )
    .unwrap();
}

fn v17_delegation_with_complete_setup() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(&v17_schema()).unwrap();
    queries::insert_round(
        &conn,
        "wallet",
        crate::Network::Testnet,
        &test_params(),
        None,
    )
    .unwrap();
    insert_v17_delegation_with_complete_setup(&conn, ROUND, "wallet", 0, None);
    conn.pragma_update(None, "user_version", 17).unwrap();
    conn
}

/// Complete delegation setup binds its real generation without requiring a
/// SpendAuth signer during migration.
#[test]
fn v17_complete_delegation_setup_binds_without_a_signer() {
    let mut conn = v17_delegation_with_complete_setup();
    let identity = v17_identity(
        "wallet",
        "testnet",
        ROUND,
        0,
        ChainSubmissionTarget::Delegation,
    )
    .unwrap()
    .unwrap();
    let expected_digest = complete_generation_for_delegation(&conn, &identity)
        .unwrap()
        .expect("complete setup must derive a delegation generation")
        .generation()
        .digest()
        .as_bytes()
        .to_vec();

    migrate(&mut conn).unwrap();

    let (state, generation_digest, source, candidate, attempts, diagnostic_kind): BoundRowFields =
        conn.query_row(
            "SELECT state, generation_digest, confirmation_source,
                        candidate_transaction_hash, committed_post_reservations,
                        diagnostic_kind
                   FROM chain_submissions WHERE kind='delegation'",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(state, "recovering");
    assert_eq!(generation_digest, expected_digest);
    assert_eq!(source, None);
    assert_eq!(candidate, None);
    assert_eq!(attempts, 0);
    assert_eq!(diagnostic_kind.as_deref(), Some("reconciliation_pending"));
}

/// Corrupt setup is quarantined without blocking the database upgrade.
#[test]
fn v17_corrupt_delegation_setup_becomes_a_derivation_failure_guard() {
    let mut conn = v17_delegation_with_complete_setup();
    conn.execute(
        "UPDATE bundles SET van_comm_rand=X'AA'
              WHERE round_id=?1 AND wallet_id='wallet' AND bundle_index=0",
        [ROUND],
    )
    .unwrap();

    migrate(&mut conn).unwrap();

    let version: u32 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, CURRENT_VERSION);
    let guard: (String, bool, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT state, generation_digest IS NOT NULL, diagnostic_kind, diagnostic
                   FROM chain_submissions WHERE kind='delegation'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(guard.0, "recovering");
    assert!(!guard.1);
    assert_eq!(guard.2.as_deref(), Some("generation_derivation_failed"));
    assert_eq!(
        guard.3.as_deref(),
        Some(GENERATION_DERIVATION_FAILED_DIAGNOSTIC)
    );
}

/// Missing or wrongly typed values in an otherwise present setup row are
/// malformed recovery inputs, not failures to read the database.
#[test]
fn v17_malformed_delegation_row_becomes_a_derivation_failure_guard() {
    for (case, mutation) in [
        (
            "null setup column",
            "UPDATE bundles SET van_comm_rand=NULL
              WHERE round_id=?1 AND wallet_id='wallet' AND bundle_index=0",
        ),
        (
            "wrong proof type",
            "UPDATE proofs SET proof='not-a-blob'
              WHERE round_id=?1 AND wallet_id='wallet' AND bundle_index=0",
        ),
    ] {
        let mut conn = v17_delegation_with_complete_setup();
        conn.execute(mutation, [ROUND]).unwrap();

        migrate(&mut conn).unwrap();

        let version: u32 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, CURRENT_VERSION, "{case}");
        let guard: (String, bool, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT state, generation_digest IS NOT NULL, diagnostic_kind, diagnostic
                   FROM chain_submissions WHERE kind='delegation'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(guard.0, "recovering", "{case}");
        assert!(!guard.1, "{case}");
        assert_eq!(
            guard.2.as_deref(),
            Some("generation_derivation_failed"),
            "{case}"
        );
        assert_eq!(
            guard.3.as_deref(),
            Some(GENERATION_DERIVATION_FAILED_DIAGNOSTIC),
            "{case}"
        );
    }
}

/// Migration and the runtime store agree on the identity key.
///
/// They must, or a migrated row and a natively reserved row for the same
/// submission would occupy separate namespaces instead of colliding on the
/// primary key.
#[test]
fn v17_identity_key_matches_the_runtime_store() {
    let mut conn = v17_recovery_backed_singleton(None);
    migrate(&mut conn).unwrap();

    let stored: Vec<u8> = conn
        .query_row(
            "SELECT identity_key FROM chain_submissions WHERE kind='vote'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let identity = v17_identity(
        "wallet",
        "testnet",
        ROUND,
        0,
        ChainSubmissionTarget::Vote { proposal_id: 1 },
    )
    .unwrap()
    .expect("the fixture round id is canonical");
    assert_eq!(stored, submission_identity_key(&identity));
}

/// Empty identities written outside the public API cannot block database open.
#[test]
fn empty_wallet_namespace_is_skipped_without_blocking_open() {
    let temp = v17_file(|conn| {
        queries::insert_round(conn, "", crate::Network::Testnet, &test_params(), None).unwrap();
        queries::insert_bundle(conn, ROUND, "", 0, &[1]).unwrap();
        queries::store_vote(conn, ROUND, "", 0, 1, 1, &[1; 32]).unwrap();
        conn.execute(
            "UPDATE bundles SET van_leaf_position=7
                  WHERE round_id=?1 AND wallet_id='' AND bundle_index=0",
            [ROUND],
        )
        .unwrap();
        conn.execute(
            "UPDATE votes SET vc_tree_position=8
                  WHERE round_id=?1 AND wallet_id='' AND bundle_index=0 AND proposal_id=1",
            [ROUND],
        )
        .unwrap();
    });

    let db = crate::storage::VotingDb::open(temp.path()).unwrap();
    db.set_wallet_id("wallet");

    assert_eq!(db.wallet_id(), "wallet");
    assert_eq!(
        db.conn()
            .query_row("SELECT count(*) FROM chain_submissions", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
        0
    );
    let positions: (i64, i64) = db
        .conn()
        .query_row(
            "SELECT b.van_leaf_position, v.vc_tree_position
               FROM bundles b
               JOIN votes v USING (round_id, wallet_id, bundle_index)
              WHERE b.wallet_id=''",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(positions, (7, 8));
    drop(db);

    crate::storage::VotingDb::open(temp.path()).unwrap();
}

/// Columns read back when asserting how one migrated row was bound.
type BoundRowFields = (
    String,
    Vec<u8>,
    Option<String>,
    Option<Vec<u8>>,
    i64,
    Option<String>,
);

/// A temporary database file removed when the test drops it.
struct TempDb(String);

impl TempDb {
    fn path(&self) -> &str {
        &self.0
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        for suffix in ["", "-journal", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.0));
        }
    }
}

/// Writes a version-17 fixture to a file so migration crash behavior can be
/// observed by reopening the same database.
fn v17_file(build: impl FnOnce(&Connection)) -> TempDb {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let unique = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let path = std::env::temp_dir()
        .join(format!(
            "zcash_voting_v17_{}_{unique}.sqlite",
            std::process::id()
        ))
        .to_string_lossy()
        .into_owned();
    let temp = TempDb(path);
    let conn = Connection::open(temp.path()).unwrap();
    conn.execute_batch(&v17_schema()).unwrap();
    build(&conn);
    conn.pragma_update(None, "user_version", 17).unwrap();
    drop(conn);
    temp
}

fn build_confirmed_batch(conn: &Connection) {
    let (first, second) = released_batch_recovery_json(ROUND);
    let first = recovery_json_with_tree_position(&first, 8);
    let second = recovery_json_with_tree_position(&second, 9);
    let stored = [&first, &second].map(|json| {
        crate::vote::stored_vote_commitment_bytes(&crate::vote::parse_recovery(json).unwrap())
            .unwrap()
    });
    queries::insert_round(
        conn,
        "wallet",
        crate::Network::Testnet,
        &test_params(),
        None,
    )
    .unwrap();
    queries::insert_bundle(conn, ROUND, "wallet", 0, &[1]).unwrap();
    for (proposal, json) in [(1, &first), (2, &second)] {
        queries::store_vote(
            conn,
            ROUND,
            "wallet",
            0,
            proposal,
            2,
            &stored[proposal as usize - 1],
        )
        .unwrap();
        conn.execute(
            "UPDATE votes SET commitment_bundle_json=?1, tx_hash='shared-hash',
                                  vc_tree_position=?2
                  WHERE round_id=?3 AND bundle_index=0 AND proposal_id=?4",
            rusqlite::params![json, proposal as i64 + 7, ROUND, proposal],
        )
        .unwrap();
    }
    conn.execute(
        "UPDATE bundles SET van_leaf_position=7 WHERE round_id=?1",
        [ROUND],
    )
    .unwrap();
}

/// A confirmed atomic batch binds one row, and its members stay locked.
///
/// Migration deliberately creates no per-member singleton row for a batch,
/// so member protection rests on the recorded version-17 domain positions.
/// This pins that layering: without it, a member's ballot intent could be
/// changed and re-submitted as a singleton after the batch already
/// confirmed on chain.
#[test]
fn confirmed_batch_members_cannot_change_ballot_intent() {
    let temp = v17_file(build_confirmed_batch);
    let mut conn = Connection::open(temp.path()).unwrap();
    migrate(&mut conn).unwrap();
    let rows: Vec<(String, String)> = conn
        .prepare("SELECT kind, state FROM chain_submissions")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        rows,
        vec![("vote_batch".to_string(), "confirmed".to_string())]
    );
    drop(conn);

    let db = crate::storage::VotingDb::open(temp.path()).unwrap();
    db.set_wallet_id("wallet");
    for proposal in [1, 2] {
        let error = db
            .set_ballot_intent(ROUND, proposal, crate::session::Decision::Choice(1), 3)
            .unwrap_err();
        assert!(
            error.to_string().contains("conflicts with ballot intent"),
            "member {proposal} must stay locked: {error}"
        );
    }
}

/// Public phase and recovery views follow the one authoritative batch row,
/// even though migration deliberately creates no singleton member rows.
#[test]
fn migrated_batch_members_use_the_authoritative_batch_phase() {
    for (confirm_positions, expected_phase) in [
        (false, crate::phases::VotePhase::SubmissionManaged),
        (true, crate::phases::VotePhase::Confirmed),
    ] {
        let temp = v17_file(|conn| {
            build_confirmed_batch(conn);
            conn.execute("UPDATE votes SET tx_hash=NULL", []).unwrap();
            if !confirm_positions {
                conn.execute("UPDATE votes SET vc_tree_position=NULL", [])
                    .unwrap();
                conn.execute("UPDATE bundles SET van_leaf_position=NULL", [])
                    .unwrap();
            }
        });
        let db = crate::storage::VotingDb::open(temp.path()).unwrap();
        db.set_wallet_id("wallet");

        for proposal_id in [1, 2] {
            assert_eq!(
                db.vote_phase(ROUND, 0, proposal_id).unwrap(),
                expected_phase
            );
        }
        assert_eq!(
            db.vote_phases(ROUND).unwrap(),
            vec![(0, 1, expected_phase), (0, 2, expected_phase)]
        );
        assert_eq!(
            crate::recovery::round_snapshot(&db, ROUND)
                .unwrap()
                .votes
                .into_iter()
                .map(|vote| vote.phase)
                .collect::<Vec<_>>(),
            vec![expected_phase, expected_phase]
        );

        let (kind, state, historical_hashes): (String, String, i64) = db
            .conn()
            .query_row(
                "SELECT s.kind, s.state,
                            (SELECT count(*) FROM votes WHERE tx_hash IS NOT NULL)
                       FROM chain_submissions s",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(kind, "vote_batch");
        assert_eq!(
            state,
            if confirm_positions {
                "confirmed"
            } else {
                "recovering"
            }
        );
        assert_eq!(historical_hashes, 0);
    }
}

/// Builds a version-17 database with one row of every import class.
///
/// Used by the crash tests so an interrupted migration has several distinct
/// inserts, position checks, and generation derivations to land inside.
fn build_mixed_v17_evidence(conn: &Connection) {
    queries::insert_round(
        conn,
        "wallet",
        crate::Network::Testnet,
        &test_params(),
        None,
    )
    .unwrap();
    for bundle in 0..=2 {
        queries::insert_bundle(conn, ROUND, "wallet", bundle, &[1]).unwrap();
    }
    // Bundle 0: recovery-backed singleton with complete positions.
    let recovery = recovery_json_with_tree_position(&released_singleton_recovery_json(ROUND), 8);
    let stored =
        crate::vote::stored_vote_commitment_bytes(&crate::vote::parse_recovery(&recovery).unwrap())
            .unwrap();
    queries::store_vote(conn, ROUND, "wallet", 0, 1, 2, &stored).unwrap();
    conn.execute(
        "UPDATE votes SET commitment_bundle_json=?1, vc_tree_position=8
              WHERE round_id=?2 AND bundle_index=0 AND proposal_id=1",
        rusqlite::params![recovery, ROUND],
    )
    .unwrap();
    conn.execute(
        "UPDATE bundles SET van_leaf_position=7 WHERE round_id=?1 AND bundle_index=0",
        [ROUND],
    )
    .unwrap();
    // Bundle 1: positions only, no recovery material.
    queries::store_vote(conn, ROUND, "wallet", 1, 2, 1, &[2; 32]).unwrap();
    conn.execute(
        "UPDATE votes SET vc_tree_position=11
              WHERE round_id=?1 AND bundle_index=1 AND proposal_id=2",
        [ROUND],
    )
    .unwrap();
    conn.execute(
        "UPDATE bundles SET van_leaf_position=12 WHERE round_id=?1 AND bundle_index=1",
        [ROUND],
    )
    .unwrap();
    // Bundle 2: incomplete evidence with no recovery material, plus
    // delegation evidence that cannot be bound.
    queries::store_vote(conn, ROUND, "wallet", 2, 3, 1, &[3; 32]).unwrap();
    conn.execute(
        "UPDATE votes SET tx_hash='NOT-CANONICAL'
              WHERE round_id=?1 AND bundle_index=2 AND proposal_id=3",
        [ROUND],
    )
    .unwrap();
    conn.execute(
        "UPDATE bundles SET delegation_tx_hash='dtx' WHERE round_id=?1 AND bundle_index=2",
        [ROUND],
    )
    .unwrap();
}

/// Dumps every row of every table, so "all original bytes" can be compared.
fn dump_all_tables(conn: &Connection) -> Vec<(String, Vec<String>)> {
    let mut dump = Vec::new();
    for table in table_names(conn) {
        let mut statement = conn.prepare(&format!("SELECT * FROM \"{table}\"")).unwrap();
        let columns = statement.column_count();
        let rows: Vec<String> = statement
            .query_map([], |row| {
                let mut encoded = String::new();
                for index in 0..columns {
                    let value = row.get_ref(index).unwrap();
                    encoded.push_str(&format!("{value:?}|"));
                }
                Ok(encoded)
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let mut rows = rows;
        rows.sort();
        dump.push((table, rows));
    }
    dump.sort();
    dump
}

/// Dumps the classification of every migrated row.
///
/// Timestamps are excluded: they are copied from each round's own
/// `created_at`, so they are stable within one database but differ between
/// independently built fixtures. What must be identical is the
/// classification: identity, kind, digest, state, source, and positions.
fn chain_submission_dump(conn: &Connection) -> Vec<String> {
    let mut statement = conn
        .prepare(
            "SELECT hex(identity_key), round_id, wallet_id, network, bundle_index, kind,
                        ifnull(proposal_id,-1), ifnull(hex(ordered_batch_digest),''),
                        ifnull(hex(generation_digest),''), state, committed_post_reservations,
                        ifnull(diagnostic_kind,''), ifnull(diagnostic,''),
                        ifnull(confirmation_source,''), ifnull(final_van_position,-1),
                        ifnull(hex(vote_commitment_positions),'')
                   FROM chain_submissions ORDER BY hex(identity_key)",
        )
        .unwrap();
    let columns = statement.column_count();
    statement
        .query_map([], |row| {
            let mut encoded = String::new();
            for index in 0..columns {
                encoded.push_str(&format!("{:?}|", row.get_ref(index).unwrap()));
            }
            Ok(encoded)
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// Asserts a database is either untouched version 17 or complete version 18.
///
/// Returns whether the migration had committed. In both cases a restarted
/// process must reach the same classification, which is the property a
/// crash at any point has to preserve.
fn assert_v17_or_complete_v18(
    temp: &TempDb,
    before: &[(String, Vec<String>)],
    reference: &[String],
) -> bool {
    let conn = Connection::open(temp.path()).unwrap();
    let version: u32 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    if version == CURRENT_VERSION {
        assert_eq!(
            chain_submission_dump(&conn),
            reference,
            "a committed migration must classify the reference rows"
        );
        return true;
    }

    assert_eq!(version, 17, "an interrupted migration must preserve v17");
    assert!(
        !table_names(&conn).contains(&"chain_submissions".to_string()),
        "an interrupted migration must leave no partial table"
    );
    assert_eq!(
        dump_all_tables(&conn),
        before,
        "an interrupted migration must preserve every original byte"
    );
    drop(conn);

    // A restarted process retries and reaches the same version 18.
    let mut retry = Connection::open(temp.path()).unwrap();
    migrate(&mut retry).unwrap();
    assert_eq!(
        chain_submission_dump(&retry),
        reference,
        "retry after an interrupt must classify identical rows"
    );
    false
}

/// Killing the process mid-migration can only leave version 17 or a
/// complete version 18.
///
/// The migration derives generations, checks position ownership, and
/// inserts several rows, so a kill can land in many different places.
/// Whatever it interrupts, reopening the database must find either the
/// untouched version-17 source or a fully migrated version 18 -- never a
/// half-imported table -- and a restart must classify identical rows.
#[test]
fn interrupted_migration_leaves_v17_or_a_complete_v18() {
    let reference = {
        let temp = v17_file(build_mixed_v17_evidence);
        let mut conn = Connection::open(temp.path()).unwrap();
        migrate(&mut conn).unwrap();
        chain_submission_dump(&conn)
    };
    assert!(
        reference.len() >= 4,
        "the fixture must exercise several inserts, got {}",
        reference.len()
    );

    // A kill that lands inside the migration: SQLite ignores an interrupt
    // raised while no statement is running, so the killer spins for the
    // whole call rather than firing once.
    let mut interrupted_before_commit = 0;
    {
        let temp = v17_file(build_mixed_v17_evidence);
        let before = dump_all_tables(&Connection::open(temp.path()).unwrap());
        let mut conn = Connection::open(temp.path()).unwrap();
        let handle = conn.get_interrupt_handle();
        let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let killer_flag = std::sync::Arc::clone(&running);
        let killer = std::thread::spawn(move || {
            while killer_flag.load(std::sync::atomic::Ordering::SeqCst) {
                handle.interrupt();
                std::thread::yield_now();
            }
        });
        let outcome = migrate(&mut conn);
        running.store(false, std::sync::atomic::Ordering::SeqCst);
        killer.join().unwrap();
        drop(conn);
        if !assert_v17_or_complete_v18(&temp, &before, &reference) {
            interrupted_before_commit += 1;
        }
        assert!(
            outcome.is_err() || interrupted_before_commit == 0,
            "an aborted migration must report an error"
        );
    }

    // Kills at many different points. Whichever side of the commit each one
    // lands on, the same invariant must hold.
    for micros in [1_u64, 25, 60, 120, 250, 400, 700, 1_500, 3_000, 6_000] {
        let temp = v17_file(build_mixed_v17_evidence);
        let before = dump_all_tables(&Connection::open(temp.path()).unwrap());
        let mut conn = Connection::open(temp.path()).unwrap();
        let handle = conn.get_interrupt_handle();
        let killer = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_micros(micros));
            handle.interrupt();
        });
        let _ = migrate(&mut conn);
        killer.join().unwrap();
        drop(conn);
        if !assert_v17_or_complete_v18(&temp, &before, &reference) {
            interrupted_before_commit += 1;
        }
    }

    assert!(
        interrupted_before_commit > 0,
        "no kill landed inside the migration, so nothing was exercised"
    );
}

/// Reopening a migrated database creates no duplicates and changes nothing.
#[test]
fn reopening_a_migrated_database_is_a_no_op() {
    let temp = v17_file(build_mixed_v17_evidence);
    let mut conn = Connection::open(temp.path()).unwrap();
    migrate(&mut conn).unwrap();
    let first = chain_submission_dump(&conn);
    drop(conn);

    for _ in 0..3 {
        let mut reopened = Connection::open(temp.path()).unwrap();
        migrate(&mut reopened).unwrap();
        assert_eq!(chain_submission_dump(&reopened), first);
    }
}

#[test]
fn v17_position_ownership_is_scoped_by_network_and_round() {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(&v17_schema()).unwrap();
    for (round_id, network) in [
        (
            "1111111111111111111111111111111111111111111111111111111111111111",
            crate::Network::Mainnet,
        ),
        (
            "2222222222222222222222222222222222222222222222222222222222222222",
            crate::Network::Mainnet,
        ),
    ] {
        let mut params = test_params();
        params.vote_round_id = round_id.to_string();
        queries::insert_round(&conn, "wallet", network, &params, None).unwrap();
        queries::insert_bundle(&conn, round_id, "wallet", 0, &[1]).unwrap();
        queries::store_vote(&conn, round_id, "wallet", 0, 1, 1, &[1; 32]).unwrap();
        conn.execute(
            "UPDATE bundles SET van_leaf_position = 7
                  WHERE round_id = ?1 AND wallet_id = 'wallet' AND bundle_index = 0",
            [round_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE votes SET vc_tree_position = 8
                  WHERE round_id = ?1 AND wallet_id = 'wallet' AND proposal_id = 1",
            [round_id],
        )
        .unwrap();
    }
    conn.pragma_update(None, "user_version", 17).unwrap();

    migrate(&mut conn).unwrap();

    let projections: i64 = conn
        .query_row(
            "SELECT count(*) FROM chain_submissions
                  WHERE state = 'confirmed' AND confirmation_source = 'legacy_projection'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(projections, 2);
}

#[test]
fn v17_duplicate_confirmed_delegation_vans_abort_migration() {
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
    for bundle_index in 0..2 {
        insert_v17_delegation_with_complete_setup(&conn, ROUND, "wallet", bundle_index, Some(7));
    }
    conn.pragma_update(None, "user_version", 17).unwrap();

    let error = migrate(&mut conn).unwrap_err();

    assert!(
        error
            .to_string()
            .contains("validated output position 7 has multiple owners"),
        "{error}"
    );
    assert_eq!(
        conn.pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        17
    );
    assert!(!table_names(&conn).contains(&"chain_submissions".to_string()));
}

#[test]
fn v17_confirmed_delegation_van_cannot_own_an_observed_vote_position() {
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
    insert_v17_delegation_with_complete_setup(&conn, ROUND, "wallet", 0, Some(7));
    queries::insert_bundle(&conn, ROUND, "wallet", 1, &[1]).unwrap();
    queries::store_vote(&conn, ROUND, "wallet", 1, 1, 1, &[1; 32]).unwrap();
    conn.execute(
        "UPDATE bundles SET van_leaf_position=20
              WHERE round_id=?1 AND wallet_id='wallet' AND bundle_index=1",
        [ROUND],
    )
    .unwrap();
    conn.execute(
        "UPDATE votes SET vc_tree_position=7
              WHERE round_id=?1 AND wallet_id='wallet' AND bundle_index=1 AND proposal_id=1",
        [ROUND],
    )
    .unwrap();
    conn.pragma_update(None, "user_version", 17).unwrap();

    let error = migrate(&mut conn).unwrap_err();

    assert!(
        error
            .to_string()
            .contains("validated output position 7 has multiple owners"),
        "{error}"
    );
}

#[test]
fn v17_legacy_projection_vans_remain_exempt_from_ownership() {
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
    for (bundle_index, vc_position) in [(0, 8), (1, 9)] {
        queries::insert_bundle(&conn, ROUND, "wallet", bundle_index, &[1]).unwrap();
        queries::store_vote(&conn, ROUND, "wallet", bundle_index, 1, 1, &[1; 32]).unwrap();
        conn.execute(
            "UPDATE bundles SET van_leaf_position=7
                  WHERE round_id=?1 AND wallet_id='wallet' AND bundle_index=?2",
            rusqlite::params![ROUND, bundle_index],
        )
        .unwrap();
        conn.execute(
            "UPDATE votes SET vc_tree_position=?1
                  WHERE round_id=?2 AND wallet_id='wallet' AND bundle_index=?3 AND proposal_id=1",
            rusqlite::params![vc_position, ROUND, bundle_index],
        )
        .unwrap();
    }
    conn.pragma_update(None, "user_version", 17).unwrap();

    migrate(&mut conn).unwrap();

    let projections: i64 = conn
        .query_row(
            "SELECT count(*) FROM chain_submissions
                  WHERE state='confirmed' AND confirmation_source='legacy_projection'
                    AND final_van_position=7",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(projections, 2);
}

#[test]
fn v17_validated_van_ownership_is_scoped_by_network_and_round() {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(&v17_schema()).unwrap();
    for (round_id, network) in [
        (
            "1111111111111111111111111111111111111111111111111111111111111111",
            crate::Network::Testnet,
        ),
        (
            "2222222222222222222222222222222222222222222222222222222222222222",
            crate::Network::Testnet,
        ),
    ] {
        let mut params = test_params();
        params.vote_round_id = round_id.to_string();
        queries::insert_round(&conn, "wallet", network, &params, None).unwrap();
        insert_v17_delegation_with_complete_setup(&conn, round_id, "wallet", 0, Some(7));
    }
    conn.pragma_update(None, "user_version", 17).unwrap();

    migrate(&mut conn).unwrap();

    let imports: i64 = conn
        .query_row(
            "SELECT count(*) FROM chain_submissions
                  WHERE kind='delegation' AND state='confirmed'
                    AND confirmation_source='legacy_import' AND final_van_position=7",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(imports, 2);
}

#[test]
fn v17_recovering_guards_still_validate_every_observed_position() {
    for (positions, expected) in [
        (vec![-1], "negative legacy VC position"),
        (vec![8, 8], "vote commitment position 8 has multiple owners"),
    ] {
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
        queries::insert_bundle(&conn, ROUND, "wallet", 0, &[1]).unwrap();
        for (offset, position) in positions.into_iter().enumerate() {
            let proposal_id = u32::try_from(offset + 1).unwrap();
            queries::store_vote(
                &conn,
                ROUND,
                "wallet",
                0,
                proposal_id,
                1,
                &[proposal_id as u8; 32],
            )
            .unwrap();
            conn.execute(
                    "UPDATE votes SET vc_tree_position = ?1
                      WHERE round_id = '1111111111111111111111111111111111111111111111111111111111111111' AND wallet_id = 'wallet'
                        AND bundle_index = 0 AND proposal_id = ?2",
                    rusqlite::params![position, proposal_id],
                )
                .unwrap();
        }
        conn.pragma_update(None, "user_version", 17).unwrap();

        let error = migrate(&mut conn).unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
        assert_eq!(
            conn.pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                .unwrap(),
            17
        );
    }
}

#[test]
fn malformed_v17_recovery_becomes_a_member_guard_without_blocking_other_rows() {
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
    queries::insert_bundle(&conn, ROUND, "wallet", 0, &[1]).unwrap();
    queries::store_vote(&conn, ROUND, "wallet", 0, 1, 1, &[1; 32]).unwrap();
    conn.execute(
        "UPDATE votes SET commitment_bundle_json='{broken' WHERE proposal_id=1",
        [],
    )
    .unwrap();
    let other_round = "2222222222222222222222222222222222222222222222222222222222222222";
    let mut other_params = test_params();
    other_params.vote_round_id = other_round.to_string();
    queries::insert_round(
        &conn,
        "wallet",
        crate::Network::Testnet,
        &other_params,
        None,
    )
    .unwrap();
    queries::insert_bundle(&conn, other_round, "wallet", 0, &[1]).unwrap();
    queries::store_vote(&conn, other_round, "wallet", 0, 1, 1, &[2; 32]).unwrap();
    conn.execute(
        "UPDATE bundles SET van_leaf_position=17 WHERE round_id=?1",
        [other_round],
    )
    .unwrap();
    conn.execute(
        "UPDATE votes SET vc_tree_position=18 WHERE round_id=?1",
        [other_round],
    )
    .unwrap();
    conn.pragma_update(None, "user_version", 17).unwrap();

    migrate(&mut conn).unwrap();
    assert_eq!(
        conn.pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        CURRENT_VERSION
    );
    let rows: Vec<(String, bool, Option<String>)> = conn
        .prepare(
            "SELECT state, generation_digest IS NOT NULL, diagnostic_kind
               FROM chain_submissions ORDER BY round_id",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        rows,
        vec![
            (
                "recovering".to_string(),
                false,
                Some("legacy_evidence_invalid".to_string())
            ),
            ("confirmed".to_string(), false, None),
        ]
    );
    assert_eq!(
        conn.query_row(
            "SELECT commitment_bundle_json FROM votes
              WHERE round_id=?1 AND proposal_id=1",
            [ROUND],
            |row| row.get::<_, String>(0)
        )
        .unwrap(),
        "{broken"
    );
}

#[test]
fn v17_recovery_mismatches_become_member_guards() {
    const ROUND_ID: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    let base = released_singleton_recovery_json(ROUND_ID);
    let stored_commitment =
        crate::vote::stored_vote_commitment_bytes(&crate::vote::parse_recovery(&base).unwrap())
            .unwrap();
    let mismatches = [
        (
            "vote_round_id",
            serde_json::json!("2222222222222222222222222222222222222222222222222222222222222222"),
        ),
        ("bundle_index", serde_json::json!(1)),
        ("proposal_id", serde_json::json!(2)),
        ("vote_decision", serde_json::json!(1)),
        ("vote_commitment", serde_json::json!(vec![0x44_u8; 32])),
    ];
    for (field, replacement) in mismatches {
        let mut value: serde_json::Value = serde_json::from_str(&base).unwrap();
        value[field] = replacement;
        let stale = serde_json::to_string(&value).unwrap();
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&v17_schema()).unwrap();
        let mut params = test_params();
        params.vote_round_id = ROUND_ID.to_string();
        queries::insert_round(&conn, "wallet", crate::Network::Testnet, &params, None).unwrap();
        queries::insert_bundle(&conn, ROUND_ID, "wallet", 0, &[1]).unwrap();
        queries::store_vote(&conn, ROUND_ID, "wallet", 0, 1, 2, &stored_commitment).unwrap();
        conn.execute(
            "UPDATE votes SET commitment_bundle_json=?1 WHERE round_id=?2",
            rusqlite::params![stale, ROUND_ID],
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 17).unwrap();

        migrate(&mut conn).unwrap();
        assert_eq!(
            conn.pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                .unwrap(),
            CURRENT_VERSION,
            "{field}"
        );
        let (state, has_digest, diagnostic): (String, bool, String) = conn
            .query_row(
                "SELECT state, generation_digest IS NOT NULL, diagnostic_kind
                   FROM chain_submissions",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            (state.as_str(), has_digest, diagnostic.as_str()),
            ("recovering", false, "legacy_evidence_invalid"),
            "{field}"
        );
    }

    let mut conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(&v17_schema()).unwrap();
    let mut params = test_params();
    params.vote_round_id = ROUND_ID.to_string();
    queries::insert_round(&conn, "wallet", crate::Network::Testnet, &params, None).unwrap();
    queries::insert_bundle(&conn, ROUND_ID, "wallet", 0, &[1]).unwrap();
    queries::store_vote(&conn, ROUND_ID, "wallet", 0, 1, 2, &stored_commitment).unwrap();
    conn.execute(
        "UPDATE votes SET commitment_bundle_json=?1, vc_tree_position=8 WHERE round_id=?2",
        rusqlite::params![base, ROUND_ID],
    )
    .unwrap();
    conn.pragma_update(None, "user_version", 17).unwrap();
    migrate(&mut conn).unwrap();
    let diagnostic: String = conn
        .query_row("SELECT diagnostic_kind FROM chain_submissions", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(diagnostic, "legacy_evidence_invalid");
}

#[test]
fn v17_invalid_atomic_batches_become_member_guards() {
    const ROUND_ID: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    let (first, second) = released_batch_recovery_json(ROUND_ID);
    let stored_commitments = [&first, &second].map(|json| {
        crate::vote::stored_vote_commitment_bytes(&crate::vote::parse_recovery(json).unwrap())
            .unwrap()
    });

    let open_batch = |first_json: Option<&str>, second_json: Option<&str>, shared_hash: bool| {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&v17_schema()).unwrap();
        let mut params = test_params();
        params.vote_round_id = ROUND_ID.to_string();
        queries::insert_round(&conn, "wallet", crate::Network::Testnet, &params, None).unwrap();
        queries::insert_bundle(&conn, ROUND_ID, "wallet", 0, &[1]).unwrap();
        for (proposal, json) in [(1, first_json), (2, second_json)] {
            queries::store_vote(
                &conn,
                ROUND_ID,
                "wallet",
                0,
                proposal,
                2,
                &stored_commitments[proposal as usize - 1],
            )
            .unwrap();
            conn.execute(
                "UPDATE votes SET commitment_bundle_json=?1,
                                      tx_hash=CASE WHEN ?2 THEN 'shared-hash' END
                      WHERE round_id=?3 AND wallet_id='wallet'
                        AND bundle_index=0 AND proposal_id=?4",
                rusqlite::params![json, shared_hash, ROUND_ID, proposal],
            )
            .unwrap();
        }
        conn.pragma_update(None, "user_version", 17).unwrap();
        conn
    };

    // A provable atomic batch binds once, as one vote_batch generation,
    // rather than as one permanently unbound guard per member.
    let mut valid = open_batch(Some(&first), Some(&second), true);
    migrate(&mut valid).unwrap();
    let (kind, state, digest) = valid
        .query_row(
            "SELECT kind, state, generation_digest IS NOT NULL FROM chain_submissions
                  WHERE kind IN ('vote','vote_batch')",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, bool>(2)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(kind, "vote_batch");
    assert_eq!(state, "recovering");
    assert!(digest, "a recovery-backed batch binds a real generation");

    let mut corruptions = Vec::new();
    for (label, field, replacement) in [
        ("duplicate index", "batch_index", serde_json::json!(0)),
        ("different size", "batch_size", serde_json::json!(1)),
        (
            "different digest",
            "batch_digest",
            serde_json::json!(vec![0x44_u8; 32]),
        ),
    ] {
        let mut value: serde_json::Value = serde_json::from_str(&second).unwrap();
        value[field] = replacement;
        corruptions.push((label, Some(first.clone()), Some(value.to_string()), false));
    }
    corruptions.push(("missing member", Some(first.clone()), None, false));
    let singleton = released_singleton_recovery_json(ROUND_ID);
    let mut second_singleton: serde_json::Value = serde_json::from_str(&singleton).unwrap();
    second_singleton["proposal_id"] = serde_json::json!(2);
    corruptions.push((
        "unreconstructable shared hash",
        Some(singleton),
        Some(second_singleton.to_string()),
        true,
    ));

    for (label, first_json, second_json, shared_hash) in corruptions {
        let mut conn = open_batch(first_json.as_deref(), second_json.as_deref(), shared_hash);
        let singleton =
            recovery_json_with_tree_position(&released_singleton_recovery_json(ROUND_ID), 8);
        let mut singleton_value: serde_json::Value = serde_json::from_str(&singleton).unwrap();
        singleton_value["proposal_id"] = serde_json::json!(3);
        let singleton = serde_json::to_string(&singleton_value).unwrap();
        let parsed_singleton = crate::vote::parse_recovery(&singleton).unwrap();
        let singleton_commitment =
            crate::vote::stored_vote_commitment_bytes(&parsed_singleton).unwrap();
        queries::store_vote(
            &conn,
            ROUND_ID,
            "wallet",
            0,
            3,
            parsed_singleton.vote_decision,
            &singleton_commitment,
        )
        .unwrap();
        conn.execute(
            "UPDATE votes SET commitment_bundle_json=?1, vc_tree_position=8
                  WHERE round_id=?2 AND wallet_id='wallet'
                    AND bundle_index=0 AND proposal_id=3",
            rusqlite::params![singleton, ROUND_ID],
        )
        .unwrap();
        conn.execute(
            "UPDATE bundles SET van_leaf_position=7
                  WHERE round_id=?1 AND wallet_id='wallet' AND bundle_index=0",
            [ROUND_ID],
        )
        .unwrap();
        migrate(&mut conn).unwrap();
        assert_eq!(
            conn.pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                .unwrap(),
            CURRENT_VERSION,
            "{label}"
        );
        let guards: Vec<(i64, String, bool, String)> = conn
            .prepare(
                "SELECT proposal_id, state, generation_digest IS NOT NULL, diagnostic_kind
                   FROM chain_submissions
                  WHERE diagnostic_kind='legacy_evidence_invalid'
                  ORDER BY proposal_id",
            )
            .unwrap()
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            guards,
            vec![
                (
                    1,
                    "recovering".to_string(),
                    false,
                    "legacy_evidence_invalid".to_string()
                ),
                (
                    2,
                    "recovering".to_string(),
                    false,
                    "legacy_evidence_invalid".to_string()
                ),
            ],
            "{label}"
        );
        let unrelated: (String, String) = conn
            .query_row(
                "SELECT state, confirmation_source FROM chain_submissions
                  WHERE kind='vote' AND proposal_id=3",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            unrelated,
            ("confirmed".to_string(), "legacy_import".to_string()),
            "{label}"
        );
        let first_dump = chain_submission_dump(&conn);
        migrate(&mut conn).unwrap();
        assert_eq!(chain_submission_dump(&conn), first_dump, "{label}");
    }
}

#[test]
fn stale_unreleased_v18_schema_is_rejected() {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE chain_submissions (state TEXT NOT NULL)")
        .unwrap();
    conn.pragma_update(None, "user_version", 18).unwrap();

    let error = migrate(&mut conn).unwrap_err();
    assert!(error
        .to_string()
        .contains("unsupported unreleased version-18"));
}

#[test]
fn v18_fingerprint_rejects_missing_columns_indexes_and_triggers() {
    fn assert_rejected(schema: &str, tamper: Option<&str>) {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(schema).unwrap();
        if let Some(tamper) = tamper {
            conn.execute_batch(tamper).unwrap();
        }
        conn.pragma_update(None, "user_version", 18).unwrap();
        let error = migrate(&mut conn).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unsupported unreleased version-18"),
            "{error}"
        );
    }

    let without_diagnostic_kind = include_str!("002_chain_submissions.sql")
        .replacen("    diagnostic_kind TEXT,\n", "", 1)
        .replacen(
            "    CHECK ((diagnostic_kind IS NULL) = (diagnostic IS NULL)),\n",
            "",
            1,
        );
    assert_rejected(&without_diagnostic_kind, None);
    assert_rejected(
        include_str!("002_chain_submissions.sql"),
        Some("DROP INDEX chain_submissions_candidate_owner"),
    );
    assert_rejected(
        include_str!("002_chain_submissions.sql"),
        Some("DROP TRIGGER chain_submissions_immutable_identity"),
    );
}

#[test]
fn test_migrate_from_prelaunch_version_resets_existing_state() {
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
    queries::insert_bundle(&conn, ROUND, "wallet", 0, &[1]).unwrap();
    conn.pragma_update(None, "user_version", 8).unwrap();

    migrate(&mut conn).unwrap();

    let version: u32 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(version, CURRENT_VERSION);

    let round_count: u32 = conn
            .query_row(
                "SELECT COUNT(*) FROM rounds WHERE round_id = '1111111111111111111111111111111111111111111111111111111111111111'",
                [],
                |row| row.get(0),
            )
            .unwrap();
    assert_eq!(round_count, 0);
}

#[test]
fn migrate_from_launch_version_preserves_delegation_state() {
    // The case this migration exists for: a wallet that already submitted a
    // delegation upgrades before voting. `van_comm_rand` is sampled randomly
    // and its governance nullifiers are spent on chain, so losing the row
    // would cost that round's voting weight permanently.
    let mut conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(&launch_schema()).unwrap();
    queries::insert_round(
        &conn,
        "wallet",
        crate::Network::Testnet,
        &test_params(),
        None,
    )
    .unwrap();
    queries::insert_bundle(&conn, ROUND, "wallet", 0, &[1]).unwrap();
    conn.execute(
            "UPDATE bundles SET van_comm_rand = ?1, gov_comm = ?2
             WHERE round_id = '1111111111111111111111111111111111111111111111111111111111111111' AND wallet_id = 'wallet' AND bundle_index = 0",
            rusqlite::params![vec![0xAB_u8; 32], vec![0xCD_u8; 32]],
        )
        .unwrap();
    conn.execute(
            "INSERT INTO share_delegations
             (round_id, wallet_id, bundle_index, proposal_id, share_index, sent_to_urls, nullifier, confirmed, submit_at, created_at)
             VALUES ('1111111111111111111111111111111111111111111111111111111111111111', 'wallet', 0, 1, 0, '[\"https://helper.example\"]', X'01', 0, 100, 90)",
            [],
        )
        .unwrap();
    conn.pragma_update(None, "user_version", LAUNCH_VERSION)
        .unwrap();

    migrate(&mut conn).unwrap();

    let version: u32 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(version, CURRENT_VERSION);

    let (van_comm_rand, gov_comm): (Vec<u8>, Vec<u8>) = conn
            .query_row(
                "SELECT van_comm_rand, gov_comm FROM bundles
                 WHERE round_id = '1111111111111111111111111111111111111111111111111111111111111111' AND wallet_id = 'wallet' AND bundle_index = 0",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
    assert_eq!(van_comm_rand, vec![0xAB; 32]);
    assert_eq!(gov_comm, vec![0xCD; 32]);

    // The round survives and gains the new column, unset.
    let stored_policy: Option<String> = conn
            .query_row(
                "SELECT bundle_policy_json FROM rounds WHERE round_id = '1111111111111111111111111111111111111111111111111111111111111111'",
                [],
                |row| row.get(0),
            )
            .unwrap();
    assert!(stored_policy.is_none());

    let delivery: (String, String, String, u32) = conn
            .query_row(
                "SELECT sent_to_urls, ambiguous_urls, attempting_urls, target_count
                 FROM share_delegations WHERE round_id = '1111111111111111111111111111111111111111111111111111111111111111' AND wallet_id = 'wallet'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
    assert_eq!(delivery.0, "[\"https://helper.example\"]");
    assert_eq!(delivery.1, "[]");
    assert_eq!(delivery.2, "[]");
    assert_eq!(delivery.3, 0);
}

#[test]
fn migrate_from_launch_version_matches_a_fresh_schema() {
    // A migrated database and a fresh one must be indistinguishable,
    // otherwise later queries work on only one of them.
    let mut migrated = Connection::open_in_memory().unwrap();
    migrated.execute_batch(&launch_schema()).unwrap();
    migrated
        .pragma_update(None, "user_version", LAUNCH_VERSION)
        .unwrap();
    migrate(&mut migrated).unwrap();

    let mut fresh = Connection::open_in_memory().unwrap();
    migrate(&mut fresh).unwrap();

    for table in [
        "rounds",
        "bundles",
        "votes",
        "helper_share_plans",
        "share_delegations",
        "pir_proof_cache",
        "chain_submissions",
    ] {
        assert_eq!(
            table_columns(&migrated, table),
            table_columns(&fresh, table),
            "column mismatch in {table}"
        );
    }
    assert_eq!(
        schema_sql(
            &migrated,
            "trigger",
            "clear_helper_share_plan_on_vote_generation_change"
        ),
        schema_sql(
            &fresh,
            "trigger",
            "clear_helper_share_plan_on_vote_generation_change"
        ),
        "migrated and fresh schemas must install the same plan lifecycle trigger"
    );
}

#[test]
fn migrate_v16_to_v17_installs_plan_lifecycle_invariants() {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(&v16_schema()).unwrap();
    conn.pragma_update(None, "user_version", 16).unwrap();

    migrate(&mut conn).unwrap();

    assert_eq!(
        conn.pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        CURRENT_VERSION
    );
    assert_helper_plan_lifecycle(&conn);
}

#[test]
fn migrate_v15_recovery_json_preserves_plan_only_through_confirmation() {
    const ROUND_ID: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    let mut conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(&v15_schema()).unwrap();
    let mut params = test_params();
    params.vote_round_id = ROUND_ID.to_string();
    queries::insert_round(&conn, "wallet", crate::Network::Testnet, &params, None).unwrap();
    queries::insert_bundle(&conn, ROUND_ID, "wallet", 0, &[1]).unwrap();
    let stored_commitment = crate::vote::stored_vote_commitment_bytes(
        &crate::vote::parse_recovery(&released_singleton_recovery_json(ROUND_ID)).unwrap(),
    )
    .unwrap();
    queries::store_vote(&conn, ROUND_ID, "wallet", 0, 1, 2, &stored_commitment).unwrap();
    queries::store_vote(&conn, ROUND_ID, "wallet", 0, 2, 1, &[0xCB; 32]).unwrap();

    let released_json = released_singleton_recovery_json(ROUND_ID);
    assert!(!released_json.contains("\"batch_digest\""));
    conn.execute(
        "UPDATE votes SET commitment_bundle_json = ?1
             WHERE round_id = ?2 AND wallet_id = 'wallet'
               AND bundle_index = 0 AND proposal_id = 1",
        rusqlite::params![released_json, ROUND_ID],
    )
    .unwrap();
    conn.pragma_update(None, "user_version", 15).unwrap();

    migrate(&mut conn).unwrap();

    let normalized =
        crate::vote::serialize_recovery(&crate::vote::parse_recovery(&released_json).unwrap())
            .unwrap();
    let stored: String = conn
        .query_row(
            "SELECT commitment_bundle_json FROM votes
                 WHERE round_id = ?1 AND wallet_id = 'wallet'
                   AND bundle_index = 0 AND proposal_id = 1",
            [ROUND_ID],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored, normalized);
    assert!(stored.contains("\"batch_digest\":null"));
    insert_helper_plan_for_round(&conn, ROUND_ID, &stored);

    let mut confirmed =
        crate::vote::parse_recovery(&stored).expect("normalized recovery must remain valid");
    confirmed.vc_tree_position = 789;
    let confirmed_json = crate::vote::serialize_recovery(&confirmed).unwrap();
    conn.execute(
        "UPDATE votes
                SET commitment_bundle_json = ?1, vc_tree_position = 789
              WHERE round_id = ?2 AND wallet_id = 'wallet'
                AND bundle_index = 0 AND proposal_id = 1",
        rusqlite::params![confirmed_json, ROUND_ID],
    )
    .unwrap();
    assert_eq!(
        stored_plan_snapshot_for_round(&conn, ROUND_ID).as_deref(),
        Some(confirmed_json.as_str())
    );

    let replacement_json = confirmed_json.replacen("\"vote_decision\":2", "\"vote_decision\":1", 1);
    assert_ne!(replacement_json, confirmed_json);
    conn.execute(
        "UPDATE votes SET commitment_bundle_json = ?1
             WHERE round_id = ?2 AND wallet_id = 'wallet'
               AND bundle_index = 0 AND proposal_id = 1",
        rusqlite::params![replacement_json, ROUND_ID],
    )
    .unwrap();
    assert_eq!(stored_plan_snapshot_for_round(&conn, ROUND_ID), None);
}

#[test]
fn fresh_schema_enforces_plan_lifecycle_invariants() {
    let mut conn = Connection::open_in_memory().unwrap();
    migrate(&mut conn).unwrap();

    assert_helper_plan_lifecycle(&conn);
}

#[test]
fn migrate_from_v14_creates_pir_proof_cache_and_preserves_state() {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(&v14_schema()).unwrap();
    queries::insert_round(
        &conn,
        "wallet",
        crate::Network::Testnet,
        &test_params(),
        None,
    )
    .unwrap();
    queries::insert_bundle(&conn, ROUND, "wallet", 0, &[1]).unwrap();
    // A cached proof from the old bundle-scoped table; v15 must carry it
    // over so an upgrade mid-round does not refetch from the PIR server.
    conn.execute(
            "INSERT INTO imt_proofs (round_id, wallet_id, bundle_index, nullifier, root, nf_bounds, leaf_pos, path, created_at)
             VALUES ('1111111111111111111111111111111111111111111111111111111111111111', 'wallet', 0, X'01', X'02', X'03', 7, X'04', 42)",
            [],
        )
        .unwrap();
    conn.pragma_update(None, "user_version", 14).unwrap();

    migrate(&mut conn).unwrap();

    let version: u32 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(version, CURRENT_VERSION);

    let round_count: u32 = conn
            .query_row(
                "SELECT COUNT(*) FROM rounds WHERE round_id = '1111111111111111111111111111111111111111111111111111111111111111'",
                [],
                |row| row.get(0),
            )
            .unwrap();
    assert_eq!(round_count, 1);

    // The old proof row was migrated, keyed by the round's network, with
    // updated_at seeded from created_at.
    let migrated_row: (String, Vec<u8>, i64, i64, i64) = conn
        .query_row(
            "SELECT network, root, leaf_pos, created_at, updated_at
                 FROM pir_proof_cache WHERE wallet_id = 'wallet' AND nullifier = X'01'",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(migrated_row, ("testnet".to_string(), vec![0x02], 7, 42, 42));

    // The old table is gone...
    assert!(!table_names(&conn).contains(&"imt_proofs".to_string()));

    // ...and the new one is usable.
    conn.execute(
            "INSERT INTO pir_proof_cache (wallet_id, network, nullifier, root, nf_bounds, leaf_pos, path, created_at, updated_at)
             VALUES ('wallet', 'testnet', X'05', X'02', X'03', 0, X'04', 0, 0)",
            [],
        )
        .unwrap();
}

#[test]
fn incremental_migrations_form_an_unbroken_chain_to_current() {
    let mut expected = LAUNCH_VERSION;
    for (from, _) in INCREMENTAL_MIGRATIONS {
        assert_eq!(
            *from, expected,
            "incremental migrations must be ordered and contiguous"
        );
        expected = from + 1;
    }
    assert_eq!(
        expected, CURRENT_VERSION,
        "every version from LAUNCH_VERSION to CURRENT_VERSION needs a migration step"
    );
}

#[test]
fn test_migrate_from_pre_v8_schema_recreates_current_schema() {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(&pre_v8_schema()).unwrap();
    conn.pragma_update(None, "user_version", 7).unwrap();

    migrate(&mut conn).unwrap();

    let columns = table_columns(&conn, "bundles");
    assert!(columns.contains(&"note_identity_hashes_blob".to_string()));
    assert!(columns.contains(&"tx1_effects".to_string()));
}

#[test]
fn test_migrate_rejects_newer_database_version() {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "user_version", CURRENT_VERSION + 1)
        .unwrap();

    let err = migrate(&mut conn).unwrap_err();
    assert!(
        err.to_string()
            .contains("unsupported newer database version"),
        "{err}"
    );
}

#[test]
fn test_tables_created() {
    let mut conn = Connection::open_in_memory().unwrap();
    migrate(&mut conn).unwrap();

    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();

    assert!(tables.contains(&"rounds".to_string()));
    assert!(tables.contains(&"bundles".to_string()));
    assert!(tables.contains(&"cached_tree_state".to_string()));
    assert!(tables.contains(&"proofs".to_string()));
    assert!(tables.contains(&"votes".to_string()));
    // Replaced by pir_proof_cache at v15.
    assert!(!tables.contains(&"imt_proofs".to_string()));
    assert!(tables.contains(&"share_delegations".to_string()));
    assert!(tables.contains(&"helper_share_plans".to_string()));
    assert!(tables.contains(&"keystone_signatures".to_string()));
    assert!(tables.contains(&"ballot_intent".to_string()));
    assert!(tables.contains(&"pir_proof_cache".to_string()));

    let round_columns = table_columns(&conn, "rounds");
    assert!(round_columns.contains(&"network".to_string()));
}

/// Verify that the bundles table columns exist after migration and can round-trip BLOB data.
#[test]
fn test_bundle_data_columns_exist() {
    let mut conn = Connection::open_in_memory().unwrap();
    migrate(&mut conn).unwrap();

    // Insert a round first
    conn.execute(
            "INSERT INTO rounds (round_id, wallet_id, network, snapshot_height, ea_pk, nc_root, nullifier_imt_root, phase, created_at) VALUES ('test', 'w1', 'testnet', 1, X'00', X'00', X'00', 0, 0)",
            [],
        ).unwrap();

    // Insert a bundle row using the delegation BLOB columns.
    conn.execute(
            "INSERT INTO bundles (round_id, wallet_id, bundle_index, van_comm_rand, dummy_nullifiers, rho_signed, padded_note_data, nf_signed, cmx_new, alpha, rseed_signed, rseed_output, tx1_effects) VALUES ('test', 'w1', 0, X'AA', X'BB', X'CC', X'DD', X'EE', X'FF', X'11', X'22', X'33', X'44')",
            [],
        ).unwrap();

    // Verify van_comm_rand round-trips (the VAN blinding factor)
    let rand: Vec<u8> = conn
        .query_row(
            "SELECT van_comm_rand FROM bundles WHERE round_id = 'test' AND bundle_index = 0",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rand, vec![0xAA]);

    // Verify dummy_nullifiers round-trips
    let dummies: Vec<u8> = conn
        .query_row(
            "SELECT dummy_nullifiers FROM bundles WHERE round_id = 'test' AND bundle_index = 0",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(dummies, vec![0xBB]);

    let tx1_effects: Vec<u8> = conn
        .query_row(
            "SELECT tx1_effects FROM bundles WHERE round_id = 'test' AND bundle_index = 0",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tx1_effects, vec![0x44]);
}

fn table_columns(conn: &Connection, table: &str) -> Vec<String> {
    conn.prepare(&format!("PRAGMA table_info({table})"))
        .unwrap()
        .query_map([], |row| row.get(1))
        .unwrap()
        .collect::<Result<Vec<String>, _>>()
        .unwrap()
}

fn table_names(conn: &Connection) -> Vec<String> {
    conn.prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<Vec<String>, _>>()
        .unwrap()
}

fn schema_sql(conn: &Connection, object_type: &str, name: &str) -> String {
    conn.query_row(
        "SELECT sql FROM sqlite_master WHERE type = ?1 AND name = ?2",
        rusqlite::params![object_type, name],
        |row| row.get(0),
    )
    .unwrap()
}

fn assert_helper_plan_lifecycle(conn: &Connection) {
    conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
    queries::insert_round(
        conn,
        "wallet",
        crate::Network::Testnet,
        &test_params(),
        None,
    )
    .unwrap();
    queries::insert_bundle(conn, ROUND, "wallet", 0, &[1]).unwrap();
    queries::store_vote(conn, ROUND, "wallet", 0, 1, 0, &[0xCA; 32]).unwrap();
    let before = r#"{"vc_tree_position":0,"marker":"same"}"#;
    conn.execute(
            "UPDATE votes SET commitment_bundle_json = ?1
             WHERE round_id = '1111111111111111111111111111111111111111111111111111111111111111' AND wallet_id = 'wallet'
               AND bundle_index = 0 AND proposal_id = 1",
            [before],
        )
        .unwrap();
    insert_helper_plan(conn, before);

    let confirmed = r#"{"vc_tree_position":7,"marker":"same"}"#;
    conn.execute(
            "UPDATE votes
                SET commitment_bundle_json = ?1, vc_tree_position = 7
              WHERE round_id = '1111111111111111111111111111111111111111111111111111111111111111' AND wallet_id = 'wallet'
                AND bundle_index = 0 AND proposal_id = 1",
            [confirmed],
        )
        .unwrap();
    assert_eq!(stored_plan_snapshot(conn).as_deref(), Some(confirmed));

    // A non-confirmation recovery-material change is a new generation,
    // even when it retains the already-confirmed VC position.
    let replacement = r#"{"vc_tree_position":7,"marker":"replacement"}"#;
    conn.execute(
            "UPDATE votes SET commitment_bundle_json = ?1
             WHERE round_id = '1111111111111111111111111111111111111111111111111111111111111111' AND wallet_id = 'wallet'
               AND bundle_index = 0 AND proposal_id = 1",
            [replacement],
        )
        .unwrap();
    assert_eq!(stored_plan_snapshot(conn), None);

    insert_helper_plan(conn, replacement);
    conn.execute(
            "DELETE FROM votes
             WHERE round_id = '1111111111111111111111111111111111111111111111111111111111111111' AND wallet_id = 'wallet'
               AND bundle_index = 0 AND proposal_id = 1",
            [],
        )
        .unwrap();
    assert_eq!(stored_plan_snapshot(conn), None);
}

fn insert_helper_plan(conn: &Connection, snapshot: &str) {
    insert_helper_plan_for_round(conn, ROUND, snapshot);
}

fn insert_helper_plan_for_round(conn: &Connection, round_id: &str, snapshot: &str) {
    conn.execute(
        "INSERT INTO helper_share_plans
             (round_id, wallet_id, bundle_index, proposal_id,
              commitment_bundle_json, configured_server_urls_json,
              share_plans_json, format_version, placement_guarantee, created_at)
             VALUES (?1, 'wallet', 0, 1, ?2, '[\"https://helper.example\"]',
                     '[]', 1, 'strict', 1)",
        rusqlite::params![round_id, snapshot],
    )
    .unwrap();
}

fn released_singleton_recovery_json(round_id: &str) -> String {
    let released_shape = serde_json::to_string(&serde_json::json!({
        "format": "zcash_voting_vote_recovery_v1",
        "vote_round_id": round_id,
        "bundle_index": 0,
        "proposal_id": 1,
        "vote_decision": 2,
        "anchor_height": 100,
        "vc_tree_position": 0,
        "single_share": false,
        "num_options": 3,
        "van_nullifier": vec![0x31_u8; 32],
        "vote_authority_note_new": vec![0x32_u8; 32],
        "vote_commitment": vec![0x33_u8; 32],
        "proof": vec![0x34_u8; 8],
        "shares_hash": vec![0x35_u8; 32],
        "r_vpk": vec![0x36_u8; 32],
        "alpha_v": vec![0x37_u8; 32],
        "vote_auth_sig": vec![0x38_u8; 64],
        "encrypted_shares": [],
        "share_blinds": [],
        "share_comms": [],
    }))
    .unwrap();
    let canonical_with_batch_nulls =
        crate::vote::serialize_recovery(&crate::vote::parse_recovery(&released_shape).unwrap())
            .unwrap();
    canonical_with_batch_nulls
        .strip_suffix(",\"batch_digest\":null,\"batch_index\":null,\"batch_size\":null}")
        .map(|prefix| format!("{prefix}}}"))
        .expect("current singleton recovery must append nullable batch metadata")
}

fn released_batch_recovery_json(round_id: &str) -> (String, String) {
    let mut first =
        crate::vote::parse_recovery(&released_singleton_recovery_json(round_id)).unwrap();
    let mut second = first.clone();
    second.proposal_id = 2;
    second.vc_tree_position = 1;
    second.van_nullifier = [0x41; 32];
    second.vote_authority_note_new = [0x42; 32];
    second.vote_commitment = [0x43; 32];
    second.r_vpk = [0x44; 32];
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
        round_id,
        u64::from(first.anchor_height),
        &actions,
    )
    .unwrap();
    first.batch = Some(crate::vote::VoteBatchRecovery {
        digest,
        index: 0,
        size: 2,
    });
    second.batch = Some(crate::vote::VoteBatchRecovery {
        digest,
        index: 1,
        size: 2,
    });
    (
        crate::vote::serialize_recovery(&first).unwrap(),
        crate::vote::serialize_recovery(&second).unwrap(),
    )
}

fn recovery_json_with_tree_position(json: &str, position: i64) -> String {
    let mut recovery: serde_json::Value = serde_json::from_str(json).unwrap();
    recovery["vc_tree_position"] = serde_json::json!(position);
    serde_json::to_string(&recovery).unwrap()
}

fn stored_plan_snapshot(conn: &Connection) -> Option<String> {
    stored_plan_snapshot_for_round(conn, ROUND)
}

fn stored_plan_snapshot_for_round(conn: &Connection, round_id: &str) -> Option<String> {
    conn.query_row(
        "SELECT commitment_bundle_json FROM helper_share_plans
             WHERE round_id = ?1 AND wallet_id = 'wallet'
               AND bundle_index = 0 AND proposal_id = 1",
        [round_id],
        |row| row.get(0),
    )
    .optional()
    .unwrap()
}
