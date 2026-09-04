use rusqlite::{Connection, TransactionBehavior};

use crate::VotingError;

const CURRENT_VERSION: u32 = 21;

/// Schema version that `001_init.sql` produces, and the oldest version that can
/// be upgraded in place.
///
/// Databases below this predate launch, so no user state is worth preserving
/// and they are reset. At or above it, round state must survive: a wallet that
/// upgrades between submitting a delegation and casting its vote cannot
/// re-create the randomly sampled `van_comm_rand` that ZKP #2 needs, and the
/// deterministic governance nullifiers are already spent on chain, so its
/// voting weight for that round would be lost with no way to recover it.
const LAUNCH_VERSION: u32 = 13;

/// In-place upgrades applied in order, oldest first.
///
/// Entry `(from, sql)` moves a database from `user_version == from` to
/// `from + 1`, and the entries must form an unbroken chain from
/// [`LAUNCH_VERSION`] to [`CURRENT_VERSION`]. Every statement here MUST preserve
/// existing rows; `001_init.sql` is updated alongside so a fresh database and a
/// migrated one end up with the same schema.
const INCREMENTAL_MIGRATIONS: &[(u32, &str)] = &[
    (13, "ALTER TABLE rounds ADD COLUMN bundle_policy_json TEXT;"),
    // v15 replaces the bundle-scoped `imt_proofs` table with the bundle- and
    // round-independent `pir_proof_cache`. Existing proofs are carried over
    // (their network comes from the owning round) so an upgrade mid-round does
    // not refetch anything, then the old table is dropped.
    (
        14,
        "CREATE TABLE pir_proof_cache (
    wallet_id   TEXT NOT NULL DEFAULT '',
    network     TEXT NOT NULL CHECK (network IN ('mainnet','testnet','regtest')),
    nullifier   BLOB NOT NULL,
    root        BLOB NOT NULL,
    nf_bounds   BLOB NOT NULL,
    leaf_pos    INTEGER NOT NULL,
    path        BLOB NOT NULL,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL,
    PRIMARY KEY (wallet_id, network, root, nullifier)
);
INSERT OR IGNORE INTO pir_proof_cache
    (wallet_id, network, nullifier, root, nf_bounds, leaf_pos, path, created_at, updated_at)
SELECT i.wallet_id, r.network, i.nullifier, i.root, i.nf_bounds, i.leaf_pos, i.path, i.created_at, i.created_at
FROM imt_proofs i
JOIN rounds r ON r.round_id = i.round_id AND r.wallet_id = i.wallet_id;
DROP TABLE imt_proofs;",
    ),
    (
        15,
        "ALTER TABLE share_delegations ADD COLUMN ambiguous_urls TEXT NOT NULL DEFAULT '[]';
ALTER TABLE share_delegations ADD COLUMN attempting_urls TEXT NOT NULL DEFAULT '[]';
ALTER TABLE share_delegations ADD COLUMN target_count INTEGER NOT NULL DEFAULT 0;",
    ),
    (
        16,
        "-- v3.1.0-rc.13 singleton recovery JSON predates atomic-batch metadata.
-- Normalize those released rows before plans can bind to their exact bytes so
-- confirmation reserialization changes only the VC tree position.
UPDATE votes
   SET commitment_bundle_json = json_set(
           json_set(
               json_set(commitment_bundle_json, '$.batch_digest', NULL),
               '$.batch_index', NULL
           ),
           '$.batch_size', NULL
       )
 WHERE CASE
           WHEN json_valid(commitment_bundle_json) THEN
               json_extract(commitment_bundle_json, '$.format') = 'zcash_voting_vote_recovery_v1'
               AND json_type(commitment_bundle_json, '$.batch_digest') IS NULL
               AND json_type(commitment_bundle_json, '$.batch_index') IS NULL
               AND json_type(commitment_bundle_json, '$.batch_size') IS NULL
           ELSE 0
       END;
CREATE TABLE helper_share_plans (
    round_id                    TEXT NOT NULL,
    wallet_id                   TEXT NOT NULL DEFAULT '',
    bundle_index                INTEGER NOT NULL,
    proposal_id                 INTEGER NOT NULL,
    commitment_bundle_json      TEXT NOT NULL,
    configured_server_urls_json TEXT NOT NULL,
    share_plans_json            TEXT NOT NULL,
    format_version              INTEGER NOT NULL CHECK (format_version = 1),
    placement_guarantee         TEXT NOT NULL CHECK (placement_guarantee IN ('strict','legacy_best_effort')),
    created_at                  INTEGER NOT NULL,
    PRIMARY KEY (round_id, wallet_id, bundle_index, proposal_id),
    FOREIGN KEY (round_id, wallet_id, bundle_index, proposal_id)
        REFERENCES votes(round_id, wallet_id, bundle_index, proposal_id) ON DELETE CASCADE
);
CREATE TRIGGER clear_helper_share_plan_on_vote_generation_change
AFTER UPDATE OF commitment_bundle_json ON votes
WHEN OLD.commitment_bundle_json IS NOT NEW.commitment_bundle_json
BEGIN
    -- Confirmation is the one non-generational recovery update: it fills the
    -- VC tree position in both the vote column and the otherwise-identical
    -- recovery JSON. Advance only a plan bound to the exact OLD snapshot and
    -- only when replacing that one JSON field produces the exact NEW bytes.
    UPDATE helper_share_plans
       SET commitment_bundle_json = NEW.commitment_bundle_json
     WHERE round_id = NEW.round_id AND wallet_id = NEW.wallet_id
       AND bundle_index = NEW.bundle_index AND proposal_id = NEW.proposal_id
       AND commitment_bundle_json = OLD.commitment_bundle_json
       AND OLD.vc_tree_position IS NULL
       AND NEW.vc_tree_position IS NOT NULL
       AND json_set(
               OLD.commitment_bundle_json,
               '$.vc_tree_position',
               NEW.vc_tree_position
           ) = NEW.commitment_bundle_json;
    DELETE FROM helper_share_plans
     WHERE round_id = NEW.round_id AND wallet_id = NEW.wallet_id
       AND bundle_index = NEW.bundle_index AND proposal_id = NEW.proposal_id
       AND commitment_bundle_json IS NOT NEW.commitment_bundle_json;
END;",
    ),
    // v18 adds the lifecycle-owned `chain_submissions` table. It is schema
    // only: version-17 domain columns on `votes` and `bundles` are preserved
    // untouched so completed rounds keep displaying, and no version-17
    // evidence is imported into the lifecycle.
    (
        17,
        include_str!("migrations/002_chain_submissions.sql"),
    ),
    (
        18,
        include_str!("migrations/003_hashless_submission.sql"),
    ),
    // v20 makes the round's immediate helper-share designation a row of its
    // own, adopted from the version-19 plan markers.
    (
        19,
        include_str!("migrations/004_round_immediate_share.sql"),
    ),
    (20, "ALTER TABLE bundles ADD COLUMN delegation_pczt BLOB;"),
];

