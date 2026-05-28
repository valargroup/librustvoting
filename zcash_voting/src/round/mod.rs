//! Round setup and bundle planning API.
//!
//! This module is the stable setup surface for wallet SDKs. It keeps database
//! ownership in [`VotingDb`] while hiding the low-level query helpers that back
//! the SQLite schema.

use std::path::Path;

use rusqlite::{named_params, OptionalExtension};

use crate::{
    storage::{queries, VotingDb as InnerVotingDb},
    types::{chunk_notes, BundleSetupResult, NoteInfo, VotingError, VotingRoundParams},
};

/// Stable public name for vote-round parameters supplied by the vote chain.
pub type RoundParams = VotingRoundParams;

/// Public database handle for persisted voting state.
pub type VotingDb = InnerVotingDb;

/// Query summary for one voting round in the current wallet scope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoundInfo {
    pub round_id: String,
    pub snapshot_height: u64,
    pub hotkey_address: Option<String>,
    pub eligible_weight: Option<u64>,
    pub bundle_count: u32,
    pub created_at: u64,
}

/// Result of idempotently planning or validating note bundles for a round.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BundleLayout {
    pub bundle_count: u32,
    pub eligible_weight: u64,
    pub dropped_count: u32,
}

impl From<BundleSetupResult> for BundleLayout {
    fn from(result: BundleSetupResult) -> Self {
        Self {
            bundle_count: result.bundle_count,
            eligible_weight: result.eligible_weight_zatoshi,
            dropped_count: 0,
        }
    }
}

/// Returns the canonical eligible note bundles for a round note set.
///
/// This is the read-only counterpart to [`VotingDb::ensure_bundles`]. Wallets
/// that need to operate on one bundle after setup can use this instead of
/// depending on the lower-level chunking internals.
pub fn note_bundles(notes: &[NoteInfo]) -> Result<Vec<Vec<NoteInfo>>, VotingError> {
    crate::types::validate_notes_for_round(notes)?;
    Ok(chunk_notes(notes).bundles)
}

impl VotingDb {
    /// Opens or creates a voting database at `path` and runs migrations.
    ///
    /// Call [`VotingDb::set_wallet_id`] before performing wallet-scoped round
    /// operations. Passing `:memory:` is supported through the legacy string
    /// API; prefer [`VotingDb::open_in_memory`] for in-memory tests.
    pub fn open_path(path: &Path) -> Result<Self, VotingError> {
        Self::open(path.to_str().ok_or_else(|| VotingError::InvalidInput {
            message: "voting database path is not valid UTF-8".to_string(),
        })?)
    }

    /// Opens a fresh in-memory voting database for tests and examples.
    pub fn open_in_memory() -> Result<Self, VotingError> {
        Self::open(":memory:")
    }

    /// Creates a voting round for the current wallet.
    ///
    /// The round id comes from `params.vote_round_id`. This call persists the
    /// round parameters and is idempotent only at the caller layer; inserting an
    /// already-existing `(wallet_id, round_id)` pair returns an error from the
    /// underlying SQLite constraint.
    pub fn create_round(&self, params: &RoundParams) -> Result<(), VotingError> {
        crate::types::validate_round_params(params)?;
        self.init_round(params, None)
    }

