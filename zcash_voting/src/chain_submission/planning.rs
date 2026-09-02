//! Read-only planning projections over the authoritative submission table.
//!
//! Planners and recovery snapshots must describe chain work from
//! `chain_submissions` rather than from the version-17 domain columns. Those
//! columns record a hash only once a transaction confirms, so a bundle whose
//! generation is `Submitting`, `Tracking`, or `Recovering` would otherwise look
//! unsubmitted and invite a second dispatch of a transaction already on the
//! wire.
//!
//! These helpers are deliberately narrow: they answer "what does the lifecycle
//! already know about this target" and never mutate.

use rusqlite::{named_params, Connection, OptionalExtension};

use crate::storage::VotingDb;
use crate::types::VotingError;

/// Chain-submission target a planner is asking about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlanningTarget {
    /// The bundle's delegation transaction.
    Delegation,
    /// One vote, matched against its own singleton generation.
    Vote { proposal_id: u32 },
}

/// Returns whether one bundle came from a delegation capability import.
///
/// Imported bundles deliberately omit local note selection; locally prepared
/// bundles always persist it. The marker remains stable after confirmation.
pub(crate) fn delegation_is_capability_imported(
    db: &VotingDb,
    round_id: &str,
    bundle_index: u32,
) -> Result<bool, VotingError> {
    db.conn()
        .query_row(
            "SELECT note_positions_blob IS NULL
             FROM bundles
             WHERE round_id = :round_id
               AND wallet_id = :wallet_id
               AND bundle_index = :bundle_index",
            named_params! {
                ":round_id": round_id,
                ":wallet_id": db.wallet_id(),
                ":bundle_index": i64::from(bundle_index),
            },
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| VotingError::Internal {
            message: format!("failed to classify delegation bundle source: {error}"),
        })?
        .ok_or_else(|| VotingError::InvalidInput {
            message: format!("bundle not found for round {round_id} index {bundle_index}"),
        })
}

/// Returns the transaction hash the lifecycle associates with `target`.
///
/// Matching is exact: a batch row names no members before confirmation, so
/// matching a vote against any batch on its bundle would attribute another
/// generation's hash to an unrelated vote. A batch member reports no hash until
/// confirmation writes the shared hash into its own projection column, which
/// the caller's fallback then reads.
///
/// Prefers the confirmed hash, then the candidate hash of an in-flight
/// generation. Returns `None` when no authoritative row exists, when the row
/// has no hash yet, or when confirmation came from tree matching rather than a
/// hash. Callers fall back to the legacy projection column so migrated
/// version-17 rows keep reporting their historical hash.
pub(crate) fn lifecycle_transaction_hash(
    conn: &Connection,
    wallet_id: &str,
    round_id: &str,
    bundle_index: u32,
    target: PlanningTarget,
) -> Result<Option<String>, VotingError> {
    // The SQL differs by target, and rusqlite rejects a named parameter the
    // statement does not reference, so each branch binds exactly its own.
    // `chain_submissions` is unique per (wallet, network, round, kind, bundle,
    // proposal), so one vote can own rows on more than one network. Planners
    // carry no network context and cannot tell which row is meant, so this asks for the
    // set of distinct hashes and refuses to guess when it is not a singleton:
    // reporting no hash is honest, attributing another chain's transaction to
    // this vote is not.
    const SELECT: &str =
        "SELECT DISTINCT COALESCE(cs.confirmed_transaction_hash, cs.candidate_transaction_hash)
           FROM chain_submissions cs
          WHERE cs.round_id = :round_id
            AND cs.wallet_id = :wallet_id
            AND cs.bundle_index = :bundle_index
            AND COALESCE(cs.confirmed_transaction_hash, cs.candidate_transaction_hash) IS NOT NULL
            AND ";
    const ORDER: &str = "
          LIMIT 2";

    let mut hashes: Vec<Vec<u8>> = Vec::new();
    {
        let (sql, delegation) = match target {
            PlanningTarget::Delegation => (format!("{SELECT}cs.kind = 'delegation'{ORDER}"), true),
            PlanningTarget::Vote { .. } => (
                format!("{SELECT}cs.kind = 'vote' AND cs.proposal_id = :proposal{ORDER}"),
                false,
            ),
        };
        let mut statement = conn.prepare(&sql).map_err(|e| VotingError::Internal {
            message: format!("failed to prepare lifecycle transaction hash query: {e}"),
        })?;
        let mut rows = if delegation {
            statement.query(named_params! {
                ":round_id": round_id,
                ":wallet_id": wallet_id,
                ":bundle_index": bundle_index as i64,
            })
        } else {
            let PlanningTarget::Vote { proposal_id } = target else {
                unreachable!("delegation handled above")
            };
            statement.query(named_params! {
                ":round_id": round_id,
                ":wallet_id": wallet_id,
                ":bundle_index": bundle_index as i64,
                ":proposal": proposal_id as i64,
            })
        }
        .map_err(|e| VotingError::Internal {
            message: format!("failed to load lifecycle transaction hash: {e}"),
        })?;
        while let Some(row) = rows.next().map_err(|e| VotingError::Internal {
            message: format!("failed to read lifecycle transaction hash: {e}"),
        })? {
            hashes.push(row.get(0).map_err(|e| VotingError::Internal {
                message: format!("failed to decode lifecycle transaction hash: {e}"),
            })?);
        }
    }
    let hash = match hashes.len() {
        1 => hashes.pop(),
        _ => None,
    };
    Ok(hash.map(hex::encode))
}

/// Transaction hash to report for one bundle's delegation.
///
/// Prefers the authoritative row so an in-flight generation is visible before
/// confirmation, then falls back to the version-17 projection column so
/// migrated rows keep their historical hash.
pub(crate) fn delegation_transaction_hash(
    db: &VotingDb,
    round_id: &str,
    bundle_index: u32,
) -> Result<Option<String>, VotingError> {
    if let Some(hash) = lifecycle_transaction_hash(
        &db.conn(),
        &db.wallet_id(),
        round_id,
        bundle_index,
        PlanningTarget::Delegation,
    )? {
        return Ok(Some(hash));
    }
    db.get_delegation_tx_hash(round_id, bundle_index)
}

/// Transaction hash to report for one vote, singleton or batch member.
///
/// Uses the same authority and fallback as [`delegation_transaction_hash`].
pub(crate) fn vote_transaction_hash(
    db: &VotingDb,
    round_id: &str,
    bundle_index: u32,
    proposal_id: u32,
) -> Result<Option<String>, VotingError> {
    if let Some(hash) = lifecycle_transaction_hash(
        &db.conn(),
        &db.wallet_id(),
        round_id,
        bundle_index,
        PlanningTarget::Vote { proposal_id },
    )? {
        return Ok(Some(hash));
    }
    db.get_vote_tx_hash(round_id, bundle_index, proposal_id)
}
