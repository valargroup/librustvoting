use rusqlite::{named_params, Connection, Transaction};

use crate::chain_submission::generation::{
    complete_generation_for_delegation, generation_for_vote, generation_for_vote_batch,
    ExpectedTreeLayout,
};
use crate::chain_submission::identity::submission_identity_key;
use crate::chain_submission::{
    ChainSubmissionIdentity, ChainSubmissionIdentityError, ChainSubmissionTarget,
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
            ":network": crate::chain_submission::identity::network_name(identity.network()),
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
            Err(_) => V17Import::generation_derivation_failed_guard(),
        };
        register_v17_legacy_import_van(&mut observed_output_positions, &network, &import)?;
        insert_v17_submission(tx, &identity, "delegation", None, None, created_at, import)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::queries;
    use crate::VotingRoundParams;
    use rusqlite::OptionalExtension;

    fn pre_v8_schema() -> String {
        include_str!("migrations/001_init.sql").replace("    note_identity_hashes_blob BLOB,\n", "")
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
        without_helper_share_plans(&without_chain_submissions(include_str!(
            "migrations/001_init.sql"
        )))
    }

    fn v17_schema() -> String {
        without_chain_submissions(include_str!("migrations/001_init.sql"))
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
        let stored_commitment = crate::vote::stored_vote_commitment_bytes(
            &crate::vote::parse_recovery(&recovery).unwrap(),
        )
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

    /// Identity defects other than a noncanonical round id are corruption, not
    /// an excuse to silently omit authoritative lifecycle protection.
    #[test]
    fn invalid_v17_wallet_and_proposal_identities_abort_atomically() {
        for (wallet_id, proposal_id, expected_error) in [
            ("", 1, "chain submission wallet_id must not be empty"),
            ("wallet", 0, "proposal_id must be between 1 and 15, got 0"),
            ("wallet", 16, "proposal_id must be between 1 and 15, got 16"),
        ] {
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

        let (state, generation_digest, source, candidate, attempts, diagnostic_kind): BoundRowFields = conn
            .query_row(
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
        let recovery =
            recovery_json_with_tree_position(&released_singleton_recovery_json(ROUND), 8);
        let stored = crate::vote::stored_vote_commitment_bytes(
            &crate::vote::parse_recovery(&recovery).unwrap(),
        )
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
    fn v17_position_ownership_is_scoped_by_network() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&v17_schema()).unwrap();
        for (round_id, network) in [
            (
                "1111111111111111111111111111111111111111111111111111111111111111",
                crate::Network::Mainnet,
            ),
            (
                "2222222222222222222222222222222222222222222222222222222222222222",
                crate::Network::Testnet,
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
            insert_v17_delegation_with_complete_setup(
                &conn,
                ROUND,
                "wallet",
                bundle_index,
                Some(7),
            );
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
    fn v17_validated_van_ownership_is_scoped_by_network() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&v17_schema()).unwrap();
        for (round_id, network) in [
            (
                "1111111111111111111111111111111111111111111111111111111111111111",
                crate::Network::Testnet,
            ),
            (
                "2222222222222222222222222222222222222222222222222222222222222222",
                crate::Network::Mainnet,
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
    fn malformed_v17_recovery_rolls_back_without_changing_version() {
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
        conn.pragma_update(None, "user_version", 17).unwrap();

        assert!(migrate(&mut conn).is_err());
        assert_eq!(
            conn.pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                .unwrap(),
            17
        );
        assert!(!table_names(&conn).contains(&"chain_submissions".to_string()));
        assert_eq!(
            conn.query_row(
                "SELECT commitment_bundle_json FROM votes WHERE proposal_id=1",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
            "{broken"
        );
    }

    #[test]
    fn v17_recovery_must_match_its_owning_vote_row() {
        const ROUND_ID: &str = "1111111111111111111111111111111111111111111111111111111111111111";
        let base = released_singleton_recovery_json(ROUND_ID);
        let stored_commitment =
            crate::vote::stored_vote_commitment_bytes(&crate::vote::parse_recovery(&base).unwrap())
                .unwrap();
        let mismatches = [
            (
                "vote_round_id",
                serde_json::json!(
                    "2222222222222222222222222222222222222222222222222222222222222222"
                ),
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

            let error = migrate(&mut conn).unwrap_err();
            assert!(error.to_string().contains("mismatch"), "{field}: {error}");
            assert_eq!(
                conn.pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                    .unwrap(),
                17
            );
            assert!(!table_names(&conn).contains(&"chain_submissions".to_string()));
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
        let error = migrate(&mut conn).unwrap_err();
        assert!(
            error.to_string().contains("vc_tree_position mismatch"),
            "{error}"
        );
        assert_eq!(
            conn.pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                .unwrap(),
            17
        );
    }

    #[test]
    fn v17_atomic_batches_must_be_complete_and_consistent() {
        const ROUND_ID: &str = "1111111111111111111111111111111111111111111111111111111111111111";
        let (first, second) = released_batch_recovery_json(ROUND_ID);
        let stored_commitments = [&first, &second].map(|json| {
            crate::vote::stored_vote_commitment_bytes(&crate::vote::parse_recovery(json).unwrap())
                .unwrap()
        });

        let open_batch = |first_json: Option<&str>,
                          second_json: Option<&str>,
                          shared_hash: bool| {
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
            let error = migrate(&mut conn).unwrap_err();
            assert!(!error.to_string().is_empty(), "{label}");
            assert_eq!(
                conn.pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                    .unwrap(),
                17,
                "{label}"
            );
            assert!(!table_names(&conn).contains(&"chain_submissions".to_string()));
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

        let without_diagnostic_kind = include_str!("migrations/002_chain_submissions.sql")
            .replacen("    diagnostic_kind TEXT,\n", "", 1)
            .replacen(
                "    CHECK ((diagnostic_kind IS NULL) = (diagnostic IS NULL)),\n",
                "",
                1,
            );
        assert_rejected(&without_diagnostic_kind, None);
        assert_rejected(
            include_str!("migrations/002_chain_submissions.sql"),
            Some("DROP INDEX chain_submissions_candidate_owner"),
        );
        assert_rejected(
            include_str!("migrations/002_chain_submissions.sql"),
            Some("DROP TRIGGER chain_submissions_immutable_identity"),
        );
    }

    #[test]
    fn test_migrate_from_prelaunch_version_resets_existing_state() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(include_str!("migrations/001_init.sql"))
            .unwrap();
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

        let replacement_json =
            confirmed_json.replacen("\"vote_decision\":2", "\"vote_decision\":1", 1);
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
}
