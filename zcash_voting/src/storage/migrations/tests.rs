use super::*;
use crate::storage::queries;
use crate::VotingRoundParams;
use rusqlite::OptionalExtension;

fn pre_v8_schema() -> String {
    include_str!("001_init.sql")
        .split("-- Immutable public delegation authorization")
        .next()
        .unwrap()
        .replace("    note_identity_hashes_blob BLOB,\n", "")
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

/// Strips the round immediate-share designation added at version 20.
fn without_round_immediate_share(schema: &str) -> String {
    let start = schema
        .find("-- Round-wide immediate helper share designation")
        .expect("schema must contain the designation added at version 20");
    schema[..start].to_string()
}

/// Strips the combined-cast rejection ledger added at version 24. No database
/// at version 23 or earlier can carry it.
fn without_combined_cast_rejections(schema: &str) -> String {
    let start = schema
        .find("-- Consecutive chain rejections of one bundle's combined")
        .expect("schema must contain the ledger added at version 24");
    schema[..start].trim_end().to_string() + "\n"
}

fn v23_schema() -> String {
    without_combined_cast_rejections(include_str!("001_init.sql"))
}

fn v22_schema() -> String {
    v23_schema()
        .split("-- Immutable public delegation authorization")
        .next()
        .unwrap()
        .replace(", 'delegate_and_cast_vote_batch'", "")
        .replace(",'delegate_and_cast_vote_batch'", "")
}

fn v21_schema() -> String {
    v22_schema().replace("    delegation_pczt     BLOB,\n", "")
}

/// The complete version-19 schema: everything but the designation row.
fn v19_schema() -> String {
    without_round_immediate_share(&v21_schema())
}

/// The complete version-20 schema as a build carrying the 15-proposal bound
/// left it: everything `001_init.sql` creates, with `chain_submissions` still
/// refusing proposal ids above 15.
fn v20_legacy_proposal_bound_schema() -> String {
    v21_schema().replace(
        "proposal_id BETWEEN 1 AND 50",
        "proposal_id BETWEEN 1 AND 15",
    )
}

fn v16_schema() -> String {
    without_helper_share_plans(&without_chain_submissions(&v21_schema()))
}

fn v17_schema() -> String {
    without_chain_submissions(&v21_schema())
}

/// The `chain_submissions` DDL exactly as shipped at version 18: no
/// `submitted_without_hash` state and the original 15-proposal bound.
fn v18_chain_submission_schema() -> String {
    include_str!("002_chain_submissions.sql")
        .replace(
            "'recovering','submitted_without_hash','confirmed'",
            "'recovering','confirmed'",
        )
        .replace(
            "proposal_id BETWEEN 1 AND 50",
            "proposal_id BETWEEN 1 AND 15",
        )
        .replace(
            "    CHECK (state != 'submitted_without_hash'\n        OR (candidate_transaction_hash IS NULL\n            AND confirmed_transaction_hash IS NULL AND final_van_position IS NULL\n            AND vote_commitment_positions IS NULL AND diagnostic_kind IS NOT NULL)),\n",
            "",
        )
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
fn v21_adds_pczt_storage_without_rewriting_delegation_state() {
    for preview_has_pczt in [false, true] {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&v21_schema()).unwrap();
        if preview_has_pczt {
            conn.execute_batch("ALTER TABLE bundles ADD COLUMN delegation_pczt BLOB;")
                .unwrap();
        }
        queries::insert_round(
            &conn,
            "wallet",
            crate::Network::Testnet,
            &test_params(),
            None,
        )
        .unwrap();
        insert_v17_delegation_with_complete_setup(&conn, ROUND, "wallet", 0, None);
        if preview_has_pczt {
            conn.execute("UPDATE bundles SET delegation_pczt = X'010203'", [])
                .unwrap();
        }
        let mut bundles = dump_table(&conn, "bundles");
        if !preview_has_pczt {
            for bundle in &mut bundles {
                bundle.push("Null".to_string());
            }
        }
        let proofs = dump_table(&conn, "proofs");
        let signatures = dump_table(&conn, "keystone_signatures");
        conn.pragma_update(None, "user_version", 21).unwrap();
        migrate(&mut conn).unwrap();
        migrate(&mut conn).unwrap();
        assert_eq!(dump_table(&conn, "bundles"), bundles);
        assert_eq!(dump_table(&conn, "proofs"), proofs);
        assert_eq!(dump_table(&conn, "keystone_signatures"), signatures);
        assert_eq!(
            conn.pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                .unwrap(),
            CURRENT_VERSION
        );
    }
}

/// Version 18 adds the lifecycle table by DDL only.
///
/// Every version-17 domain column stays byte-identical, and no
/// `chain_submissions` row is created for any pre-upgrade evidence, whatever
/// its shape: a fully confirmed vote, a hash-only vote, a delegation-only
/// bundle, or committed recovery material.
#[test]
fn a_busy_schema_transaction_stays_retryable() {
    // The sidecar open path retries only `DbBusy`, so a `SQLITE_BUSY` raised
    // while the migration takes the write lock must not be flattened into
    // `Internal` — that made a concurrent open of an older sidecar fail
    // outright instead of retrying.
    let temp = v17_file(|_| {});
    let holder = Connection::open(temp.path()).unwrap();
    holder.execute_batch("BEGIN IMMEDIATE").unwrap();

    let mut contender = Connection::open(temp.path()).unwrap();
    // Without a busy timeout the contended write lock fails immediately, which
    // is the classification this test is about, not the waiting behavior.
    contender.busy_timeout(std::time::Duration::ZERO).unwrap();

    let error = migrate(&mut contender).unwrap_err();

    assert_eq!(error.kind(), crate::VotingErrorKind::DbBusy);
    assert!(error.retryable(), "{error}");
}

#[test]
fn v17_domain_evidence_is_preserved_and_creates_no_submission_rows() {
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
    for bundle in 0..=3 {
        queries::insert_bundle(&conn, ROUND, "wallet", bundle, &[1]).unwrap();
    }
    // Bundle 0: a completed vote with every domain column filled.
    queries::store_vote(&conn, ROUND, "wallet", 0, 1, 2, &[1; 32]).unwrap();
    conn.execute(
        "UPDATE bundles SET delegation_tx_hash='dtx-0', van_leaf_position=7
              WHERE round_id=?1 AND wallet_id='wallet' AND bundle_index=0",
        [ROUND],
    )
    .unwrap();
    conn.execute(
        "UPDATE votes SET tx_hash='vtx-0', vc_tree_position=8
              WHERE round_id=?1 AND wallet_id='wallet' AND bundle_index=0",
        [ROUND],
    )
    .unwrap();
    // Bundle 1: a vote with only a historical hash.
    queries::store_vote(&conn, ROUND, "wallet", 1, 2, 1, &[2; 32]).unwrap();
    conn.execute(
        "UPDATE votes SET tx_hash='vtx-1'
              WHERE round_id=?1 AND wallet_id='wallet' AND bundle_index=1",
        [ROUND],
    )
    .unwrap();
    // Bundle 2: delegation evidence only.
    conn.execute(
        "UPDATE bundles SET delegation_tx_hash='dtx-2', van_leaf_position=9
              WHERE round_id=?1 AND wallet_id='wallet' AND bundle_index=2",
        [ROUND],
    )
    .unwrap();
    // Bundle 3: committed recovery material without chain evidence.
    let recovery = released_singleton_recovery_json(ROUND);
    queries::store_vote(&conn, ROUND, "wallet", 3, 1, 2, &[3; 32]).unwrap();
    conn.execute(
        "UPDATE votes SET commitment_bundle_json=?1
              WHERE round_id=?2 AND wallet_id='wallet' AND bundle_index=3",
        rusqlite::params![recovery, ROUND],
    )
    .unwrap();
    conn.pragma_update(None, "user_version", 17).unwrap();
    let votes_before = dump_table(&conn, "votes");
    let mut bundles_before = dump_table(&conn, "bundles");
    // The new nullable PCZT column is appended without changing older fields.
    for bundle in &mut bundles_before {
        bundle.push("Null".to_string());
    }

    migrate(&mut conn).unwrap();

    assert_eq!(
        conn.query_row("SELECT count(*) FROM chain_submissions", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(dump_table(&conn, "votes"), votes_before);
    assert_eq!(dump_table(&conn, "bundles"), bundles_before);
    let version: u32 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, CURRENT_VERSION);
}

/// After migration, pre-upgrade votes project their phase from the domain
/// columns, which is what keeps a completed round displayable.
#[test]
fn migrated_v17_votes_project_domain_phases() {
    let temp = v17_file(|conn| {
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
        let recovery = released_singleton_recovery_json(ROUND);
        let confirmed_recovery = recovery_json_with_tree_position(&recovery, 8);
        queries::store_vote(conn, ROUND, "wallet", 0, 1, 2, &[1; 32]).unwrap();
        conn.execute(
            "UPDATE votes SET tx_hash='vtx-0', vc_tree_position=8, commitment_bundle_json=?1
                  WHERE round_id=?2 AND wallet_id='wallet' AND bundle_index=0",
            rusqlite::params![confirmed_recovery, ROUND],
        )
        .unwrap();
        conn.execute(
            "UPDATE bundles SET delegation_tx_hash='dtx-0', van_leaf_position=7
                  WHERE round_id=?1 AND wallet_id='wallet' AND bundle_index=0",
            [ROUND],
        )
        .unwrap();
        queries::store_vote(conn, ROUND, "wallet", 1, 1, 2, &[2; 32]).unwrap();
        conn.execute(
            "UPDATE votes SET tx_hash='vtx-1', commitment_bundle_json=?1
                  WHERE round_id=?2 AND wallet_id='wallet' AND bundle_index=1",
            rusqlite::params![recovery, ROUND],
        )
        .unwrap();
        queries::store_vote(conn, ROUND, "wallet", 2, 1, 2, &[3; 32]).unwrap();
        conn.execute(
            "UPDATE votes SET commitment_bundle_json=?1
                  WHERE round_id=?2 AND wallet_id='wallet' AND bundle_index=2",
            rusqlite::params![recovery, ROUND],
        )
        .unwrap();
    });
    let db = crate::storage::VotingDb::open(temp.path()).unwrap();
    db.set_wallet_id("wallet");

    assert_eq!(
        db.vote_phase(ROUND, 0, 1).unwrap(),
        crate::phases::VotePhase::Confirmed
    );
    assert_eq!(
        db.vote_phase(ROUND, 1, 1).unwrap(),
        crate::phases::VotePhase::Submitted
    );
    assert_eq!(
        db.vote_phase(ROUND, 2, 1).unwrap(),
        crate::phases::VotePhase::Committed
    );
    assert_eq!(
        db.delegation_phase(ROUND, 0).unwrap(),
        crate::phases::DelegationPhase::Confirmed
    );
    let snapshot = crate::recovery::round_snapshot(&db, ROUND).unwrap();
    let confirmed = snapshot
        .votes
        .iter()
        .find(|vote| vote.bundle_index == 0)
        .unwrap();
    assert_eq!(confirmed.choice, 2);
    assert_eq!(confirmed.tx_hash.as_deref(), Some("vtx-0"));
    assert_eq!(confirmed.vc_tree_position, Some(8));
    assert_eq!(snapshot.delegation[0].tx_hash.as_deref(), Some("dtx-0"));
    assert_eq!(snapshot.delegation[0].van_leaf_position, Some(7));
}

/// The version-18 schema admits only rows bound to a generation.
///
/// `NOT NULL` is what rejects the null digest: SQLite treats a NULL CHECK
/// result as passing, so the length CHECK alone would not.
#[test]
fn fresh_current_schema_requires_a_bound_generation() {
    let mut conn = Connection::open_in_memory().unwrap();
    migrate(&mut conn).unwrap();
    queries::insert_round(
        &conn,
        "wallet",
        crate::Network::Testnet,
        &test_params(),
        None,
    )
    .unwrap();
    let insert = |digest: Option<Vec<u8>>, state: &str, source: Option<&str>| {
        conn.execute(
            "INSERT INTO chain_submissions
             (identity_key, round_id, wallet_id, network, bundle_index, kind,
              proposal_id, generation_digest, state, committed_post_reservations,
              diagnostic_kind, diagnostic, confirmation_source,
              final_van_position, vote_commitment_positions, created_at, updated_at)
             VALUES (?1, ?2, 'wallet', 'testnet', 0, 'vote', 1, ?3, ?4, 0,
                     CASE WHEN ?4='recovering' THEN 'reconciliation_pending' END,
                     CASE WHEN ?4='recovering' THEN 'pending' END,
                     ?5,
                     CASE WHEN ?4='confirmed' THEN 7 END,
                     CASE WHEN ?4='confirmed' THEN ?6 END, 9, 9)",
            rusqlite::params![
                vec![0x41_u8; 32],
                ROUND,
                digest,
                state,
                source,
                [vec![1, 0, 0, 0, 1], 8_u64.to_be_bytes().to_vec()].concat(),
            ],
        )
    };

    let null_digest = insert(None, "recovering", None).unwrap_err();
    assert!(is_constraint_violation(&null_digest), "{null_digest}");
    let short_digest = insert(Some(vec![0x42; 16]), "recovering", None).unwrap_err();
    assert!(is_constraint_violation(&short_digest), "{short_digest}");
    for legacy_source in ["legacy_projection", "legacy_import"] {
        let error = insert(Some(vec![0x42; 32]), "confirmed", Some(legacy_source)).unwrap_err();
        assert!(is_constraint_violation(&error), "{legacy_source}: {error}");
    }
    insert(Some(vec![0x42; 32]), "recovering", None).unwrap();
}

#[test]
fn fresh_and_migrated_v18_schemas_accept_supported_singleton_proposals() {
    let mut fresh = Connection::open_in_memory().unwrap();
    migrate(&mut fresh).unwrap();

    let mut migrated = Connection::open_in_memory().unwrap();
    migrated.execute_batch(&v17_schema()).unwrap();
    migrated.pragma_update(None, "user_version", 17).unwrap();
    migrate(&mut migrated).unwrap();

    for (schema_kind, conn) in [("fresh", fresh), ("migrated", migrated)] {
        queries::insert_round(
            &conn,
            "wallet",
            crate::Network::Testnet,
            &test_params(),
            None,
        )
        .unwrap();
        let insert = |identity_byte: u8, proposal_id: u32| {
            conn.execute(
                "INSERT INTO chain_submissions
                 (identity_key, round_id, wallet_id, network, bundle_index, kind,
                  proposal_id, generation_digest, state, committed_post_reservations,
                  created_at, updated_at)
                 VALUES (?1, ?2, 'wallet', 'testnet', 0, 'vote', ?3, ?4,
                         'submitting', 1, 9, 9)",
                rusqlite::params![
                    vec![identity_byte; 32],
                    ROUND,
                    i64::from(proposal_id),
                    vec![0x42_u8; 32],
                ],
            )
        };

        insert(0x50, crate::types::MAX_PROPOSAL_ID).unwrap_or_else(|error| {
            panic!("{schema_kind} schema rejected the maximum proposal: {error}")
        });
        let error = insert(0x51, crate::types::MAX_PROPOSAL_ID + 1).unwrap_err();
        assert!(is_constraint_violation(&error), "{schema_kind}: {error}");
    }
}

#[test]
fn v20_legacy_proposal_bound_migrates_and_keeps_its_rows() {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(&v20_legacy_proposal_bound_schema())
        .unwrap();
    conn.pragma_update(None, "user_version", 20).unwrap();
    queries::insert_round(
        &conn,
        "wallet",
        crate::Network::Testnet,
        &test_params(),
        None,
    )
    .unwrap();
    let insert = |conn: &Connection, identity_byte: u8, proposal_id: u32| {
        conn.execute(
            "INSERT INTO chain_submissions
             (identity_key, round_id, wallet_id, network, bundle_index, kind,
              proposal_id, generation_digest, state, committed_post_reservations,
              created_at, updated_at)
             VALUES (?1, ?2, 'wallet', 'testnet', 0, 'vote', ?3, ?4,
                     'submitting', 1, 9, 9)",
            rusqlite::params![
                vec![identity_byte; 32],
                ROUND,
                i64::from(proposal_id),
                vec![0x42_u8; 32],
            ],
        )
    };
    insert(&conn, 0x60, 3).unwrap();
    let before = dump_table(&conn, "chain_submissions");
    // The stale bound is what makes this database unopenable today.
    assert!(is_constraint_violation(
        &insert(&conn, 0x61, 20).unwrap_err()
    ));

    migrate(&mut conn).unwrap();

    let version: u32 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(version, CURRENT_VERSION);
    assert_eq!(dump_table(&conn, "chain_submissions"), before);
    // The bound is whatever this build declares; the migration's job is to make
    // the table agree with it, not to hold a fixed number.
    insert(&conn, 0x61, crate::types::MAX_PROPOSAL_ID)
        .expect("migrated schema must accept the declared maximum proposal");
    let error = insert(&conn, 0x62, crate::types::MAX_PROPOSAL_ID + 1).unwrap_err();
    assert!(is_constraint_violation(&error), "{error}");
    // A second open must still pass the fingerprint check.
    migrate(&mut conn).unwrap();
}

/// A version-20 sidecar that is also missing an index or a trigger upgrades
/// through the ladder and comes out repaired.
///
/// The rebuild in migration 005 drops both before it recreates them, so an
/// unconditional drop would abort the upgrade on the way to the repair that
/// would have fixed it.
#[test]
fn v20_missing_indexes_and_triggers_upgrade_and_are_repaired() {
    for tamper in [
        "DROP INDEX chain_submissions_candidate_owner",
        "DROP INDEX chain_submissions_identity",
        "DROP TRIGGER chain_submissions_immutable_identity",
        "DROP TRIGGER chain_submissions_monotonic_reservations",
    ] {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&v20_legacy_proposal_bound_schema())
            .unwrap();
        conn.execute_batch(tamper).unwrap();
        conn.pragma_update(None, "user_version", 20).unwrap();

        migrate(&mut conn).unwrap_or_else(|error| panic!("{tamper}: {error}"));

        let version: u32 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, CURRENT_VERSION, "{tamper}");
        // The fingerprint agrees again, so a second open is a no-op.
        migrate(&mut conn).unwrap_or_else(|error| panic!("{tamper}: {error}"));
    }
}

