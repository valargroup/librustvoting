//! Projects an authoritative `vote_batch` row onto its member votes.
//!
//! A batch binds once and owns no per-member rows, so membership is never read
//! from a stored roster. The batch generation is re-derived from the persisted
//! signed members and its digest must match the persisted row before the
//! batch's phase is applied to any member.

use std::collections::BTreeMap;

use rusqlite::{named_params, Connection};

use super::{authoritative_submission_phase, VotePhase};
use crate::{
    chain_submission::{generation_for_vote_batch, ChainSubmissionIdentity, ChainSubmissionTarget},
    storage::queries,
    types::VotingError,
};

/// Loads every member phase claimed by an authoritative batch row.
///
/// Returns `(bundle_index, proposal_id) -> phase` for each member of each
/// batch row in the round, optionally restricted to one bundle. A batch whose
/// persisted digest does not re-derive from its members, a noncanonical round
/// id, or a vote claimed by two batches is an invariant error rather than a
/// silently different phase.
pub(super) fn load_authoritative_batch_phases(
    conn: &Connection,
    wallet_id: &str,
    round_id: &str,
    bundle_index: Option<u32>,
) -> Result<BTreeMap<(u32, u32), VotePhase>, VotingError> {
    let batch_rows = {
        let mut statement = conn
            .prepare(
                "SELECT bundle_index, ordered_batch_digest, generation_digest, state
                   FROM chain_submissions
                  WHERE round_id=:round_id AND wallet_id=:wallet_id
                    AND kind='vote_batch'
                    AND (:bundle_index IS NULL OR bundle_index=:bundle_index)",
            )
            .map_err(|error| VotingError::Internal {
                message: format!("failed to prepare authoritative vote batch query: {error}"),
            })?;
        let rows = statement
            .query_map(
                named_params! {
                    ":round_id": round_id,
                    ":wallet_id": wallet_id,
                    ":bundle_index": bundle_index.map(i64::from),
                },
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .map_err(|error| VotingError::Internal {
                message: format!("failed to query authoritative vote batches: {error}"),
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| VotingError::Internal {
                message: format!("failed to read authoritative vote batch: {error}"),
            })?;
        rows
    };
    if batch_rows.is_empty() {
        return Ok(BTreeMap::new());
    }

    let round_bytes: [u8; 32] = hex::decode(round_id)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| VotingError::Internal {
            message: format!("authoritative vote batch has noncanonical round id {round_id}"),
        })?;
    let network = queries::load_round_network(conn, round_id, wallet_id)?;
    let mut phases_by_member = BTreeMap::new();

    for (stored_bundle_index, ordered_digest, generation_digest, state) in batch_rows {
        let Some(phase) = authoritative_submission_phase(Some(&state)) else {
            continue;
        };
        let stored_bundle_index =
            u32::try_from(stored_bundle_index).map_err(|_| VotingError::Internal {
                message: format!(
                    "authoritative vote batch has invalid bundle index {stored_bundle_index}"
                ),
            })?;
        let ordered_batch_digest: [u8; 32] =
            ordered_digest
                .try_into()
                .map_err(|digest: Vec<u8>| VotingError::Internal {
                    message: format!(
                        "authoritative vote batch digest must be 32 bytes, got {}",
                        digest.len()
                    ),
                })?;
        let identity = ChainSubmissionIdentity::new(
            wallet_id,
            network,
            round_bytes,
            stored_bundle_index,
            ChainSubmissionTarget::VoteBatch {
                ordered_batch_digest,
            },
        )
        .map_err(|error| VotingError::Internal {
            message: format!("invalid authoritative vote batch identity: {error}"),
        })?;
        let bound_generation = generation_for_vote_batch(conn, &identity)?;
        let expected_generation_digest = bound_generation.generation().digest();
        if generation_digest.as_slice() != expected_generation_digest.as_bytes() {
            return Err(VotingError::Internal {
                message: format!(
                    "authoritative vote batch generation digest does not match persisted members for round={round_id}, bundle={stored_bundle_index}"
                ),
            });
        }
        for &member_proposal_id in bound_generation.ordered_proposal_ids() {
            if phases_by_member
                .insert((stored_bundle_index, member_proposal_id), phase)
                .is_some()
            {
                return Err(VotingError::Internal {
                    message: format!(
                        "vote belongs to multiple authoritative batches for round={round_id}, bundle={stored_bundle_index}, proposal={member_proposal_id}"
                    ),
                });
            }
        }
    }

    Ok(phases_by_member)
}