const RESET_SQL: &str = "DROP TABLE IF EXISTS pir_proof_cache;
DROP TABLE IF EXISTS ballot_intent;
DROP TABLE IF EXISTS imt_proofs;
DROP TABLE IF EXISTS round_immediate_share;
DROP TABLE IF EXISTS helper_share_plans;
DROP TABLE IF EXISTS chain_submissions;
DROP TABLE IF EXISTS share_delegations;
DROP TABLE IF EXISTS keystone_signatures;
DROP TABLE IF EXISTS votes;
DROP TABLE IF EXISTS witnesses;
DROP TABLE IF EXISTS proofs;
DROP TABLE IF EXISTS bundles;
DROP TABLE IF EXISTS cached_tree_state;
DROP TABLE IF EXISTS rounds;";

pub fn migrate(conn: &mut Connection) -> Result<(), VotingError> {
    let version: u32 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .map_err(|e| VotingError::from_sqlite("failed to read database version", &e))?;

    if version > CURRENT_VERSION {
        return Err(VotingError::Internal {
            message: format!(
                "unsupported newer database version: expected at most {}, got {}",
                CURRENT_VERSION, version
            ),
        });
    }

    if version == CURRENT_VERSION {
        return verify_current_chain_submission_schema(conn);
    }

    // Immediate: a deferred transaction takes the write lock lazily on its
    // first statement, and a read-to-write upgrade that loses the race fails
    // without invoking the busy handler, so the connection busy timeout would
    // not cover the schema statements below.
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| {
            VotingError::from_sqlite("failed to start database migration transaction", &e)
        })?;

    if version < LAUNCH_VERSION {
        tx.execute_batch(RESET_SQL).map_err(|e| {
            VotingError::from_sqlite("failed to reset pre-launch database schema", &e)
        })?;
        tx.execute_batch(include_str!("migrations/001_init.sql"))
            .map_err(|e| VotingError::from_sqlite("failed to create launch database schema", &e))?;
    } else {
        // Launched databases hold delegation state that cannot be rebuilt, so
        // they are upgraded in place rather than recreated.
        let mut upgraded = version;
        for (from, sql) in INCREMENTAL_MIGRATIONS {
            if *from < upgraded {
                continue;
            }
            tx.execute_batch(sql).map_err(|e| {
                VotingError::from_sqlite(
                    &format!(
                        "failed to upgrade database schema from version {} to {}",
                        from,
                        from + 1
                    ),
                    &e,
                )
            })?;
            upgraded = from + 1;
        }
        if upgraded != CURRENT_VERSION {
            return Err(VotingError::Internal {
                message: format!(
                    "no upgrade path from database version {} to {}: incremental migrations stop at {}",
                    version, CURRENT_VERSION, upgraded
                ),
            });
        }
    }

    tx.pragma_update(None, "user_version", CURRENT_VERSION)
        .map_err(|e| VotingError::from_sqlite("failed to update database version", &e))?;
    tx.commit()
        .map_err(|e| VotingError::from_sqlite("failed to commit database migration", &e))?;

    Ok(())
}