#[test]
fn v18_submission_rows_migrate_incrementally_to_v19() {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(&v17_schema()).unwrap();
    conn.execute_batch(&v18_chain_submission_schema()).unwrap();
    queries::insert_round(
        &conn,
        "wallet",
        crate::Network::Testnet,
        &test_params(),
        None,
    )
    .unwrap();
    conn.execute(
        "INSERT INTO chain_submissions
         (identity_key, round_id, wallet_id, network, bundle_index, kind,
          proposal_id, generation_digest, state, committed_post_reservations,
          tracking_started_at, diagnostic_kind, diagnostic, created_at, updated_at)
         VALUES (?1, ?2, 'wallet', 'testnet', 0, 'vote', 1, ?3,
                 'recovering', 7, 8, 'ambiguous_dispatch', 'timeout', 9, 10)",
        rusqlite::params![vec![0x41_u8; 32], ROUND, vec![0x42_u8; 32]],
    )
    .unwrap();
    // The shipped v18 bound admits proposal 15 and rejects 16.
    let insert_v18_proposal = |identity_byte: u8, proposal_id: i64| {
        conn.execute(
            "INSERT INTO chain_submissions
             (identity_key, round_id, wallet_id, network, bundle_index, kind,
              proposal_id, generation_digest, state, committed_post_reservations,
              created_at, updated_at)
             VALUES (?1, ?2, 'wallet', 'testnet', 0, 'vote', ?3, ?4,
                     'submitting', 1, 9, 9)",
            rusqlite::params![
                vec![identity_byte; 32],
                ROUND,
                proposal_id,
                vec![0x42_u8; 32]
            ],
        )
    };
    insert_v18_proposal(0x43, 15).unwrap();
    assert!(is_constraint_violation(
        &insert_v18_proposal(0x44, 16).unwrap_err()
    ));
    conn.pragma_update(None, "user_version", 18).unwrap();

    migrate(&mut conn).unwrap();
    // Reopening a migrated database runs the current-schema fingerprint
    // check against it; a migration that drifted from the fresh DDL fails here.
    migrate(&mut conn).unwrap();
    assert_eq!(
        chain_submission_schema_fingerprint(&conn).unwrap(),
        chain_submission_schema_fingerprint(&{
            let mut fresh = Connection::open_in_memory().unwrap();
            migrate(&mut fresh).unwrap();
            fresh
        })
        .unwrap()
    );

    // The rebuild widened the bound: proposal 15 survived and 50 is accepted.
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM chain_submissions WHERE proposal_id = 15",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap(),
        1
    );
    conn.execute(
        "INSERT INTO chain_submissions
         (identity_key, round_id, wallet_id, network, bundle_index, kind,
          proposal_id, generation_digest, state, committed_post_reservations,
          created_at, updated_at)
         VALUES (?1, ?2, 'wallet', 'testnet', 1, 'vote', ?4, ?3,
                 'submitting', 1, 9, 9)",
        rusqlite::params![
            vec![0x45_u8; 32],
            ROUND,
            vec![0x42_u8; 32],
            // The declared maximum, so the row stays valid whichever bound
            // this build carries. Its own bundle, because the row above
            // already claims that proposal on bundle 0.
            crate::types::MAX_PROPOSAL_ID
        ],
    )
    .unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT state, committed_post_reservations, tracking_started_at, diagnostic
               FROM chain_submissions
              WHERE identity_key = ?1",
            [vec![0x41_u8; 32]],
            |row| Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?
            )),
        )
        .unwrap(),
        ("recovering".to_string(), 7, 8, "timeout".to_string())
    );
    conn.execute(
        "UPDATE chain_submissions
            SET state='submitted_without_hash',
                diagnostic_kind='ambiguous_attempts_exhausted',
                diagnostic='exhausted'
          WHERE identity_key=?1",
        [vec![0x41_u8; 32]],
    )
    .unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT state, tracking_started_at FROM chain_submissions
              WHERE identity_key = ?1",
            [vec![0x41_u8; 32]],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .unwrap(),
        ("submitted_without_hash".to_string(), 8)
    );
}

