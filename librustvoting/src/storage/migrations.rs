use rusqlite::Connection;

use crate::VotingError;

const CURRENT_VERSION: u32 = 6;

pub fn migrate(conn: &Connection) -> Result<(), VotingError> {
    let version: u32 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .map_err(|e| VotingError::Internal {
            message: format!("failed to read database version: {}", e),
        })?;

    if version < 1 {
        conn.execute_batch(include_str!("migrations/001_init.sql"))
            .map_err(|e| VotingError::Internal {
                message: format!("migration 001_init failed: {}", e),
            })?;
        conn.pragma_update(None, "user_version", 1)
            .map_err(|e| VotingError::Internal {
                message: format!("failed to update database version: {}", e),
            })?;
    }

    if version < 2 {
        // Add tables for witness caching that were added to 001_init.sql
        // after some DBs had already been created at version 1.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS cached_tree_state (
                round_id        TEXT PRIMARY KEY REFERENCES rounds(round_id),
                snapshot_height INTEGER NOT NULL,
                tree_state      BLOB NOT NULL
            );
            CREATE TABLE IF NOT EXISTS witnesses (
                round_id        TEXT NOT NULL,
                note_position   INTEGER NOT NULL,
                note_commitment BLOB NOT NULL,
                root            BLOB NOT NULL,
                auth_path       BLOB NOT NULL,
                created_at      INTEGER NOT NULL,
                PRIMARY KEY (round_id, note_position),
                FOREIGN KEY (round_id) REFERENCES rounds(round_id)
            );",
        )
        .map_err(|e| VotingError::Internal {
            message: format!("migration to version 2 failed: {}", e),
        })?;
        conn.pragma_update(None, "user_version", 2)
            .map_err(|e| VotingError::Internal {
                message: format!("failed to update database version: {}", e),
            })?;
    }

    if version < 3 {
        // v3: delegation data moved from rounds to bundles table, witnesses
        // gained bundle_index. Drop everything and recreate from 001_init.sql.
        conn.execute_batch(
            "DROP TABLE IF EXISTS share_delegations;
             DROP TABLE IF EXISTS keystone_signatures;
             DROP TABLE IF EXISTS votes;
             DROP TABLE IF EXISTS witnesses;
             DROP TABLE IF EXISTS proofs;
             DROP TABLE IF EXISTS bundles;
             DROP TABLE IF EXISTS cached_tree_state;
             DROP TABLE IF EXISTS rounds;"
        )
        .map_err(|e| VotingError::Internal {
            message: format!("migration to version 3 failed (drop): {}", e),
        })?;
        conn.execute_batch(include_str!("migrations/001_init.sql"))
            .map_err(|e| VotingError::Internal {
                message: format!("migration to version 3 failed (create): {}", e),
            })?;
        conn.pragma_update(None, "user_version", 3)
            .map_err(|e| VotingError::Internal {
                message: format!("failed to update database version: {}", e),
            })?;
    }

    if version < 4 {
        // v4: add wallet_id column for per-wallet state isolation.
        // Drop everything and recreate from 001_init.sql.
        conn.execute_batch(
            "DROP TABLE IF EXISTS share_delegations;
             DROP TABLE IF EXISTS keystone_signatures;
             DROP TABLE IF EXISTS votes;
             DROP TABLE IF EXISTS witnesses;
             DROP TABLE IF EXISTS proofs;
             DROP TABLE IF EXISTS bundles;
             DROP TABLE IF EXISTS cached_tree_state;
             DROP TABLE IF EXISTS rounds;"
        )
        .map_err(|e| VotingError::Internal {
            message: format!("migration to version 4 failed (drop): {}", e),
        })?;
        conn.execute_batch(include_str!("migrations/001_init.sql"))
            .map_err(|e| VotingError::Internal {
                message: format!("migration to version 4 failed (create): {}", e),
            })?;
        conn.pragma_update(None, "user_version", 4)
            .map_err(|e| VotingError::Internal {
                message: format!("failed to update database version: {}", e),
            })?;
    }

    if version < 5 {
        // v5: add share_delegations, keystone_signatures tables; add columns to
        // bundles (delegation_tx_hash) and votes (tx_hash, vc_tree_position,
        // commitment_bundle_json). Drop-all-recreate for pre-production.
        conn.execute_batch(
            "DROP TABLE IF EXISTS share_delegations;
             DROP TABLE IF EXISTS keystone_signatures;
             DROP TABLE IF EXISTS votes;
             DROP TABLE IF EXISTS witnesses;
             DROP TABLE IF EXISTS proofs;
             DROP TABLE IF EXISTS bundles;
             DROP TABLE IF EXISTS cached_tree_state;
             DROP TABLE IF EXISTS rounds;"
        )
        .map_err(|e| VotingError::Internal {
            message: format!("migration to version 5 failed (drop): {}", e),
        })?;
        conn.execute_batch(include_str!("migrations/001_init.sql"))
            .map_err(|e| VotingError::Internal {
                message: format!("migration to version 5 failed (create): {}", e),
            })?;
        conn.pragma_update(None, "user_version", 5)
            .map_err(|e| VotingError::Internal {
                message: format!("failed to update database version: {}", e),
            })?;
    }

    if version < 6 {
        // v6: share-status polling support.
        //
        // votes: add van_authority_spent (separate from submitted — tracks
        // CastVote TX confirmation for proposal_authority bitmask).
        // The column may already exist if 001_init.sql was used by a
        // drop+recreate step (v3/v4/v5) — check before altering.
        //
        // share_delegations: PK now includes helper_url (one receipt per
        // helper per share); renamed nullifier→share_nullifier,
        // confirmed→reveal_confirmed; added seq, submit_at columns.
        // SQLite cannot alter PKs so drop+recreate is required. Any
        // in-flight v5 share delegations are lost; the polling scanner
        // will resubmit them on next app launch.
        let has_van_column = conn
            .prepare("SELECT van_authority_spent FROM votes LIMIT 0")
            .is_ok();
        if !has_van_column {
            conn.execute_batch(
                "ALTER TABLE votes ADD COLUMN van_authority_spent INTEGER NOT NULL DEFAULT 0;",
            )
            .map_err(|e| VotingError::Internal {
                message: format!("migration to version 6 failed (alter votes): {}", e),
            })?;
        }
        conn.execute_batch(
            "DROP TABLE IF EXISTS share_delegations;
             CREATE TABLE share_delegations (
                 round_id          TEXT NOT NULL,
                 wallet_id         TEXT NOT NULL DEFAULT '',
                 bundle_index      INTEGER NOT NULL,
                 proposal_id       INTEGER NOT NULL,
                 share_index       INTEGER NOT NULL,
                 helper_url        TEXT NOT NULL,
                 share_nullifier   BLOB NOT NULL,
                 seq               INTEGER NOT NULL DEFAULT 0,
                 created_at        INTEGER NOT NULL,
                 submit_at         INTEGER NOT NULL DEFAULT 0,
                 reveal_confirmed  INTEGER NOT NULL DEFAULT 0,
                 PRIMARY KEY (round_id, wallet_id, bundle_index, proposal_id, share_index, helper_url),
                 FOREIGN KEY (round_id, wallet_id, bundle_index)
                     REFERENCES bundles(round_id, wallet_id, bundle_index) ON DELETE CASCADE
             );",
        )
        .map_err(|e| VotingError::Internal {
            message: format!("migration to version 6 failed (share_delegations): {}", e),
        })?;
        conn.pragma_update(None, "user_version", 6)
            .map_err(|e| VotingError::Internal {
                message: format!("failed to update database version: {}", e),
            })?;
    }

    let final_version: u32 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .map_err(|e| VotingError::Internal {
            message: format!("failed to verify database version: {}", e),
        })?;

    if final_version != CURRENT_VERSION {
        return Err(VotingError::Internal {
            message: format!(
                "unexpected database version after migration: expected {}, got {}",
                CURRENT_VERSION, final_version
            ),
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_migrate_fresh_database() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();

        let version: u32 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, CURRENT_VERSION);
    }

    #[test]
    fn test_migrate_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap();

        let version: u32 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, CURRENT_VERSION);
    }

    #[test]
    fn test_tables_created() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();

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
        assert!(tables.contains(&"share_delegations".to_string()));
        assert!(tables.contains(&"keystone_signatures".to_string()));
    }

    #[test]
    fn test_migrate_v5_to_v6() {
        let conn = Connection::open_in_memory().unwrap();

        // Simulate a v5 database by running init then setting version to 5
        // with the old share_delegations schema.
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        conn.execute_batch(
            "CREATE TABLE rounds (
                round_id TEXT NOT NULL, wallet_id TEXT NOT NULL DEFAULT '',
                snapshot_height INTEGER NOT NULL, ea_pk BLOB NOT NULL,
                nc_root BLOB NOT NULL, nullifier_imt_root BLOB NOT NULL,
                session_json TEXT, phase INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL, PRIMARY KEY (round_id, wallet_id)
            );
            CREATE TABLE bundles (
                round_id TEXT NOT NULL, wallet_id TEXT NOT NULL DEFAULT '',
                bundle_index INTEGER NOT NULL, note_positions_blob BLOB,
                van_comm_rand BLOB, dummy_nullifiers BLOB, rho_signed BLOB,
                padded_note_data BLOB, nf_signed BLOB, cmx_new BLOB,
                alpha BLOB, rseed_signed BLOB, rseed_output BLOB,
                gov_comm BLOB, total_note_value INTEGER, address_index INTEGER,
                van_leaf_position INTEGER, rk BLOB, gov_nullifiers_blob BLOB,
                padded_note_secrets BLOB, pczt_sighash BLOB, delegation_tx_hash TEXT,
                PRIMARY KEY (round_id, wallet_id, bundle_index),
                FOREIGN KEY (round_id, wallet_id) REFERENCES rounds(round_id, wallet_id) ON DELETE CASCADE
            );
            CREATE TABLE votes (
                id INTEGER PRIMARY KEY, round_id TEXT NOT NULL,
                wallet_id TEXT NOT NULL DEFAULT '', bundle_index INTEGER NOT NULL,
                proposal_id INTEGER NOT NULL, choice INTEGER NOT NULL,
                commitment BLOB, submitted INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL, tx_hash TEXT,
                vc_tree_position INTEGER, commitment_bundle_json TEXT,
                UNIQUE(round_id, wallet_id, bundle_index, proposal_id),
                FOREIGN KEY (round_id, wallet_id, bundle_index)
                    REFERENCES bundles(round_id, wallet_id, bundle_index) ON DELETE CASCADE
            );
            CREATE TABLE share_delegations (
                round_id TEXT NOT NULL, wallet_id TEXT NOT NULL DEFAULT '',
                bundle_index INTEGER NOT NULL, proposal_id INTEGER NOT NULL,
                share_index INTEGER NOT NULL, helper_url TEXT NOT NULL,
                nullifier BLOB NOT NULL, confirmed INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL,
                PRIMARY KEY (round_id, wallet_id, bundle_index, proposal_id, share_index),
                FOREIGN KEY (round_id, wallet_id, bundle_index)
                    REFERENCES bundles(round_id, wallet_id, bundle_index) ON DELETE CASCADE
            );",
        ).unwrap();
        conn.pragma_update(None, "user_version", 5).unwrap();

        // Insert test data so we can verify votes survive the migration.
        conn.execute_batch(
            "INSERT INTO rounds VALUES ('r1','w1',100,X'00',X'00',X'00',NULL,0,0);
             INSERT INTO bundles (round_id,wallet_id,bundle_index) VALUES ('r1','w1',0);
             INSERT INTO votes (round_id,wallet_id,bundle_index,proposal_id,choice,submitted,created_at)
                 VALUES ('r1','w1',0,3,1,1,0);
             INSERT INTO share_delegations (round_id,wallet_id,bundle_index,proposal_id,share_index,helper_url,nullifier,confirmed,created_at)
                 VALUES ('r1','w1',0,3,0,'https://h1',X'AA',1,0);",
        ).unwrap();

        migrate(&conn).unwrap();

        let version: u32 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, 6);

        // votes table gained van_authority_spent, existing rows default to 0.
        let (submitted, van_spent): (i64, i64) = conn
            .query_row(
                "SELECT submitted, van_authority_spent FROM votes WHERE proposal_id = 3",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(submitted, 1);
        assert_eq!(van_spent, 0);

        // share_delegations was recreated — old rows are gone, new schema present.
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM share_delegations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);

        // Verify new columns exist by inserting a v6 receipt.
        conn.execute(
            "INSERT INTO share_delegations (round_id,wallet_id,bundle_index,proposal_id,share_index,helper_url,share_nullifier,seq,created_at,submit_at,reveal_confirmed)
             VALUES ('r1','w1',0,3,0,'https://h1',X'BB',1,0,1700000000,0)",
            [],
        ).unwrap();
    }

    /// Verify that the bundles table columns exist after migration and can round-trip BLOB data.
    #[test]
    fn test_bundle_data_columns_exist() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();

        // Insert a round first
        conn.execute(
            "INSERT INTO rounds (round_id, wallet_id, snapshot_height, ea_pk, nc_root, nullifier_imt_root, phase, created_at) VALUES ('test', 'w1', 1, X'00', X'00', X'00', 0, 0)",
            [],
        ).unwrap();

        // Insert a bundle row using all nullable BLOB columns.
        conn.execute(
            "INSERT INTO bundles (round_id, wallet_id, bundle_index, van_comm_rand, dummy_nullifiers, rho_signed, padded_note_data, nf_signed, cmx_new, alpha, rseed_signed, rseed_output) VALUES ('test', 'w1', 0, X'AA', X'BB', X'CC', X'DD', X'EE', X'FF', X'11', X'22', X'33')",
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
    }
}
