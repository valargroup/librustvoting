use rusqlite::{named_params, Connection, Transaction};

use crate::chain_submission::{
    complete_generation_for_delegation, generation_for_vote, generation_for_vote_batch,
    network_name, submission_identity_key, ChainSubmissionIdentity, ChainSubmissionIdentityError,
    ChainSubmissionTarget, ExpectedTreeLayout,
};
use crate::VotingError;

const CURRENT_VERSION: u32 = 18;

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
    (
        17,
        include_str!("migrations/002_chain_submissions.sql"),
    ),
];

const RESET_SQL: &str = "DROP TABLE IF EXISTS pir_proof_cache;
DROP TABLE IF EXISTS ballot_intent;
DROP TABLE IF EXISTS imt_proofs;
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
        .map_err(|e| VotingError::Internal {
            message: format!("failed to read database version: {}", e),
        })?;

    if version > CURRENT_VERSION {
        return Err(VotingError::Internal {
            message: format!(
                "unsupported newer database version: expected at most {}, got {}",
                CURRENT_VERSION, version
            ),
        });
    }

    if version == CURRENT_VERSION {
        return verify_v18_schema(conn);
    }

    let tx = conn.transaction().map_err(|e| VotingError::Internal {
        message: format!("failed to start database migration transaction: {}", e),
    })?;

    if version < LAUNCH_VERSION {
        tx.execute_batch(RESET_SQL)
            .map_err(|e| VotingError::Internal {
                message: format!("failed to reset pre-launch database schema: {}", e),
            })?;
        tx.execute_batch(include_str!("migrations/001_init.sql"))
            .map_err(|e| VotingError::Internal {
                message: format!("failed to create launch database schema: {}", e),
            })?;
    } else {
        // Launched databases hold delegation state that cannot be rebuilt, so
        // they are upgraded in place rather than recreated.
        let mut upgraded = version;
        for (from, sql) in INCREMENTAL_MIGRATIONS {
            if *from < upgraded {
                continue;
            }
            tx.execute_batch(sql).map_err(|e| VotingError::Internal {
                message: format!(
                    "failed to upgrade database schema from version {} to {}: {}",
                    from,
                    from + 1,
                    e
                ),
            })?;
            if *from == 17 {
                backfill_v17_chain_evidence(&tx)?;
            }
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
        .map_err(|e| VotingError::Internal {
            message: format!("failed to update database version: {}", e),
        })?;
    tx.commit().map_err(|e| VotingError::Internal {
        message: format!("failed to commit database migration: {}", e),
    })?;

    Ok(())
}

fn verify_v18_schema(conn: &Connection) -> Result<(), VotingError> {
    let expected = Connection::open_in_memory().map_err(migration_error)?;
    expected
        .execute_batch(include_str!("migrations/002_chain_submissions.sql"))
        .map_err(migration_error)?;
    if chain_submission_schema_fingerprint(conn)? != chain_submission_schema_fingerprint(&expected)?
    {
        return Err(VotingError::Internal {
            message: "database uses an unsupported unreleased version-18 chain-submission schema; recreate it from version 17".to_string(),
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

fn migration_error(error: rusqlite::Error) -> VotingError {
    VotingError::Internal {
        message: format!("failed to inspect chain-submission migration state: {error}"),
    }
}

fn encode_positions(positions: &[u64]) -> Result<Vec<u8>, VotingError> {
    let count = u32::try_from(positions.len()).map_err(|_| VotingError::Internal {
        message: "too many chain-submission positions".to_string(),
    })?;
    let mut encoded = Vec::with_capacity(5 + positions.len() * 8);
    encoded.push(1);
    encoded.extend_from_slice(&count.to_be_bytes());
    for position in positions {
        if *position > i64::MAX as u64 {
            return Err(VotingError::Internal {
                message: "version-17 confirmation position exceeds SQLite's signed range"
                    .to_string(),
            });
        }
        encoded.extend_from_slice(&position.to_be_bytes());
    }
    Ok(encoded)
}

fn parse_network(network: &str) -> Result<crate::Network, VotingError> {
    match network {
        "mainnet" => Ok(crate::Network::Mainnet),
        "testnet" => Ok(crate::Network::Testnet),
        "regtest" => Ok(crate::Network::Regtest),
        other => Err(VotingError::Internal {
            message: format!("unsupported version-17 network {other}"),
        }),
    }
}

/// Rebuilds the runtime submission identity for one version-17 row.
///
/// Migration and runtime reservation derive the same identity key from this
/// type, so a migrated row and a natively reserved row for the same submission
/// collide on the primary key rather than occupying separate namespaces.
///
/// Returns `None` when the round id cannot form a canonical lifecycle identity.
/// Such a round can never be the subject of runtime submission work either, so
/// there is nothing to guard: the row is left as version-17 domain data, which
/// keeps the round prunable and explicitly deletable.
fn v17_identity(
    wallet_id: &str,
    network: &str,
    round_id: &str,
    bundle_index: i64,
    target: ChainSubmissionTarget,
) -> Result<Option<ChainSubmissionIdentity>, VotingError> {
    let Some(round_bytes) = hex::decode(round_id)
        .ok()
        .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
        // The stored column must equal what the runtime re-encodes from the
        // identity, or lookups and the rounds foreign key would disagree.
        .filter(|bytes| hex::encode(bytes) == round_id)
    else {
        return Ok(None);
    };
    let bundle_index = u32::try_from(bundle_index).map_err(|_| VotingError::Internal {
        message: "invalid legacy bundle index".to_string(),
    })?;
    match ChainSubmissionIdentity::new(
        wallet_id,
        parse_network(network)?,
        round_bytes,
        bundle_index,
        target,
    ) {
        Ok(identity) => Ok(Some(identity)),
        Err(ChainSubmissionIdentityError::InvalidVoteRoundId) => Ok(None),
        Err(error) => Err(VotingError::Internal {
            message: format!(
                "invalid version-17 chain-submission identity for wallet={wallet_id}, \
                 network={network}, round={round_id}, bundle={bundle_index}: {error}"
            ),
        }),
    }
}

/// One classified version-18 row derived from version-17 evidence.
struct V17Import {
    generation_digest: Option<Vec<u8>>,
    state: &'static str,
    confirmation_source: Option<&'static str>,
    final_van_position: Option<u64>,
    vote_commitment_positions: Option<Vec<u8>>,
    diagnostic_kind: Option<&'static str>,
    diagnostic: Option<&'static str>,
}

const RECOVERY_UNAVAILABLE_DIAGNOSTIC: &str =
    "version-17 chain evidence lacks generation recovery material";
const GENERATION_DERIVATION_FAILED_DIAGNOSTIC: &str =
    "version-17 recovery material could not derive a generation";

impl V17Import {
    /// Permanently guards evidence that has no recovery inputs to bind.
    fn recovery_unavailable_guard() -> Self {
        Self {
            generation_digest: None,
            state: "recovering",
            confirmation_source: None,
            final_van_position: None,
            vote_commitment_positions: None,
            diagnostic_kind: Some("recovery_unavailable"),
            diagnostic: Some(RECOVERY_UNAVAILABLE_DIAGNOSTIC),
        }
    }

    /// Permanently guards evidence whose persisted recovery inputs do not bind.
    ///
    /// The source error is intentionally not persisted because it may expose
    /// implementation details. The distinct kind preserves the reason this
    /// identity cannot participate in runtime recovery.
    fn generation_derivation_failed_guard() -> Self {
        Self {
            generation_digest: None,
            state: "recovering",
            confirmation_source: None,
            final_van_position: None,
            vote_commitment_positions: None,
            diagnostic_kind: Some("generation_derivation_failed"),
            diagnostic: Some(GENERATION_DERIVATION_FAILED_DIAGNOSTIC),
        }
    }

    fn legacy_projection(
        final_van_position: u64,
        vote_commitment_position: u64,
    ) -> Result<Self, VotingError> {
        Ok(Self {
            generation_digest: None,
            state: "confirmed",
            confirmation_source: Some("legacy_projection"),
            final_van_position: Some(final_van_position),
            vote_commitment_positions: Some(encode_positions(&[vote_commitment_position])?),
            diagnostic_kind: None,
            diagnostic: None,
        })
    }
}

/// Confirms a bound version-17 generation whose recorded positions match its
/// derived layout, and otherwise leaves it bound and `Recovering`.
///
/// A bound `Recovering` row is not a dead end: it carries a real generation
/// digest and expected layout, so ordinary tree recovery can locate its exact
/// output layout and confirm it. Positions that are incomplete, non-contiguous,
/// reordered, or outside checked position arithmetic are never accepted as a
/// confirmation.
fn legacy_import_or_recovering(
    generation_digest: Vec<u8>,
    expected_layout: &ExpectedTreeLayout,
    final_van_position: Option<u64>,
    vote_commitment_positions: Vec<u64>,
) -> Result<V17Import, VotingError> {
    match final_van_position {
        Some(final_van_position)
            if positions_match_expected_layout(
                expected_layout,
                final_van_position,
                &vote_commitment_positions,
            ) =>
        {
            Ok(V17Import {
                generation_digest: Some(generation_digest),
                state: "confirmed",
                confirmation_source: Some("legacy_import"),
                final_van_position: Some(final_van_position),
                vote_commitment_positions: Some(encode_positions(&vote_commitment_positions)?),
                diagnostic_kind: None,
                diagnostic: None,
            })
        }
        _ => Ok(V17Import {
            generation_digest: Some(generation_digest),
            state: "recovering",
            confirmation_source: None,
            final_van_position: None,
            vote_commitment_positions: None,
            diagnostic_kind: Some("reconciliation_pending"),
            diagnostic: Some(
                "version-17 positions do not confirm the bound generation; recovery is pending",
            ),
        }),
    }
}

/// Checks the protocol layout `[VAN, VC 0, ..., VC N-1]` without wrapping.
fn positions_match_expected_layout(
    expected_layout: &ExpectedTreeLayout,
    final_van_position: u64,
    vote_commitment_positions: &[u64],
) -> bool {
    let expected_commitments = expected_layout.leaves().len() - 1;
    vote_commitment_positions.len() == expected_commitments
        && vote_commitment_positions
            .iter()
            .enumerate()
            .all(|(index, recorded_position)| {
                u64::try_from(index)
                    .ok()
                    .and_then(|offset| final_van_position.checked_add(1)?.checked_add(offset))
                    == Some(*recorded_position)
            })
}

/// Claims a validated `legacy_import` VAN in the network-wide output namespace.
///
/// Every observed VC has already claimed its position before classification.
/// A `legacy_projection` VAN is deliberately excluded because migration cannot
/// validate that it belongs to the projected generation.
fn register_v17_legacy_import_van(
    observed_output_positions: &mut std::collections::HashSet<(String, u64)>,
    network: &str,
    import: &V17Import,
) -> Result<(), VotingError> {
    if import.confirmation_source != Some("legacy_import") {
        return Ok(());
    }
    let position = import
        .final_van_position
        .ok_or_else(|| VotingError::Internal {
            message: "version-17 legacy import is missing its validated VAN position".to_string(),
        })?;
    if !observed_output_positions.insert((network.to_string(), position)) {
        return Err(VotingError::Internal {
            message: format!(
                "version-17 {network} validated output position {position} has multiple owners"
            ),
        });
    }
    Ok(())
}

/// Inserts one classified version-17 row under the shared identity key.
#[allow(clippy::too_many_arguments)]
fn insert_v17_submission(
    tx: &Transaction<'_>,
    identity: &ChainSubmissionIdentity,
    kind: &str,
    proposal_id: Option<u32>,
    ordered_batch_digest: Option<Vec<u8>>,
    created_at: i64,
    import: V17Import,
) -> Result<(), VotingError> {
    tx.execute(
        "INSERT INTO chain_submissions
            (identity_key, round_id, wallet_id, network, bundle_index, kind, proposal_id,
             ordered_batch_digest, generation_digest, state, committed_post_reservations,
             diagnostic_kind, diagnostic, confirmation_source, final_van_position,
             vote_commitment_positions, created_at, updated_at)
         VALUES (:key, :round, :wallet, :network, :bundle, :kind, :proposal,
                 :batch, :digest, :state, 0, :diagnostic_kind, :diagnostic,
                 :confirmation_source, :final_van, :positions, :created, :created)",
        named_params! {
            ":key": submission_identity_key(identity),
            ":round": hex::encode(identity.vote_round_id()),
            ":wallet": identity.wallet_id(),
            ":network": network_name(identity.network()),
            ":bundle": identity.bundle_index(),
            ":kind": kind,
            ":proposal": proposal_id,
            ":batch": ordered_batch_digest,
            ":digest": import.generation_digest,
            ":state": import.state,
            ":diagnostic_kind": import.diagnostic_kind,
            ":diagnostic": import.diagnostic,
            ":confirmation_source": import.confirmation_source,
            ":final_van": import.final_van_position.map(|value| value as i64),
            ":positions": import.vote_commitment_positions,
            ":created": created_at.max(0),
        },
    )
    .map_err(|error| VotingError::Internal {
        message: format!("failed to import version-17 chain evidence: {error}"),
    })?;
    Ok(())
}

/// Scopes one provable atomic batch: wallet, network, round, bundle, digest.
type V17BatchScope = (String, String, String, i64, [u8; 32]);

type V17VoteEvidenceRow = (
    String,
    String,
    String,
    i64,
    i64,
    Option<String>,
    Option<i64>,
    Option<i64>,
    Option<String>,
    i64,
    Option<Vec<u8>>,
    i64,
    bool,
);

fn validate_v17_recovery_batches(rows: &[V17VoteEvidenceRow]) -> Result<(), VotingError> {
    use std::collections::HashMap;

    type BatchScope = (String, String, String, i64, [u8; 32]);
    type SharedHashScope = (String, String, String, i64, String);

    let mut batches: HashMap<BatchScope, Vec<(crate::vote::VoteRecoveryBundle, Option<String>)>> =
        HashMap::new();
    let mut shared_hashes: HashMap<SharedHashScope, Vec<Option<[u8; 32]>>> = HashMap::new();

    for row in rows {
        let parsed = row
            .8
            .as_deref()
            .map(crate::vote::parse_recovery)
            .transpose()?;
        let batch_digest = parsed
            .as_ref()
            .and_then(|recovery| recovery.batch.as_ref().map(|batch| batch.digest));
        if let Some(recovery) = parsed {
            if let Some(batch) = recovery.batch.as_ref() {
                batches
                    .entry((
                        row.1.clone(),
                        row.2.clone(),
                        row.0.clone(),
                        row.3,
                        batch.digest,
                    ))
                    .or_default()
                    .push((recovery, row.5.clone()));
            }
        }
        if let Some(hash) = row.5.as_ref() {
            shared_hashes
                .entry((
                    row.1.clone(),
                    row.2.clone(),
                    row.0.clone(),
                    row.3,
                    hash.clone(),
                ))
                .or_default()
                .push(batch_digest);
        }
    }

    for ((wallet, network, round, bundle, digest), mut members) in batches {
        let expected_size = members[0].0.batch.as_ref().unwrap().size;
        if members.len() != expected_size as usize
            || members
                .iter()
                .any(|(recovery, _)| recovery.batch.as_ref().unwrap().size != expected_size)
        {
            return Err(VotingError::Internal {
                message: format!(
                    "incomplete version-17 vote batch for wallet={wallet}, network={network}, round={round}, bundle={bundle}"
                ),
            });
        }
        members.sort_by_key(|(recovery, _)| recovery.batch.as_ref().unwrap().index);
        if members
            .iter()
            .enumerate()
            .any(|(index, (recovery, _))| recovery.batch.as_ref().unwrap().index != index as u32)
        {
            return Err(VotingError::Internal {
                message: format!(
                    "inconsistent version-17 vote batch ordering for wallet={wallet}, network={network}, round={round}, bundle={bundle}"
                ),
            });
        }
        let anchor_height = members[0].0.anchor_height;
        if members
            .iter()
            .any(|(recovery, _)| recovery.anchor_height != anchor_height)
        {
            return Err(VotingError::Internal {
                message: format!(
                    "inconsistent version-17 vote batch anchor for wallet={wallet}, network={network}, round={round}, bundle={bundle}"
                ),
            });
        }
        let first_hash = members[0].1.as_deref();
        if members
            .iter()
            .any(|(_, hash)| hash.as_deref() != first_hash)
        {
            return Err(VotingError::Internal {
                message: format!(
                    "inconsistent version-17 vote batch hash for wallet={wallet}, network={network}, round={round}, bundle={bundle}"
                ),
            });
        }
        let actions = members
            .iter()
            .map(
                |(recovery, _)| crate::vote_commitment::CastVoteBatchSighashAction {
                    r_vpk: &recovery.r_vpk,
                    van_nullifier: &recovery.van_nullifier,
                    vote_authority_note_new: &recovery.vote_authority_note_new,
                    vote_commitment: &recovery.vote_commitment,
                    proposal_id: recovery.proposal_id,
                },
            )
            .collect::<Vec<_>>();
        let recomputed = crate::vote_commitment::cast_vote_batch_sighash(
            &round,
            u64::from(anchor_height),
            &actions,
        )?;
        if recomputed != digest {
            return Err(VotingError::Internal {
                message: format!(
                    "version-17 vote batch digest mismatch for wallet={wallet}, network={network}, round={round}, bundle={bundle}"
                ),
            });
        }
    }

    for ((wallet, network, round, bundle, _), digests) in shared_hashes {
        if digests.len() > 1
            && (digests[0].is_none() || digests.iter().any(|digest| *digest != digests[0]))
        {
            return Err(VotingError::Internal {
                message: format!(
                    "shared version-17 vote hash is not one reconstructable batch for wallet={wallet}, network={network}, round={round}, bundle={bundle}"
                ),
            });
        }
    }
    Ok(())
}

/// Imports every version-17 row that carries chain evidence.
///
/// Classification is by what can be *derived*, not by what version 17 happened
/// to store. Recovery material determines a complete semantic generation on its
/// own, so a recovery-backed row becomes an ordinary bound submission that can
/// poll, scan, retry, and confirm. A row with confirmation positions but no
/// recovery material becomes a terminal `legacy_projection` confirmation: its
/// observed successor VAN is exactly what the next proposal in the bundle
/// consumes, which is all advancement needs, and the source keeps the
/// provenance honest. Evidence without a derivable generation stays a
/// permanently unbound digestless guard, allowing the database upgrade to
/// complete without treating corrupt recovery as authoritative.
///
/// Any row that cannot be represented without guessing aborts the migration.
fn backfill_v17_chain_evidence(tx: &Transaction<'_>) -> Result<(), VotingError> {
    let mut observed_output_positions = std::collections::HashSet::new();
    let mut statement = tx
        .prepare(
            "SELECT v.round_id, v.wallet_id, r.network, v.bundle_index, v.proposal_id,
                    v.tx_hash, b.van_leaf_position, v.vc_tree_position,
                    v.commitment_bundle_json, v.choice, v.commitment, v.created_at,
                    CASE WHEN v.tx_hash IS NOT NULL AND
                         (SELECT count(*) FROM votes sibling
                           WHERE sibling.round_id = v.round_id
                             AND sibling.wallet_id = v.wallet_id
                             AND sibling.bundle_index = v.bundle_index
                             AND sibling.tx_hash = v.tx_hash) > 1
                         THEN 1 ELSE 0 END
               FROM votes v
               JOIN bundles b USING (round_id, wallet_id, bundle_index)
               JOIN rounds r USING (round_id, wallet_id)
              ORDER BY v.wallet_id, v.round_id, v.bundle_index, v.proposal_id",
        )
        .map_err(migration_error)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<i64>>(6)?,
                row.get::<_, Option<i64>>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, i64>(9)?,
                row.get::<_, Option<Vec<u8>>>(10)?,
                row.get::<_, i64>(11)?,
                row.get::<_, i64>(12)? != 0,
            ))
        })
        .map_err(migration_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(migration_error)?;
    drop(statement);
    validate_v17_recovery_batches(&rows)?;

    // Batch members are bound once, as a single vote_batch generation. A
    // provable batch has already been validated for completeness, ordering,
    // anchor, hash agreement, and digest recomputation.
    let mut provable_batches: std::collections::BTreeMap<V17BatchScope, i64> =
        std::collections::BTreeMap::new();

    for (
        round_id,
        wallet_id,
        network,
        bundle,
        proposal,
        hash,
        van,
        vc,
        recovery,
        stored_choice,
        stored_commitment,
        created,
        shared_hash,
    ) in rows
    {
        let bundle_index = u32::try_from(bundle).map_err(|_| VotingError::Internal {
            message: "invalid legacy bundle index".to_string(),
        })?;
        let proposal_id = u32::try_from(proposal).map_err(|_| VotingError::Internal {
            message: "invalid legacy proposal id".to_string(),
        })?;
        let mut recovery_batch_digest = None;
        if let Some(json) = recovery.as_deref() {
            if !tx
                .query_row("SELECT json_valid(?1)", [json], |row| row.get::<_, bool>(0))
                .map_err(migration_error)?
            {
                return Err(VotingError::Internal {
                    message: format!("malformed non-null vote recovery JSON for round={round_id}, bundle={bundle}, proposal={proposal}"),
                });
            }
            // The public recovery loader performs the complete closed-format
            // validation used by runtime generation derivation.
            let parsed = crate::vote::recovery_bundle_with_conn(
                tx,
                &wallet_id,
                &round_id,
                bundle_index,
                proposal_id,
            )?
            .ok_or_else(|| VotingError::Internal {
                message: "non-null legacy recovery JSON disappeared while migrating".to_string(),
            })?;
            crate::vote::validate_recovery_matches_stored_vote(
                &parsed,
                &round_id,
                bundle_index,
                proposal_id,
                stored_choice,
                stored_commitment.as_deref(),
            )?;
            if let Some(position) = vc {
                let position = u64::try_from(position).map_err(|_| VotingError::Internal {
                    message: "negative legacy VC position".to_string(),
                })?;
                if parsed.vc_tree_position != position {
                    return Err(VotingError::Internal {
                        message: format!(
                            "vote recovery bundle vc_tree_position mismatch for round={round_id}, bundle={bundle_index}, proposal={proposal_id}"
                        ),
                    });
                }
            }
            recovery_batch_digest = parsed.batch.as_ref().map(|batch| batch.digest);
        }
        let checked_van = van
            .map(|position| {
                u64::try_from(position).map_err(|_| VotingError::Internal {
                    message: "negative legacy VAN position".to_string(),
                })
            })
            .transpose()?;
        let checked_vc = vc
            .map(|position| {
                u64::try_from(position).map_err(|_| VotingError::Internal {
                    message: "negative legacy VC position".to_string(),
                })
            })
            .transpose()?;
        if let Some(position) = checked_vc {
            if !observed_output_positions.insert((network.clone(), position)) {
                return Err(VotingError::Internal {
                    message: format!(
                        "version-17 {network} vote commitment position {position} has multiple owners"
                    ),
                });
            }
        }
        let has_evidence = hash.is_some() || van.is_some() || vc.is_some() || recovery.is_some();
        if !has_evidence {
            continue;
        }

        if let Some(digest) = recovery_batch_digest {
            // Defer to one bound vote_batch generation for the whole group,
            // created as early as its earliest member.
            provable_batches
                .entry((wallet_id, network, round_id, bundle, digest))
                .and_modify(|earliest| *earliest = (*earliest).min(created))
                .or_insert(created);
            continue;
        }

        let batch_indicated = shared_hash
            || recovery.as_deref().is_some_and(|_| {
                tx.query_row(
                    "SELECT coalesce(json_type(?1, '$.batch_digest') != 'null', 0)
                     OR coalesce(json_type(?1, '$.batch_index') != 'null', 0)
                     OR coalesce(json_type(?1, '$.batch_size') != 'null', 0)",
                    [recovery.as_deref().unwrap()],
                    |row| row.get::<_, bool>(0),
                )
                .unwrap_or(true)
            });
        let Some(identity) = v17_identity(
            &wallet_id,
            &network,
            &round_id,
            bundle,
            ChainSubmissionTarget::Vote { proposal_id },
        )?
        else {
            continue;
        };

        let import = if recovery.is_some() && !batch_indicated {
            // Bind valid recovery, but quarantine an underivable historical
            // generation instead of blocking the complete database upgrade.
            match generation_for_vote(tx, &identity) {
                Ok(bound) => legacy_import_or_recovering(
                    bound.generation().digest().as_bytes().to_vec(),
                    bound.expected_layout(),
                    checked_van,
                    checked_vc.into_iter().collect(),
                )?,
                Err(error @ VotingError::Storage { .. }) => return Err(error),
                Err(_) => V17Import::generation_derivation_failed_guard(),
            }
        } else if !batch_indicated && checked_van.is_some() && checked_vc.is_some() {
            // Positions without recovery material: version 17 regarded these
            // outputs as confirmed at these locations, and the observed
            // successor VAN is what the next proposal consumes.
            V17Import::legacy_projection(checked_van.unwrap(), checked_vc.unwrap())?
        } else {
            V17Import::recovery_unavailable_guard()
        };
        register_v17_legacy_import_van(&mut observed_output_positions, &network, &import)?;
        insert_v17_submission(
            tx,
            &identity,
            "vote",
            Some(proposal_id),
            None,
            created,
            import,
        )?;
    }

    for ((wallet_id, network, round_id, bundle, digest), created) in provable_batches {
        let Some(identity) = v17_identity(
            &wallet_id,
            &network,
            &round_id,
            bundle,
            ChainSubmissionTarget::VoteBatch {
                ordered_batch_digest: digest,
            },
        )?
        else {
            continue;
        };
        let bound = match generation_for_vote_batch(tx, &identity) {
            Ok(bound) => bound,
            Err(error @ VotingError::Storage { .. }) => return Err(error),
            Err(_) => {
                insert_v17_submission(
                    tx,
                    &identity,
                    "vote_batch",
                    None,
                    Some(digest.to_vec()),
                    created,
                    V17Import::generation_derivation_failed_guard(),
                )?;
                continue;
            }
        };
        let final_van: Option<i64> = tx
            .query_row(
                "SELECT van_leaf_position FROM bundles
                  WHERE round_id=?1 AND wallet_id=?2 AND bundle_index=?3",
                rusqlite::params![round_id, wallet_id, bundle],
                |row| row.get(0),
            )
            .map_err(migration_error)?;
        let final_van = final_van
            .map(|position| {
                u64::try_from(position).map_err(|_| VotingError::Internal {
                    message: "negative legacy VAN position".to_string(),
                })
            })
            .transpose()?;
        // Positions are taken in the batch's signed action order, which is the
        // order the bound generation's expected layout also uses.
        let mut positions = Vec::with_capacity(bound.ordered_proposal_ids().len());
        for proposal_id in bound.ordered_proposal_ids() {
            let position: Option<i64> = tx
                .query_row(
                    "SELECT vc_tree_position FROM votes
                      WHERE round_id=?1 AND wallet_id=?2 AND bundle_index=?3 AND proposal_id=?4",
                    rusqlite::params![round_id, wallet_id, bundle, proposal_id],
                    |row| row.get(0),
                )
                .map_err(migration_error)?;
            let Some(position) = position else {
                positions.clear();
                break;
            };
            positions.push(u64::try_from(position).map_err(|_| VotingError::Internal {
                message: "negative legacy VC position".to_string(),
            })?);
        }
        let import = legacy_import_or_recovering(
            bound.generation().digest().as_bytes().to_vec(),
            bound.expected_layout(),
            final_van,
            positions,
        )?;
        register_v17_legacy_import_van(&mut observed_output_positions, &network, &import)?;
        insert_v17_submission(
            tx,
            &identity,
            "vote_batch",
            None,
            Some(digest.to_vec()),
            created,
            import,
        )?;
    }

    let mut bundles = tx
        .prepare(
            "SELECT b.round_id, b.wallet_id, r.network, b.bundle_index,
                    b.delegation_tx_hash, b.van_leaf_position, r.created_at
               FROM bundles b JOIN rounds r USING (round_id, wallet_id)
              WHERE b.delegation_tx_hash IS NOT NULL OR b.van_leaf_position IS NOT NULL
              ORDER BY b.wallet_id, b.round_id, b.bundle_index",
        )
        .map_err(migration_error)?;
    let delegation_rows = bundles
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(6)?,
            ))
        })
        .map_err(migration_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(migration_error)?;
    drop(bundles);
    for (round_id, wallet_id, network, bundle, created_at) in delegation_rows {
        // A confirmed successor vote in the same bundle makes the original
        // delegation evidence obsolete. Importing a delegation row for it would
        // permanently block later generations for no gain.
        let has_confirmed_successor: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM chain_submissions
                  WHERE round_id=?1 AND wallet_id=?2 AND network=?3
                    AND bundle_index=?4 AND kind IN ('vote','vote_batch')
                    AND state='confirmed')",
                rusqlite::params![round_id, wallet_id, network, bundle],
                |row| row.get(0),
            )
            .map_err(migration_error)?;
        if has_confirmed_successor {
            continue;
        }
        let Some(identity) = v17_identity(
            &wallet_id,
            &network,
            &round_id,
            bundle,
            ChainSubmissionTarget::Delegation,
        )?
        else {
            continue;
        };
        // Delegation setup, proof, nullifiers, and VAN randomizer determine the
        // generation on their own; the SpendAuth signature is excluded from the
        // digest and is not needed here.
        let import = match complete_generation_for_delegation(tx, &identity) {
            Ok(Some(bound)) => {
                // The bundle's current VAN position is only the *original*
                // delegation output while no vote has advanced it. Once a vote
                // has, the recorded position describes a later generation and
                // must never be attributed to the delegation.
                let advanced: bool = tx
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM votes
                          WHERE round_id=?1 AND wallet_id=?2 AND bundle_index=?3
                            AND vc_tree_position IS NOT NULL)",
                        rusqlite::params![round_id, wallet_id, bundle],
                        |row| row.get(0),
                    )
                    .map_err(migration_error)?;
                let delegation_van: Option<i64> = if advanced {
                    None
                } else {
                    tx.query_row(
                        "SELECT van_leaf_position FROM bundles
                          WHERE round_id=?1 AND wallet_id=?2 AND bundle_index=?3",
                        rusqlite::params![round_id, wallet_id, bundle],
                        |row| row.get(0),
                    )
                    .map_err(migration_error)?
                };
                let delegation_van = delegation_van
                    .map(|position| {
                        u64::try_from(position).map_err(|_| VotingError::Internal {
                            message: "negative legacy VAN position".to_string(),
                        })
                    })
                    .transpose()?;
                legacy_import_or_recovering(
                    bound.generation().digest().as_bytes().to_vec(),
                    bound.expected_layout(),
                    delegation_van,
                    vec![],
                )?
            }
            // Delegation setup material is absent, so no generation exists to
            // bind. The evidence is preserved as a permanently unbound guard
            // rather than guessing which generation produced the projection.
            Ok(None) => V17Import::recovery_unavailable_guard(),
            Err(error @ VotingError::Storage { .. }) => return Err(error),
            Err(_) => V17Import::generation_derivation_failed_guard(),
        };
        register_v17_legacy_import_van(&mut observed_output_positions, &network, &import)?;
        insert_v17_submission(tx, &identity, "delegation", None, None, created_at, import)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