fn is_constraint_violation(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(failure, _)
            if failure.code == rusqlite::ErrorCode::ConstraintViolation
    )
}

/// Serializes every row of one table so before/after images can be compared.
fn dump_table(conn: &Connection, table: &str) -> Vec<Vec<String>> {
    let mut statement = conn.prepare(&format!("SELECT * FROM {table}")).unwrap();
    let column_count = statement.column_count();
    statement
        .query_map([], |row| {
            (0..column_count)
                .map(|index| row.get_ref(index).map(|value| format!("{value:?}")))
                .collect::<Result<Vec<_>, _>>()
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

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

    let padded = vec![0x02u8; 32 * (crate::governance::BUNDLE_NOTE_SLOTS - 1)];
    let secrets = vec![0x03u8; 64 * (crate::governance::BUNDLE_NOTE_SLOTS - 1)];
    conn.execute(
        "UPDATE bundles SET van_comm_rand=?1, dummy_nullifiers=?2,
         rho_signed=?3, padded_note_data=?2, nf_signed=?4, cmx_new=?5,
         alpha=?6, rseed_signed=?7, rseed_output=?8, gov_comm=?9,
         total_note_value=1, address_index=0, padded_note_secrets=?10,
         pczt_sighash=?11, tx1_effects=?12, rk=?13, gov_nullifiers_blob=?14
         WHERE round_id=?15 AND wallet_id=?16 AND bundle_index=?17",
        rusqlite::params![
            vec![6u8; 32],
            padded,
            vec![7u8; 32],
            vec![8u8; 32],
            vec![9u8; 32],
            vec![10u8; 32],
            vec![11u8; 32],
            vec![12u8; 32],
            vec![13u8; 32],
            secrets,
            vec![14u8; 32],
            crate::tx1::placeholder_tx1_effects(),
            vec![15u8; 32],
            vec![5u8; 32 * crate::governance::BUNDLE_NOTE_SLOTS],
            round_id,
            wallet_id,
            bundle_index
        ],
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

#[test]
fn v17_projectionless_proved_delegation_remains_fresh_work() {
    let temp = v17_file(|conn| {
        queries::insert_round(
            conn,
            "wallet",
            crate::Network::Testnet,
            &test_params(),
            None,
        )
        .unwrap();
        insert_v17_delegation_with_complete_setup(conn, ROUND, "wallet", 0, None);
        conn.execute(
            "UPDATE bundles SET delegation_tx_hash=NULL
              WHERE round_id=?1 AND wallet_id='wallet' AND bundle_index=0",
            [ROUND],
        )
        .unwrap();
    });
    let db = crate::storage::VotingDb::open(temp.path()).unwrap();
    db.set_wallet_id("wallet");
    db.set_ballot_intent(ROUND, 1, crate::session::Decision::Choice(1), 2)
        .unwrap();

    assert_eq!(
        db.conn()
            .query_row("SELECT count(*) FROM chain_submissions", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
        0
    );
    assert_eq!(
        db.delegation_phase(ROUND, 0).unwrap(),
        crate::phases::DelegationPhase::Proved
    );
    let plan = crate::session::resume_plan(&db, ROUND, &[1]).unwrap();
    assert!(!plan
        .next_steps
        .contains(&crate::session::NextStep::Delegate { bundle_index: 0 }));
}

#[test]
fn stale_current_schema_is_rejected() {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE chain_submissions (state TEXT NOT NULL)")
        .unwrap();
    conn.pragma_update(None, "user_version", CURRENT_VERSION)
        .unwrap();

    let error = migrate(&mut conn).unwrap_err();
    assert!(error.to_string().contains(&format!(
        "unsupported chain-submission schema for version {CURRENT_VERSION}"
    )));
}

#[test]
fn current_fingerprint_rejects_missing_columns_indexes_and_triggers() {
    fn assert_rejected(schema: &str, tamper: Option<&str>) {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(schema).unwrap();
        if let Some(tamper) = tamper {
            conn.execute_batch(tamper).unwrap();
        }
        conn.pragma_update(None, "user_version", CURRENT_VERSION)
            .unwrap();
        let error = migrate(&mut conn).unwrap_err();
        assert!(
            error.to_string().contains(&format!(
                "unsupported chain-submission schema for version {CURRENT_VERSION}"
            )),
            "{error}"
        );
    }

    let without_diagnostic_kind = include_str!("002_chain_submissions.sql")
        .replacen("    diagnostic_kind TEXT,\n", "", 1)
        .replacen(
            "    CHECK ((diagnostic_kind IS NULL) = (diagnostic IS NULL)),\n",
            "",
            1,
        )
        .replacen(
            "    CHECK (state != 'submitted_without_hash'\n        OR (candidate_transaction_hash IS NULL\n            AND confirmed_transaction_hash IS NULL AND final_van_position IS NULL\n            AND vote_commitment_positions IS NULL AND diagnostic_kind IS NOT NULL)),\n",
            "",
            1,
        );
    // A missing column is drift the repair cannot resolve honestly: it has no
    // value to carry across, so this stays fatal.
    assert_rejected(&without_diagnostic_kind, None);

    // A missing index or trigger is not. The columns are intact, so the table
    // is rebuilt from the current DDL with every row carried across, which
    // restores the enforcement that went missing instead of stranding the
    // wallet on a database it can no longer open.
    fn assert_repaired(tamper: &str) {
        // The full schema, because the rebuild carries rows across a table
        // whose foreign key must still resolve.
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(include_str!("001_init.sql")).unwrap();
        conn.execute_batch(tamper).unwrap();
        conn.pragma_update(None, "user_version", CURRENT_VERSION)
            .unwrap();

        migrate(&mut conn).expect("intact columns must be repaired, not refused");

        // Repaired means the fingerprint matches again, so a second open is a
        // no-op rather than another rebuild.
        migrate(&mut conn).expect("a repaired schema stays repaired");
    }

    assert_repaired("DROP INDEX chain_submissions_candidate_owner");
    assert_repaired("DROP TRIGGER chain_submissions_immutable_identity");
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
        "round_immediate_share",
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
    for trigger in [
        "round_immediate_share_immutable",
        "clear_round_immediate_share_on_vote_generation_change",
    ] {
        assert_eq!(
            schema_sql(&migrated, "trigger", trigger),
            schema_sql(&fresh, "trigger", trigger),
            "migrated and fresh schemas must install the same designation trigger {trigger}"
        );
    }
    assert_eq!(
        chain_submission_schema_fingerprint(&migrated).unwrap(),
        chain_submission_schema_fingerprint(&fresh).unwrap(),
        "migrated and fresh chain-submission schemas must share one fingerprint"
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
fn v19_immediate_markers_backfill_to_v20() {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(&v19_schema()).unwrap();
    conn.pragma_update(None, "user_version", 19).unwrap();
    queries::insert_round(
        &conn,
        "wallet",
        crate::Network::Testnet,
        &test_params(),
        None,
    )
    .unwrap();
    queries::insert_bundle(&conn, ROUND, "wallet", 0, &[1]).unwrap();
    for proposal_id in [1u32, 2] {
        queries::store_vote(&conn, ROUND, "wallet", 0, proposal_id, 0, &[0xCA; 32]).unwrap();
        conn.execute(
            "UPDATE votes SET commitment_bundle_json = '{\"vc_tree_position\":0}'
             WHERE round_id = ?1 AND wallet_id = 'wallet' AND bundle_index = 0 AND proposal_id = ?2",
            rusqlite::params![ROUND, proposal_id],
        )
        .unwrap();
    }
    // Only proposal 1's plan carries the version-19 marker, on its first share.
    for (proposal_id, plans) in [
        (
            1,
            r#"[{"immediate":true,"submit_at":0},{"immediate":false,"submit_at":9}]"#,
        ),
        (
            2,
            r#"[{"immediate":false,"submit_at":5},{"immediate":false,"submit_at":9}]"#,
        ),
    ] {
        conn.execute(
            "INSERT INTO helper_share_plans
             (round_id, wallet_id, bundle_index, proposal_id, commitment_bundle_json,
              configured_server_urls_json, share_plans_json, format_version,
              placement_guarantee, created_at)
             VALUES (?1, 'wallet', 0, ?2, '{\"vc_tree_position\":0}', '[]', ?3, 1, 'strict', 42)",
            rusqlite::params![ROUND, proposal_id, plans],
        )
        .unwrap();
    }

    migrate(&mut conn).unwrap();

    let designation: (i64, i64, i64, i64) = conn
        .query_row(
            "SELECT bundle_index, proposal_id, share_index, designated_at
             FROM round_immediate_share WHERE round_id = ?1 AND wallet_id = 'wallet'",
            [ROUND],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(designation, (0, 1, 0, 42));
    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM round_immediate_share", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(rows, 1, "one designation per round");
    let error = conn
        .execute(
            "UPDATE round_immediate_share SET proposal_id = 2 WHERE round_id = ?1",
            [ROUND],
        )
        .unwrap_err();
    assert!(error.to_string().contains("immutable"), "{error}");
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

/// Opt-in probe: repairs a real sidecar named by `REPAIR_PROBE_DB`.
///
/// Ignored by default because it needs a file from a real wallet, and a no-op
/// when that file is not named: CI runs the ignored tests too, so a probe that
/// demanded its input would fail every run that has nothing to probe.
#[test]
#[ignore]
fn repairs_a_real_sidecar_named_by_the_environment() {
    let Ok(path) = std::env::var("REPAIR_PROBE_DB") else {
        return;
    };
    let mut conn = Connection::open(&path).unwrap();
    conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
    let before: i64 = conn
        .query_row("SELECT COUNT(*) FROM chain_submissions", [], |row| {
            row.get(0)
        })
        .unwrap();

    migrate(&mut conn).expect("a drifted sidecar must repair");

    let after: i64 = conn
        .query_row("SELECT COUNT(*) FROM chain_submissions", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(before, after, "every submission must survive the repair");
    migrate(&mut conn).expect("a repaired sidecar stays repaired");
}

#[test]
fn v22_upgrades_combined_storage_to_the_fresh_schema() {
    let mut upgraded = Connection::open_in_memory().unwrap();
    upgraded.execute_batch(&v22_schema()).unwrap();
    upgraded.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
    queries::insert_round(
        &upgraded,
        "wallet",
        crate::Network::Testnet,
        &test_params(),
        None,
    )
    .unwrap();
    insert_v17_delegation_with_complete_setup(&upgraded, ROUND, "wallet", 0, None);
    upgraded
        .execute("UPDATE bundles SET delegation_pczt = X'010203'", [])
        .unwrap();
    upgraded
        .execute(
            "INSERT INTO chain_submissions
         (identity_key, round_id, wallet_id, network, bundle_index, kind,
          proposal_id, generation_digest, state, committed_post_reservations,
          tracking_started_at, diagnostic_kind, diagnostic, created_at, updated_at)
         VALUES (?1, ?2, 'wallet', 'testnet', 0, 'vote', 50, ?3,
                 'recovering', 7, 8, 'ambiguous_dispatch', 'timeout', 9, 10)",
            rusqlite::params![vec![0x41_u8; 32], ROUND, vec![0x42_u8; 32]],
        )
        .unwrap();
    upgraded
        .execute(
            "INSERT INTO keystone_signatures
         (round_id, wallet_id, bundle_index, sig, sighash, rk, created_at)
         VALUES (?1, 'wallet', 0, ?2, ?3, ?4, 9)",
            rusqlite::params![
                ROUND,
                vec![0x51_u8; 64],
                vec![0x52_u8; 32],
                vec![0x53_u8; 32]
            ],
        )
        .unwrap();
    let preserved_tables = [
        "bundles",
        "proofs",
        "keystone_signatures",
        "chain_submissions",
    ];
    let before = preserved_tables.map(|table| dump_table(&upgraded, table));
    upgraded.pragma_update(None, "user_version", 22).unwrap();
    migrate(&mut upgraded).unwrap();
    migrate(&mut upgraded).unwrap();
    for (table, rows) in preserved_tables.into_iter().zip(before) {
        assert_eq!(
            dump_table(&upgraded, table),
            rows,
            "{table} must survive migration"
        );
    }
    let mut fresh = Connection::open_in_memory().unwrap();
    migrate(&mut fresh).unwrap();
    let columns = |connection: &Connection, table: &str| {
        let mut statement = connection
            .prepare(&format!("PRAGMA table_info({table})"))
            .unwrap();
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(5)?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    for table in ["chain_submissions", "delegate_cast_recovery"] {
        assert_eq!(columns(&upgraded, table), columns(&fresh, table));
    }
    assert_eq!(
        upgraded
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        CURRENT_VERSION
    );
}

#[test]
fn v23_upgrades_to_a_schema_identical_to_a_fresh_one() {
    // The rejection ledger's SQL is written twice: once in `001_init.sql` for
    // a fresh database and once in `007_combined_cast_rejections.sql` for the
    // ladder. Column shape alone would not catch a drifted CHECK constraint or
    // a missing trigger, so this compares the full normalized schema of every
    // object, which is what `preview_schema_fingerprint` already reports.
    let mut upgraded = Connection::open_in_memory().unwrap();
    upgraded.execute_batch(&v23_schema()).unwrap();
    upgraded.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
    queries::insert_round(
        &upgraded,
        "wallet",
        crate::Network::Testnet,
        &test_params(),
        None,
    )
    .unwrap();
    insert_v17_delegation_with_complete_setup(&upgraded, ROUND, "wallet", 0, None);
    let preserved = dump_table(&upgraded, "bundles");
    upgraded.pragma_update(None, "user_version", 23).unwrap();

    migrate(&mut upgraded).unwrap();

    let mut fresh = Connection::open_in_memory().unwrap();
    migrate(&mut fresh).unwrap();
    assert_eq!(
        preview_schema_fingerprint(&upgraded).unwrap(),
        preview_schema_fingerprint(&fresh).unwrap(),
        "the ladder and `001_init.sql` must build the same schema"
    );
    assert_eq!(
        dump_table(&upgraded, "bundles"),
        preserved,
        "an in-place upgrade preserves delegation state"
    );
    assert_eq!(
        upgraded
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        CURRENT_VERSION
    );
}

#[test]
fn the_rejection_ledger_refuses_a_streak_that_does_not_match_its_generation() {
    // The upsert is the only writer today and always agrees with these
    // triggers, so nothing else would notice if they were dropped. They exist
    // to stop a future writer from rewriting history: a streak may only grow
    // within one delegation generation, and a new generation restarts it at
    // one. Reads of the ledger treat the count as monotonic evidence.
    let mut conn = Connection::open_in_memory().unwrap();
    migrate(&mut conn).unwrap();
    conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
    queries::insert_round(
        &conn,
        "wallet",
        crate::Network::Testnet,
        &test_params(),
        None,
    )
    .unwrap();
    conn.execute(
        "INSERT INTO combined_cast_rejections (
             round_id, wallet_id, bundle_index, delegation_generation_digest,
             consecutive_rejections, last_batch_digest, last_diagnostic_kind,
             last_diagnostic, first_rejected_at, last_rejected_at)
         VALUES (?1, 'wallet', 0, ?2, 2, ?3, 'chain_rejected', 'refused', 5, 5)",
        rusqlite::params![ROUND, vec![0xA1_u8; 32], vec![0xA2_u8; 32]],
    )
    .unwrap();

    let rewind = conn.execute(
        "UPDATE combined_cast_rejections SET consecutive_rejections = 1",
        [],
    );
    assert!(
        rewind.is_err(),
        "a streak cannot decrease within a generation"
    );

    let carry_over = conn.execute(
        "UPDATE combined_cast_rejections
            SET delegation_generation_digest = ?1, consecutive_rejections = 3",
        rusqlite::params![vec![0xB1_u8; 32]],
    );
    assert!(
        carry_over.is_err(),
        "a new delegation generation restarts the streak at one"
    );

    conn.execute(
        "UPDATE combined_cast_rejections
            SET delegation_generation_digest = ?1, consecutive_rejections = 1",
        rusqlite::params![vec![0xB1_u8; 32]],
    )
    .expect("restarting at one is the allowed generation change");
}

fn combined_preview(has_pczt: bool) -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    // A real preview predates the version-24 rejection ledger.
    let schema = v23_schema();
    conn.execute_batch(&if has_pczt {
        schema.clone()
    } else {
        schema.replace("    delegation_pczt     BLOB,\n", "")
    })
    .unwrap();
    conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA user_version=22;")
        .unwrap();
    queries::insert_round(
        &conn,
        "wallet",
        crate::Network::Testnet,
        &test_params(),
        None,
    )
    .unwrap();
    insert_v17_delegation_with_complete_setup(&conn, ROUND, "wallet", 0, None);
    conn.execute(
        "INSERT INTO delegate_cast_recovery VALUES (?1, 'wallet', 0, ?2, ?3, ?4)",
        rusqlite::params![ROUND, vec![1_u8; 32], vec![2_u8; 32], vec![3_u8; 64]],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO chain_submissions
         (identity_key, round_id, wallet_id, network, bundle_index, kind,
          ordered_batch_digest, generation_digest, state, candidate_transaction_hash,
          tracking_started_at, created_at, updated_at)
         VALUES (?1, ?2, 'wallet', 'testnet', 0, 'delegate_and_cast_vote_batch',
                 ?3, ?4, 'tracking', ?5, 8, 9, 10)",
        rusqlite::params![
            vec![4_u8; 32],
            ROUND,
            vec![1_u8; 32],
            vec![2_u8; 32],
            vec![5_u8; 32]
        ],
    )
    .unwrap();
    conn
}

#[test]
fn v22_combined_preview_preserves_recovery_and_tracking_with_or_without_pczt() {
    for has_pczt in [false, true] {
        let mut conn = combined_preview(has_pczt);
        if has_pczt {
            conn.execute("UPDATE bundles SET delegation_pczt=X'010203'", [])
                .unwrap();
        }
        let recovery = dump_table(&conn, "delegate_cast_recovery");
        let submissions = dump_table(&conn, "chain_submissions");
        let bundles = dump_table(&conn, "bundles");
        migrate(&mut conn).unwrap();
        migrate(&mut conn).unwrap();
        assert_eq!(recovery, dump_table(&conn, "delegate_cast_recovery"));
        assert_eq!(submissions, dump_table(&conn, "chain_submissions"));
        let migrated_bundles = dump_table(&conn, "bundles");
        if has_pczt {
            assert_eq!(bundles, migrated_bundles);
        } else {
            assert_eq!(bundles.len(), migrated_bundles.len());
            for (old, new) in bundles.iter().zip(&migrated_bundles) {
                assert_eq!(old, &new[..old.len()]);
            }
            let pczt: Option<Vec<u8>> = conn
                .query_row("SELECT delegation_pczt FROM bundles", [], |r| r.get(0))
                .unwrap();
            assert!(pczt.is_none());
        }
        assert_eq!(
            conn.pragma_query_value(None, "user_version", |r| r.get::<_, u32>(0))
                .unwrap(),
            CURRENT_VERSION
        );
        assert!(conn
            .execute(
                "UPDATE delegate_cast_recovery SET spend_auth_signature=zeroblob(64)",
                []
            )
            .is_err());
    }
}

#[test]
fn v22_unknown_combined_preview_rolls_back_without_losing_recovery() {
    for drift in [
        "DROP TRIGGER delegate_cast_recovery_immutable",
        "ALTER TABLE delegate_cast_recovery ADD COLUMN unknown BLOB",
        "CREATE TABLE unknown_preview_table (id INTEGER)",
    ] {
        let mut conn = combined_preview(false);
        conn.execute_batch(drift).unwrap();
        let schema = preview_schema_fingerprint(&conn).unwrap();
        let recovery = dump_table(&conn, "delegate_cast_recovery");
        let submissions = dump_table(&conn, "chain_submissions");
        assert!(migrate(&mut conn)
            .unwrap_err()
            .to_string()
            .contains("unsupported version-22 combined-vote preview schema"));
        assert_eq!(schema, preview_schema_fingerprint(&conn).unwrap());
        assert_eq!(recovery, dump_table(&conn, "delegate_cast_recovery"));
        assert_eq!(submissions, dump_table(&conn, "chain_submissions"));
        assert_eq!(
            conn.pragma_query_value(None, "user_version", |r| r.get::<_, u32>(0))
                .unwrap(),
            22
        );
    }
}
