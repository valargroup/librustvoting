use rusqlite::{Connection, TransactionBehavior};

use crate::VotingError;

const CURRENT_VERSION: u32 = 24;

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
    // v21 rebuilds `chain_submissions` onto the 50-proposal bound. Sidecars
    // migrated by a build that carried the 15-proposal bound keep it at
    // version 20, and the version-20 fingerprint check then refuses to open
    // them at all, so the widened bound needs a version of its own.
    (
        20,
        include_str!("migrations/005_chain_submissions_proposal_range.sql"),
    ),
    (21, "ALTER TABLE bundles ADD COLUMN delegation_pczt BLOB;"),
    (22, include_str!("migrations/006_delegate_cast.sql")),
    // The rejection ledger cannot be a `chain_submissions` column: that table
    // is fingerprinted on every open and rebuilt on drift, and the combined
    // freshness gates key off the absence of a row in it.
    (
        23,
        include_str!("migrations/007_combined_cast_rejections.sql"),
    ),
];

const RESET_SQL: &str = "DROP TABLE IF EXISTS combined_cast_rejections;
DROP TABLE IF EXISTS pir_proof_cache;
DROP TABLE IF EXISTS ballot_intent;
DROP TABLE IF EXISTS imt_proofs;
DROP TABLE IF EXISTS round_immediate_share;
DROP TABLE IF EXISTS helper_share_plans;
DROP TABLE IF EXISTS delegate_cast_recovery;
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
        return ensure_current_chain_submission_schema(conn);
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
            // Preview builds already stored PCZTs at version 21. Preserve them.
            if *from == 21 && tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('bundles') WHERE name = 'delegation_pczt')",
                [], |row| row.get::<_, bool>(0),
            ).map_err(|e| VotingError::from_sqlite("failed to inspect delegation PCZT storage", &e))? {
                upgraded = from + 1;
                continue;
            }
            if *from == 22 && reconcile_combined_preview(&tx)? {
                upgraded = from + 1;
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

    // A ladder that ends on a different shape than this build describes is the
    // same drift, reached the long way round.
    ensure_current_chain_submission_schema(conn)
}

/// Recognizes the version-22 combined-vote preview without discarding its
/// immutable recovery records. Only the exact current schema, optionally
/// missing the nullable PCZT column, is accepted. The caller owns the migration
/// transaction, so both the column repair and version advance commit together.
fn reconcile_combined_preview(conn: &Connection) -> Result<bool, VotingError> {
    let has_recovery: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name = 'delegate_cast_recovery')",
            [],
            |row| row.get(0),
        )
        .map_err(migration_error)?;
    if !has_recovery {
        return Ok(false);
    }
    let expected = Connection::open_in_memory().map_err(migration_error)?;
    expected
        .execute_batch(include_str!("migrations/001_init.sql"))
        .map_err(migration_error)?;
    // The preview predates the combined-cast rejection ledger, which the
    // version-23 ladder entry below creates. It is therefore not part of what
    // identifies a preview schema, and leaving it in the expected fingerprint
    // would refuse every preview database instead of upgrading it.
    expected
        .execute_batch(
            "DROP TRIGGER IF EXISTS combined_cast_rejections_monotonic_streak;
             DROP TRIGGER IF EXISTS combined_cast_rejections_generation_restart;
             DROP TABLE IF EXISTS combined_cast_rejections;",
        )
        .map_err(migration_error)?;
    let actual = preview_schema_fingerprint(conn)?;
    if actual == preview_schema_fingerprint(&expected)? {
        return Ok(true);
    }
    expected
        .execute_batch("ALTER TABLE bundles DROP COLUMN delegation_pczt;")
        .map_err(migration_error)?;
    if actual != preview_schema_fingerprint(&expected)? {
        return Err(VotingError::Internal {
            message: "unsupported version-22 combined-vote preview schema; existing recovery records were preserved".into(),
        });
    }
    conn.execute_batch("ALTER TABLE bundles ADD COLUMN delegation_pczt BLOB;")
        .map_err(migration_error)?;
    Ok(true)
}