fn verify_current_chain_submission_schema(conn: &Connection) -> Result<(), VotingError> {
    let expected = Connection::open_in_memory().map_err(migration_error)?;
    expected
        .execute_batch(include_str!("migrations/002_chain_submissions.sql"))
        .map_err(migration_error)?;
    if chain_submission_schema_fingerprint(conn)? != chain_submission_schema_fingerprint(&expected)?
    {
        return Err(VotingError::Internal {
            message: format!(
                "database uses an unsupported chain-submission schema for version {CURRENT_VERSION}"
            ),
        });
    }
    Ok(())
}

fn chain_submission_schema_fingerprint(
    conn: &Connection,
) -> Result<Vec<(String, String, String)>, VotingError> {
    let mut statement = conn
        .prepare(
            "SELECT type, name, sql
               FROM sqlite_schema
              WHERE tbl_name = 'chain_submissions' AND sql IS NOT NULL
              ORDER BY type, name",
        )
        .map_err(migration_error)?;
    let fingerprint = statement
        .query_map([], |row| {
            let sql: String = row.get(2)?;
            Ok((row.get(0)?, row.get(1)?, normalize_schema_sql(&sql)))
        })
        .map_err(migration_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(migration_error)?;
    Ok(fingerprint)
}

fn normalize_schema_sql(sql: &str) -> String {
    let mut normalized = String::with_capacity(sql.len());
    let mut characters = sql.chars().peekable();
    let mut in_string = false;
    while let Some(character) = characters.next() {
        if character == '\'' {
            normalized.push(character);
            if in_string && characters.peek() == Some(&'\'') {
                if let Some(escaped_quote) = characters.next() {
                    normalized.push(escaped_quote);
                }
            } else {
                in_string = !in_string;
            }
        } else if in_string || !character.is_whitespace() {
            normalized.push(character);
        }
    }
    normalized
}

/// Classifies a SQLite failure raised while inspecting migration state.
///
/// Concurrent opens can see `SQLITE_BUSY` here too, so the error must stay
/// retryable rather than collapsing into `Internal`.
fn migration_error(error: rusqlite::Error) -> VotingError {
    VotingError::from_sqlite("failed to inspect chain-submission migration state", &error)
}

#[cfg(test)]
mod tests;