    /// Loads one round summary for the current wallet.
    ///
    /// Returns `Ok(None)` when the round does not exist. Other database errors
    /// are returned as [`VotingError::Internal`].
    pub fn round(&self, round_id: &str) -> Result<Option<RoundInfo>, VotingError> {
        let conn = self.conn();
        let wallet_id = self.wallet_id();
        let row = conn
            .query_row(
                "SELECT snapshot_height, created_at
                 FROM rounds
                 WHERE round_id = :round_id AND wallet_id = :wallet_id",
                named_params! { ":round_id": round_id, ":wallet_id": wallet_id },
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()
            .map_err(|e| VotingError::Internal {
                message: format!("failed to load round {round_id}: {e}"),
            })?;

        let Some((snapshot_height, created_at)) = row else {
            return Ok(None);
        };

        let bundle_count = queries::get_bundle_count(&conn, round_id, &wallet_id)?;
        let eligible_weight = round_eligible_weight(&conn, round_id, &wallet_id)?;

        Ok(Some(RoundInfo {
            round_id: round_id.to_string(),
            snapshot_height: snapshot_height as u64,
            hotkey_address: None,
            eligible_weight,
            bundle_count,
            created_at: created_at as u64,
        }))
    }

    /// Lists all rounds for the current wallet in newest-first order.
    pub fn rounds(&self) -> Result<Vec<RoundInfo>, VotingError> {
        self.list_rounds()?
            .into_iter()
            .map(|summary| {
                self.round(&summary.round_id)?
                    .ok_or_else(|| VotingError::Internal {
                        message: format!("round disappeared while listing: {}", summary.round_id),
                    })
            })
            .collect()
    }

    /// Deletes all persisted state for one round in the current wallet scope.
    pub fn delete_round(&self, round_id: &str) -> Result<(), VotingError> {
        self.clear_round(round_id)
    }

    /// Creates bundle rows for `notes`, or validates existing bundle rows.
    ///
    /// The note ordering and weight quantization are the canonical library
    /// policy. On first call, surviving bundles are persisted. On later calls,
    /// the same notes must reproduce the stored bundle identities.
    pub fn ensure_bundles(
        &self,
        round_id: &str,
        notes: &[NoteInfo],
    ) -> Result<BundleLayout, VotingError> {
        crate::types::validate_notes_for_round(notes)?;
        let plan = chunk_notes(notes);
        let expected_count = plan.bundles.len() as u32;
        let existing_count = self.get_bundle_count(round_id)?;

        if existing_count == 0 {
            let (bundle_count, eligible_weight) = self.setup_bundles(round_id, notes)?;
            return Ok(BundleLayout {
                bundle_count,
                eligible_weight,
                dropped_count: plan.dropped_count as u32,
            });
        }

        if existing_count != expected_count {
            return Err(VotingError::InvalidInput {
                message: format!(
                    "existing bundle count {existing_count} does not match planned bundle count {expected_count}"
                ),
            });
        }

        let conn = self.conn();
        let wallet_id = self.wallet_id();
        for (bundle_index, bundle_notes) in plan.bundles.iter().enumerate() {
            queries::require_bundle_notes(
                &conn,
                round_id,
                &wallet_id,
                bundle_index as u32,
                bundle_notes,
            )?;
        }

        Ok(BundleLayout {
            bundle_count: expected_count,
            eligible_weight: plan.eligible_weight,
            dropped_count: plan.dropped_count as u32,
        })
    }
}

fn round_eligible_weight(
    conn: &rusqlite::Connection,
    round_id: &str,
    wallet_id: &str,
) -> Result<Option<u64>, VotingError> {
    let total: Option<i64> = conn
        .query_row(
            "SELECT SUM((total_note_value / :ballot_divisor) * :ballot_divisor)
             FROM bundles
             WHERE round_id = :round_id AND wallet_id = :wallet_id",
            named_params! {
                ":round_id": round_id,
                ":wallet_id": wallet_id,
                ":ballot_divisor": crate::governance::BALLOT_DIVISOR as i64,
            },
            |row| row.get(0),
        )
        .map_err(|e| VotingError::Internal {
            message: format!("failed to calculate round eligible weight: {e}"),
        })?;

    Ok(total.map(|v| v as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROUND_ID: &str = "0101010101010101010101010101010101010101010101010101010101010101";

    fn test_db(wallet_id: &str) -> VotingDb {
        let db = VotingDb::open_in_memory().unwrap();
        db.set_wallet_id(wallet_id);
        db.create_round(&round_params()).unwrap();
        db
    }

    fn round_params() -> RoundParams {
        RoundParams {
            vote_round_id: ROUND_ID.to_string(),
            snapshot_height: 1000,
            ea_pk: vec![0xEA; 32],
            nc_root: vec![0xAA; 32],
            nullifier_imt_root: vec![0xBB; 32],
        }
    }

    fn note(position: u64, value: u64) -> NoteInfo {
        NoteInfo {
            commitment: vec![position as u8; 32],
            nullifier: vec![position as u8 + 1; 32],
            value,
            position,
            diversifier: vec![0x03; 11],
            rho: vec![0x04; 32],
            rseed: vec![0x05; 32],
            scope: 0,
            ufvk_str: "uview1test".to_string(),
        }
    }

    #[test]
    fn ensure_bundles_creates_and_validates_idempotently() {
        let db = test_db("wallet-a");
        let notes = vec![note(0, crate::governance::BALLOT_DIVISOR)];

        let created = db.ensure_bundles(ROUND_ID, &notes).unwrap();
        let reused = db.ensure_bundles(ROUND_ID, &notes).unwrap();

        assert_eq!(created.bundle_count, 1);
        assert_eq!(created.eligible_weight, crate::governance::BALLOT_DIVISOR);
        assert_eq!(reused, created);
    }

    #[test]
    fn ensure_bundles_rejects_changed_existing_bundle_identity() {
        let db = test_db("wallet-b");
        db.ensure_bundles(ROUND_ID, &[note(0, crate::governance::BALLOT_DIVISOR)])
            .unwrap();

        let mut substituted = note(0, crate::governance::BALLOT_DIVISOR);
        substituted.nullifier[0] ^= 0x01;

        let err = db.ensure_bundles(ROUND_ID, &[substituted]).unwrap_err();

        assert!(err.to_string().contains("note identity mismatch"), "{err}");
    }

    #[test]
    fn round_reports_bundle_count_and_quantized_weight() {
        let db = test_db("wallet-c");
        let notes = vec![
            note(0, crate::governance::BALLOT_DIVISOR + 1),
            note(1, crate::governance::BALLOT_DIVISOR),
            note(2, 1),
            note(3, 1),
            note(4, 1),
            note(5, crate::governance::BALLOT_DIVISOR),
        ];
        let layout = db.ensure_bundles(ROUND_ID, &notes).unwrap();
        db.conn()
            .execute(
                "UPDATE bundles
                 SET total_note_value = ?1
                 WHERE round_id = ?2 AND wallet_id = ?3 AND bundle_index = 0",
                rusqlite::params![layout.eligible_weight as i64 + 1, ROUND_ID, "wallet-c"],
            )
            .unwrap();

        let round = db.round(ROUND_ID).unwrap().unwrap();

        assert_eq!(round.bundle_count, layout.bundle_count);
        assert_eq!(round.eligible_weight, Some(layout.eligible_weight));
    }
}
