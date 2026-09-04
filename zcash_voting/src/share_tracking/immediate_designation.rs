//! The round's immediate helper-share designation as durable state.
//!
//! Exactly one share per round is submitted immediately. Which one is
//! decided once, from the complete ballot, when the designated vote's own
//! plan is first prepared, and the decision is written to its own row in
//! that transaction. Every later reader takes the row as authoritative and
//! never re-derives it, so a designated proposal that later leaves the
//! roster keeps the designation and no plan ever names a second one.

use rusqlite::{named_params, Connection, OptionalExtension};

use crate::share_policy::ImmediateShareKey;
use crate::types::VotingError;

/// The round's durable designation, if one has been made.
pub(crate) fn round_immediate_share(
    conn: &Connection,
    round_id: &str,
    wallet_id: &str,
) -> Result<Option<ImmediateShareKey>, VotingError> {
    conn.query_row(
        "SELECT bundle_index, proposal_id, share_index FROM round_immediate_share
         WHERE round_id = :round_id AND wallet_id = :wallet_id",
        named_params! { ":round_id": round_id, ":wallet_id": wallet_id },
        |row| {
            Ok(ImmediateShareKey {
                bundle_index: row.get::<_, i64>(0)? as u32,
                proposal_id: row.get::<_, i64>(1)? as u32,
                share_index: row.get::<_, i64>(2)? as u32,
            })
        },
    )
    .optional()
    .map_err(|e| VotingError::from_sqlite("load round immediate share designation", &e))
}

/// Designates `proposed` as the round's immediate share unless a
/// designation already exists, and returns the durable one. The first
/// writer wins; a concurrent writer observes its row.
pub(crate) fn designate_round_immediate_share(
    conn: &Connection,
    round_id: &str,
    wallet_id: &str,
    proposed: ImmediateShareKey,
) -> Result<ImmediateShareKey, VotingError> {
    conn.execute(
        "INSERT OR IGNORE INTO round_immediate_share
         (round_id, wallet_id, bundle_index, proposal_id, share_index, designated_at)
         VALUES (:round_id, :wallet_id, :bundle_index, :proposal_id, :share_index,
                 strftime('%s','now'))",
        named_params! {
            ":round_id": round_id,
            ":wallet_id": wallet_id,
            ":bundle_index": proposed.bundle_index as i64,
            ":proposal_id": proposed.proposal_id as i64,
            ":share_index": proposed.share_index as i64,
        },
    )
    .map_err(|e| VotingError::from_sqlite("designate round immediate share", &e))?;
    round_immediate_share(conn, round_id, wallet_id)?.ok_or_else(|| VotingError::Internal {
        message: "round immediate share designation was not found after writing it".to_string(),
    })
}
