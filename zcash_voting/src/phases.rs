//! Canonical per-artifact lifecycle phases.
//!
//! A voting round can contain multiple bundles. Each bundle may progress at a
//! different pace, so the stable API reports delegation status per bundle
//! instead of maintaining one lossy round-level phase.

use rusqlite::{named_params, OptionalExtension};

use crate::{storage::VotingDb, types::VotingError};

/// Delegation lifecycle for one bundle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DelegationPhase {
    /// The bundle row exists and is bound to its note identities.
    Prepared,
    /// The governance PCZT and signing fields have been persisted.
    PcztBuilt,
    /// The ZKP #1 delegation proof has been generated and persisted.
    Proved,
    /// A delegation transaction hash has been recorded.
    Submitted,
    /// The vote authority note leaf position has been recovered from chain.
    Confirmed,
}

impl DelegationPhase {
    /// Returns the stable string used by FFI layers and UI state machines.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::PcztBuilt => "pczt_built",
            Self::Proved => "proved",
            Self::Submitted => "submitted",
            Self::Confirmed => "confirmed",
        }
    }
}

impl VotingDb {
    /// Loads the canonical delegation phase for one bundle.
    ///
    /// Returns [`VotingError::InvalidInput`] when the bundle row does not exist
    /// for the current wallet.
    pub fn delegation_phase(
        &self,
        round_id: &str,
        bundle_index: u32,
    ) -> Result<DelegationPhase, VotingError> {
        let conn = self.conn();
        let wallet_id = self.wallet_id();
        let phase = conn
            .query_row(
                "SELECT b.pczt_sighash IS NOT NULL OR b.rk IS NOT NULL,
                        EXISTS(
                            SELECT 1 FROM proofs p
                            WHERE p.round_id = b.round_id
                              AND p.wallet_id = b.wallet_id
                              AND p.bundle_index = b.bundle_index
                              AND p.success = 1
                        ),
                        b.delegation_tx_hash IS NOT NULL,
                        b.van_leaf_position IS NOT NULL
                 FROM bundles b
                 WHERE b.round_id = :round_id
                   AND b.wallet_id = :wallet_id
                   AND b.bundle_index = :bundle_index",
                named_params! {
                    ":round_id": round_id,
                    ":wallet_id": wallet_id,
                    ":bundle_index": bundle_index as i64,
                },
                |row| {
                    Ok(phase_from_columns(
                        row.get::<_, i64>(0)? != 0,
                        row.get::<_, i64>(1)? != 0,
                        row.get::<_, i64>(2)? != 0,
                        row.get::<_, i64>(3)? != 0,
                    ))
                },
            )
            .optional()
            .map_err(|e| VotingError::Internal {
                message: format!("failed to load delegation phase: {e}"),
            })?;

        phase.ok_or_else(|| VotingError::InvalidInput {
            message: format!("bundle not found for round {round_id} index {bundle_index}"),
        })
    }