/// Compares the explicit schema objects that identify a preview database,
/// including recovery constraints and triggers; table existence alone cannot
/// establish migration compatibility.
///
/// Everything `chain_submissions` owns is excluded. That table's DDL has been
/// edited without a version bump more than once, so real sidecars are in the
/// field missing one of its indexes or triggers;
/// [`ensure_current_chain_submission_schema`] exists to repair exactly that and
/// runs after this ladder. Fingerprinting those objects here would refuse such
/// a database with a hard error before the repair could run, and since nothing
/// would advance `user_version`, the sidecar this reconciliation exists to
/// rescue would become permanently unopenable. `005_chain_submissions_proposal_range.sql`
/// makes the same argument for the version-20 step.
fn preview_schema_fingerprint(
    conn: &Connection,
) -> Result<Vec<(String, String, String)>, VotingError> {
    let mut statement = conn
        .prepare(
            "SELECT type, name, sql FROM sqlite_schema \
         WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' \
           AND name NOT LIKE 'chain_submissions%' \
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

/// Brings `chain_submissions` to the shape `002_chain_submissions.sql`
/// describes, rebuilding it in place if it has drifted, and keeping every row.
///
/// The table's DDL has been edited without a version bump more than once — the
/// proposal bound has moved in both directions — and each time every database
/// already at the current version became unopenable, because the fingerprint
/// check below is exact and the migration ladder has nothing left to run. A
/// rebuild is the honest answer: rows are copied verbatim, so nothing durable
/// is at stake, and the schema converges on what this build expects instead of
/// stranding the wallet. A row the new shape genuinely rejects still fails the
/// copy, which is the one case worth stopping for.
fn ensure_current_chain_submission_schema(conn: &mut Connection) -> Result<(), VotingError> {
    if chain_submission_schema_matches_current(conn)? {
        return Ok(());
    }
    // Only constraint, index and trigger drift is repairable. The rebuild
    // carries rows across by name, so a table whose columns differ is drift
    // this cannot honestly resolve — there is no answer for a column that is
    // missing or unknown — and it stays the hard failure it has always been.
    let columns = chain_submission_columns(conn)?;
    if columns != expected_chain_submission_columns()? {
        return verify_current_chain_submission_schema(conn);
    }
    let column_list = columns.join(", ");

    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| VotingError::from_sqlite("failed to start schema repair", &e))?;
    tx.execute_batch(
        "DROP TRIGGER IF EXISTS chain_submissions_immutable_identity;
         DROP TRIGGER IF EXISTS chain_submissions_monotonic_reservations;
         DROP TRIGGER IF EXISTS chain_submissions_immutable_tracking_start;
         DROP INDEX IF EXISTS chain_submissions_identity;
         DROP INDEX IF EXISTS chain_submissions_candidate_owner;
         DROP INDEX IF EXISTS chain_submissions_confirmation_hash_owner;
         ALTER TABLE chain_submissions RENAME TO chain_submissions_drifted;",
    )
    .map_err(|e| VotingError::from_sqlite("failed to set aside the drifted schema", &e))?;
    tx.execute_batch(include_str!("migrations/002_chain_submissions.sql"))
        .map_err(|e| VotingError::from_sqlite("failed to recreate chain submissions", &e))?;
    tx.execute_batch(&format!(
        "INSERT INTO chain_submissions ({column_list})
             SELECT {column_list} FROM chain_submissions_drifted;
         DROP TABLE chain_submissions_drifted;"
    ))
    .map_err(|e| VotingError::from_sqlite("failed to carry chain submissions across", &e))?;
    tx.commit()
        .map_err(|e| VotingError::from_sqlite("failed to commit schema repair", &e))?;

    verify_current_chain_submission_schema(conn)
}

fn chain_submission_columns(conn: &Connection) -> Result<Vec<String>, VotingError> {
    let mut statement = conn
        .prepare("SELECT name FROM pragma_table_info('chain_submissions') ORDER BY cid")
        .map_err(migration_error)?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(migration_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(migration_error)?;
    Ok(columns)
}

fn expected_chain_submission_columns() -> Result<Vec<String>, VotingError> {
    let expected = Connection::open_in_memory().map_err(migration_error)?;
    expected
        .execute_batch(include_str!("migrations/002_chain_submissions.sql"))
        .map_err(migration_error)?;
    chain_submission_columns(&expected)
}

fn chain_submission_schema_matches_current(conn: &Connection) -> Result<bool, VotingError> {
    let expected = Connection::open_in_memory().map_err(migration_error)?;
    expected
        .execute_batch(include_str!("migrations/002_chain_submissions.sql"))
        .map_err(migration_error)?;
    Ok(chain_submission_schema_fingerprint(conn)?
        == chain_submission_schema_fingerprint(&expected)?)
}

fn verify_current_chain_submission_schema(conn: &Connection) -> Result<(), VotingError> {
    if !chain_submission_schema_matches_current(conn)? {
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