    /// Lists canonical delegation phases for all bundles in one round.
    ///
    /// Results are sorted by `bundle_index` and scoped to the current wallet id.
    pub fn delegation_phases(
        &self,
        round_id: &str,
    ) -> Result<Vec<(u32, DelegationPhase)>, VotingError> {
        let conn = self.conn();
        let wallet_id = self.wallet_id();
        let mut stmt = conn
            .prepare(
                "SELECT b.bundle_index,
                        b.pczt_sighash IS NOT NULL OR b.rk IS NOT NULL,
                        EXISTS(
                            SELECT 1 FROM proofs p
                            WHERE p.round_id = b.round_id
                              AND p.wallet_id = b.wallet_id
                              AND p.bundle_index = b.bundle_index
                              AND p.success = 1
                        ),
                        b.delegation_tx_hash IS NOT NULL,
                        b.van_leaf_position IS NOT NULL
                 FROM bundles b
                 WHERE b.round_id = :round_id
                   AND b.wallet_id = :wallet_id
                 ORDER BY b.bundle_index",
            )
            .map_err(|e| VotingError::Internal {
                message: format!("failed to prepare delegation phases query: {e}"),
            })?;

        let rows = stmt
            .query_map(
                named_params! { ":round_id": round_id, ":wallet_id": wallet_id },
                |row| {
                    Ok((
                        row.get::<_, i64>(0)? as u32,
                        phase_from_columns(
                            row.get::<_, i64>(1)? != 0,
                            row.get::<_, i64>(2)? != 0,
                            row.get::<_, i64>(3)? != 0,
                            row.get::<_, i64>(4)? != 0,
                        ),
                    ))
                },
            )
            .map_err(|e| VotingError::Internal {
                message: format!("failed to query delegation phases: {e}"),
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| VotingError::Internal {
                message: format!("failed to read delegation phase row: {e}"),
            })?;

        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{round::RoundParams, storage::VotingDb, types::NoteInfo};

    const ROUND_ID: &str = "0101010101010101010101010101010101010101010101010101010101010101";
    const WALLET_ID: &str = "wallet";

    fn db_with_bundle() -> VotingDb {
        let db = VotingDb::open_in_memory().unwrap();
        db.set_wallet_id(WALLET_ID);
        db.create_round(&round_params()).unwrap();
        db.ensure_bundles(ROUND_ID, &[note(0)]).unwrap();
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

    fn note(position: u64) -> NoteInfo {
        NoteInfo {
            commitment: vec![0x01; 32],
            nullifier: vec![0x02; 32],
            value: crate::governance::BALLOT_DIVISOR,
            position,
            diversifier: vec![0x03; 11],
            rho: vec![0x04; 32],
            rseed: vec![0x05; 32],
            scope: 0,
            ufvk_str: "uview1test".to_string(),
        }
    }

    #[test]
    fn delegation_phase_advances_from_persisted_artifacts() {
        let db = db_with_bundle();
        assert_eq!(
            db.delegation_phase(ROUND_ID, 0).unwrap(),
            DelegationPhase::Prepared
        );

        db.conn()
            .execute(
                "UPDATE bundles SET pczt_sighash = X'01', rk = X'02'
                 WHERE round_id = ?1 AND wallet_id = ?2 AND bundle_index = 0",
                rusqlite::params![ROUND_ID, WALLET_ID],
            )
            .unwrap();
        assert_eq!(
            db.delegation_phase(ROUND_ID, 0).unwrap(),
            DelegationPhase::PcztBuilt
        );

        crate::storage::queries::store_proof(&db.conn(), ROUND_ID, WALLET_ID, 0, &[0xAB; 96])
            .unwrap();
        assert_eq!(
            db.delegation_phase(ROUND_ID, 0).unwrap(),
            DelegationPhase::Proved
        );

        db.store_delegation_tx_hash(ROUND_ID, 0, "tx").unwrap();
        assert_eq!(
            db.delegation_phase(ROUND_ID, 0).unwrap(),
            DelegationPhase::Submitted
        );

        db.store_van_position(ROUND_ID, 0, 42).unwrap();
        assert_eq!(
            db.delegation_phase(ROUND_ID, 0).unwrap(),
            DelegationPhase::Confirmed
        );
    }

    #[test]
    fn delegation_phases_are_sorted_by_bundle_index() {
        let db = VotingDb::open_in_memory().unwrap();
        db.set_wallet_id(WALLET_ID);
        db.create_round(&round_params()).unwrap();
        db.ensure_bundles(
            ROUND_ID,
            &[note(0), note(1), note(2), note(3), note(4), note(5)],
        )
        .unwrap();

        let phases = db.delegation_phases(ROUND_ID).unwrap();

        assert_eq!(phases.len(), 2);
        assert_eq!(phases[0], (0, DelegationPhase::Prepared));
        assert_eq!(phases[1], (1, DelegationPhase::Prepared));
    }
}

fn phase_from_columns(
    has_pczt: bool,
    has_proof: bool,
    has_tx_hash: bool,
    has_van_position: bool,
) -> DelegationPhase {
    if has_van_position {
        DelegationPhase::Confirmed
    } else if has_tx_hash {
        DelegationPhase::Submitted
    } else if has_proof {
        DelegationPhase::Proved
    } else if has_pczt {
        DelegationPhase::PcztBuilt
    } else {
        DelegationPhase::Prepared
    }
}
