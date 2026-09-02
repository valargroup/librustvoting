//! Canonical per-artifact lifecycle phases.
//!
//! A voting round can contain multiple bundles. Each bundle may progress at a
//! different pace, so the stable API reports delegation status per bundle
//! instead of maintaining one lossy round-level phase.

use rusqlite::{named_params, OptionalExtension};

use crate::{storage::VotingDb, types::VotingError};

/// Correlated SQL expression for a successful ZKP #1 whose persisted setup can
/// still be signed, submitted, and used by the later vote proof.
const REUSABLE_DELEGATION_PROOF_SQL: &str = "EXISTS(
    SELECT 1 FROM proofs p
    WHERE p.round_id = b.round_id
      AND p.wallet_id = b.wallet_id
      AND p.bundle_index = b.bundle_index
      AND p.success = 1
      AND p.proof IS NOT NULL
      AND b.van_comm_rand IS NOT NULL
      AND b.dummy_nullifiers IS NOT NULL
      AND b.rho_signed IS NOT NULL
      AND b.padded_note_data IS NOT NULL
      AND b.nf_signed IS NOT NULL
      AND b.cmx_new IS NOT NULL
      AND b.alpha IS NOT NULL
      AND b.rseed_signed IS NOT NULL
      AND b.rseed_output IS NOT NULL
      AND b.gov_comm IS NOT NULL
      AND b.total_note_value IS NOT NULL
      AND b.address_index IS NOT NULL
      AND b.rk IS NOT NULL
      AND b.gov_nullifiers_blob IS NOT NULL
      AND b.padded_note_secrets IS NOT NULL
      AND b.pczt_sighash IS NOT NULL
      AND b.tx1_effects IS NOT NULL
)";

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

/// Wallet-facing workflow phase strings used by resume orchestration.
///
/// This is a compatibility view that collapses the canonical per-artifact phases
/// into the stable vocabulary consumed by app state machines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum WorkflowPhase {
    Prepared,
    Signed,
    SubmittedDelegation,
    SubmittedVote,
    SubmittedShare,
    Confirmed,
}

/// Cast-vote lifecycle for one bundle/proposal pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum VotePhase {
    /// The vote row exists, but no recovery bundle has been persisted.
    Prepared,
    /// The ZKP #2 bundle, share recovery data, and cast-vote signature are persisted.
    Committed,
    /// A cast-vote transaction hash has been recorded.
    Submitted,
    /// The vote commitment tree position has been recorded.
    Confirmed,
}

impl VotePhase {
    /// Returns the stable string used by FFI layers and UI state machines.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::Committed => "committed",
            Self::Submitted => "submitted",
            Self::Confirmed => "confirmed",
        }
    }
}

/// Helper-share lifecycle for one delegated share.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SharePhase {
    /// A helper-server submission record exists.
    Submitted,
    /// The helper share has been confirmed on-chain.
    Confirmed,
}

impl SharePhase {
    /// Returns the stable string used by FFI layers and UI state machines.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Submitted => "submitted",
            Self::Confirmed => "confirmed",
        }
    }
}

impl DelegationPhase {
    /// Returns whether this phase can skip local ZKP #1 generation.
    ///
    /// Wallet integrations should use this method instead of matching phase
    /// variants themselves so future lifecycle phases retain crate-owned proof
    /// semantics.
    pub fn has_persisted_proof(self) -> bool {
        matches!(self, Self::Proved | Self::Submitted | Self::Confirmed)
    }

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

impl WorkflowPhase {
    /// Returns the stable string used by FFI layers and UI state machines.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::Signed => "signed",
            Self::SubmittedDelegation => "submitted_delegation",
            Self::SubmittedVote => "submitted_vote",
            Self::SubmittedShare => "submitted_share",
            Self::Confirmed => "confirmed",
        }
    }

    /// Converts a canonical delegation phase into the merged workflow phase.
    pub fn for_delegation(phase: DelegationPhase) -> Self {
        match phase {
            DelegationPhase::Prepared => Self::Prepared,
            DelegationPhase::PcztBuilt | DelegationPhase::Proved => Self::Signed,
            DelegationPhase::Submitted => Self::SubmittedDelegation,
            DelegationPhase::Confirmed => Self::Confirmed,
        }
    }

    /// Converts a canonical vote phase into the merged workflow phase.
    pub fn for_vote(phase: VotePhase) -> Self {
        match phase {
            VotePhase::Prepared => Self::Prepared,
            VotePhase::Committed => Self::Signed,
            VotePhase::Submitted => Self::SubmittedVote,
            VotePhase::Confirmed => Self::Confirmed,
        }
    }

    /// Converts a canonical share phase into the merged workflow phase.
    pub fn for_share(phase: SharePhase) -> Self {
        match phase {
            SharePhase::Submitted => Self::SubmittedShare,
            SharePhase::Confirmed => Self::Confirmed,
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
        self.delegation_phase_if_present(round_id, bundle_index)?
            .ok_or_else(|| VotingError::InvalidInput {
                message: format!("bundle not found for round {round_id} index {bundle_index}"),
            })
    }

    /// Returns whether one bundle can skip ZKP #1 generation.
    ///
    /// A successful proof is reusable only while the persisted setup required
    /// for signing, submission, and voting remains complete. Submitted and
    /// confirmed bundles also return `true` because they no longer need local
    /// proof generation. A bundle that has not been prepared yet returns
    /// `false`, making this query suitable for deciding whether a caller needs
    /// to connect to PIR before bundle preparation begins.
    pub fn has_persisted_delegation_proof(
        &self,
        round_id: &str,
        bundle_index: u32,
    ) -> Result<bool, VotingError> {
        Ok(self
            .delegation_phase_if_present(round_id, bundle_index)?
            .is_some_and(DelegationPhase::has_persisted_proof))
    }

    fn delegation_phase_if_present(
        &self,
        round_id: &str,
        bundle_index: u32,
    ) -> Result<Option<DelegationPhase>, VotingError> {
        let conn = self.conn();
        let wallet_id = self.wallet_id();
        let query = format!(
            "SELECT b.pczt_sighash IS NOT NULL OR b.rk IS NOT NULL,
                    {REUSABLE_DELEGATION_PROOF_SQL},
                    b.delegation_tx_hash IS NOT NULL,
                    b.van_leaf_position IS NOT NULL
             FROM bundles b
             WHERE b.round_id = :round_id
               AND b.wallet_id = :wallet_id
               AND b.bundle_index = :bundle_index"
        );
        conn.query_row(
            &query,
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
        let query = format!(
            "SELECT b.bundle_index,
                    b.pczt_sighash IS NOT NULL OR b.rk IS NOT NULL,
                    {REUSABLE_DELEGATION_PROOF_SQL},
                    b.delegation_tx_hash IS NOT NULL,
                    b.van_leaf_position IS NOT NULL
             FROM bundles b
             WHERE b.round_id = :round_id
               AND b.wallet_id = :wallet_id
             ORDER BY b.bundle_index"
        );
        let mut stmt = conn.prepare(&query).map_err(|e| VotingError::Internal {
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

    /// Loads the canonical vote phase for one bundle/proposal pair.
    pub fn vote_phase(
        &self,
        round_id: &str,
        bundle_index: u32,
        proposal_id: u32,
    ) -> Result<VotePhase, VotingError> {
        let conn = self.conn();
        let wallet_id = self.wallet_id();
        let phase = conn
            .query_row(
                "SELECT tx_hash IS NOT NULL, vc_tree_position IS NOT NULL,
                        commitment_bundle_json IS NOT NULL
                 FROM votes
                 WHERE round_id = :round_id
                   AND wallet_id = :wallet_id
                   AND bundle_index = :bundle_index
                   AND proposal_id = :proposal_id",
                named_params! {
                    ":round_id": round_id,
                    ":wallet_id": wallet_id,
                    ":bundle_index": bundle_index as i64,
                    ":proposal_id": proposal_id as i64,
                },
                |row| {
                    Ok(vote_phase_from_columns(
                        row.get::<_, i64>(0)? != 0,
                        row.get::<_, i64>(1)? != 0,
                        row.get::<_, i64>(2)? != 0,
                    ))
                },
            )
            .optional()
            .map_err(|e| VotingError::Internal {
                message: format!("failed to load vote phase: {e}"),
            })?;

        phase.ok_or_else(|| VotingError::InvalidInput {
            message: format!(
                "vote not found for round {round_id} bundle {bundle_index} proposal {proposal_id}"
            ),
        })
    }

    /// Lists canonical vote phases for all votes in one round.
    pub fn vote_phases(&self, round_id: &str) -> Result<Vec<(u32, u32, VotePhase)>, VotingError> {
        let conn = self.conn();
        let wallet_id = self.wallet_id();
        let mut stmt = conn
            .prepare(
                "SELECT bundle_index, proposal_id, tx_hash IS NOT NULL,
                        vc_tree_position IS NOT NULL, commitment_bundle_json IS NOT NULL
                 FROM votes
                 WHERE round_id = :round_id AND wallet_id = :wallet_id
                 ORDER BY bundle_index, proposal_id",
            )
            .map_err(|e| VotingError::Internal {
                message: format!("failed to prepare vote phases query: {e}"),
            })?;

        let rows = stmt
            .query_map(
                named_params! { ":round_id": round_id, ":wallet_id": wallet_id },
                |row| {
                    Ok((
                        row.get::<_, i64>(0)? as u32,
                        row.get::<_, i64>(1)? as u32,
                        vote_phase_from_columns(
                            row.get::<_, i64>(2)? != 0,
                            row.get::<_, i64>(3)? != 0,
                            row.get::<_, i64>(4)? != 0,
                        ),
                    ))
                },
            )
            .map_err(|e| VotingError::Internal {
                message: format!("failed to query vote phases: {e}"),
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| VotingError::Internal {
                message: format!("failed to read vote phase row: {e}"),
            })?;
        Ok(rows)
    }

    /// Loads the canonical helper-share phase for one share record.
    pub fn share_phase(
        &self,
        round_id: &str,
        bundle_index: u32,
        proposal_id: u32,
        share_index: u32,
    ) -> Result<SharePhase, VotingError> {
        let conn = self.conn();
        let wallet_id = self.wallet_id();
        let phase = conn
            .query_row(
                "SELECT confirmed
                 FROM share_delegations
                 WHERE round_id = :round_id
                   AND wallet_id = :wallet_id
                   AND bundle_index = :bundle_index
                   AND proposal_id = :proposal_id
                   AND share_index = :share_index",
                named_params! {
                    ":round_id": round_id,
                    ":wallet_id": wallet_id,
                    ":bundle_index": bundle_index as i64,
                    ":proposal_id": proposal_id as i64,
                    ":share_index": share_index as i64,
                },
                |row| {
                    Ok(if row.get::<_, i64>(0)? != 0 {
                        SharePhase::Confirmed
                    } else {
                        SharePhase::Submitted
                    })
                },
            )
            .optional()
            .map_err(|e| VotingError::Internal {
                message: format!("failed to load share phase: {e}"),
            })?;

        phase.ok_or_else(|| VotingError::InvalidInput {
            message: format!(
                "share not found for round {round_id} bundle {bundle_index} proposal {proposal_id} share {share_index}"
            ),
        })
    }

    /// Lists canonical helper-share phases for all shares in one round.
    pub fn share_phases(
        &self,
        round_id: &str,
    ) -> Result<Vec<(u32, u32, u32, SharePhase)>, VotingError> {
        let conn = self.conn();
        let wallet_id = self.wallet_id();
        let mut stmt = conn
            .prepare(
                "SELECT bundle_index, proposal_id, share_index, confirmed
                 FROM share_delegations
                 WHERE round_id = :round_id AND wallet_id = :wallet_id
                 ORDER BY bundle_index, proposal_id, share_index",
            )
            .map_err(|e| VotingError::Internal {
                message: format!("failed to prepare share phases query: {e}"),
            })?;

        let rows = stmt
            .query_map(
                named_params! { ":round_id": round_id, ":wallet_id": wallet_id },
                |row| {
                    Ok((
                        row.get::<_, i64>(0)? as u32,
                        row.get::<_, i64>(1)? as u32,
                        row.get::<_, i64>(2)? as u32,
                        if row.get::<_, i64>(3)? != 0 {
                            SharePhase::Confirmed
                        } else {
                            SharePhase::Submitted
                        },
                    ))
                },
            )
            .map_err(|e| VotingError::Internal {
                message: format!("failed to query share phases: {e}"),
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| VotingError::Internal {
                message: format!("failed to read share phase row: {e}"),
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
        db.create_round(crate::Network::Testnet, &round_params(), None)
            .unwrap();
        db.ensure_bundles(ROUND_ID, &[note(0)]).unwrap();
        db
    }

    fn store_complete_delegation_setup(db: &VotingDb, bundle_index: u32) {
        let gov_nullifiers = vec![vec![0x0B; 32]; crate::governance::BUNDLE_NOTE_SLOTS];
        crate::storage::queries::store_delegation_data_with_pczt_fields(
            &db.conn(),
            ROUND_ID,
            WALLET_ID,
            bundle_index,
            &[0x01; 32],
            &[],
            &[0x02; 32],
            &[],
            &[0x03; 32],
            &[0x04; 32],
            &[0x05; 32],
            &[0x06; 32],
            &[0x07; 32],
            &[0x08; 32],
            1,
            0,
            &[],
            &[0x09; 32],
            &crate::tx1::placeholder_tx1_effects(),
            &[0x0C],
            &[0x0A; 32],
            &gov_nullifiers,
        )
        .unwrap();
    }

    fn store_vote_recovery_fixture(
        db: &VotingDb,
        bundle_index: u32,
        proposal_id: u32,
        vc_tree_position: Option<u64>,
    ) {
        let conn = db.conn();
        conn.execute(
            "UPDATE votes SET commitment_bundle_json = :json, vc_tree_position = :pos
             WHERE round_id = :round_id
               AND wallet_id = :wallet_id
               AND bundle_index = :bundle_index
               AND proposal_id = :proposal_id",
            named_params! {
                ":json": r#"{"format":"zcash_voting_vote_recovery_v1"}"#,
                ":pos": vc_tree_position.map(|position| position as i64),
                ":round_id": ROUND_ID,
                ":wallet_id": WALLET_ID,
                ":bundle_index": bundle_index as i64,
                ":proposal_id": proposal_id as i64,
            },
        )
        .unwrap();
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
            nullifier: vec![position as u8 + 0x02; 32],
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
        assert!(!db.has_persisted_delegation_proof(ROUND_ID, 99).unwrap());
        assert_eq!(
            db.delegation_phase(ROUND_ID, 0).unwrap(),
            DelegationPhase::Prepared
        );
        assert!(!db.has_persisted_delegation_proof(ROUND_ID, 0).unwrap());

        store_complete_delegation_setup(&db, 0);
        assert_eq!(
            db.delegation_phase(ROUND_ID, 0).unwrap(),
            DelegationPhase::PcztBuilt
        );
        assert!(!db.has_persisted_delegation_proof(ROUND_ID, 0).unwrap());

        crate::storage::queries::store_proof(&db.conn(), ROUND_ID, WALLET_ID, 0, &[0xAB; 96])
            .unwrap();
        assert_eq!(
            db.delegation_phase(ROUND_ID, 0).unwrap(),
            DelegationPhase::Proved
        );
        assert!(db.has_persisted_delegation_proof(ROUND_ID, 0).unwrap());

        db.store_delegation_tx_hash(ROUND_ID, 0, "tx").unwrap();
        assert_eq!(
            db.delegation_phase(ROUND_ID, 0).unwrap(),
            DelegationPhase::Submitted
        );
        assert!(db.has_persisted_delegation_proof(ROUND_ID, 0).unwrap());

        db.store_van_position(ROUND_ID, 0, 42).unwrap();
        assert_eq!(
            db.delegation_phase(ROUND_ID, 0).unwrap(),
            DelegationPhase::Confirmed
        );
        assert!(db.has_persisted_delegation_proof(ROUND_ID, 0).unwrap());
    }

    #[test]
    fn incomplete_successful_proof_is_not_reusable() {
        let db = db_with_bundle();
        crate::storage::queries::store_proof(&db.conn(), ROUND_ID, WALLET_ID, 0, &[0xAB; 96])
            .unwrap();

        assert_eq!(
            db.delegation_phase(ROUND_ID, 0).unwrap(),
            DelegationPhase::Prepared
        );
        assert!(!db.has_persisted_delegation_proof(ROUND_ID, 0).unwrap());
        assert_eq!(
            db.delegation_phases(ROUND_ID).unwrap(),
            vec![(0, DelegationPhase::Prepared)]
        );
    }

    #[test]
    fn delegation_phases_are_sorted_by_bundle_index() {
        let db = VotingDb::open_in_memory().unwrap();
        db.set_wallet_id(WALLET_ID);
        db.create_round(crate::Network::Testnet, &round_params(), None)
            .unwrap();
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

    #[test]
    fn vote_phase_advances_from_persisted_artifacts() {
        let db = db_with_bundle();
        crate::storage::queries::store_vote(&db.conn(), ROUND_ID, WALLET_ID, 0, 1, 2, &[0xCA; 32])
            .unwrap();
        assert_eq!(db.vote_phase(ROUND_ID, 0, 1).unwrap(), VotePhase::Prepared);

        store_vote_recovery_fixture(&db, 0, 1, None);
        assert_eq!(db.vote_phase(ROUND_ID, 0, 1).unwrap(), VotePhase::Committed);

        db.record_vote_submission(ROUND_ID, 0, 1, "tx").unwrap();
        store_vote_recovery_fixture(&db, 0, 1, Some(456));
        assert_eq!(db.vote_phase(ROUND_ID, 0, 1).unwrap(), VotePhase::Confirmed);
    }

    #[test]
    fn vote_and_share_phase_lists_are_sorted() {
        let db = db_with_bundle();
        crate::storage::queries::store_vote(&db.conn(), ROUND_ID, WALLET_ID, 0, 2, 1, &[0xCA; 32])
            .unwrap();
        crate::storage::queries::store_vote(&db.conn(), ROUND_ID, WALLET_ID, 0, 1, 2, &[0xCB; 32])
            .unwrap();
        db.record_share_delegation(
            ROUND_ID,
            0,
            1,
            1,
            &["https://helper.example".to_string()],
            &[0x44; 32],
            0,
        )
        .unwrap();

        let vote_phases = db.vote_phases(ROUND_ID).unwrap();
        let share_phases = db.share_phases(ROUND_ID).unwrap();

        assert_eq!(
            vote_phases,
            vec![(0, 1, VotePhase::Prepared), (0, 2, VotePhase::Prepared)]
        );
        assert_eq!(share_phases, vec![(0, 1, 1, SharePhase::Submitted)]);

        assert_eq!(
            db.share_phase(ROUND_ID, 0, 1, 1).unwrap(),
            SharePhase::Submitted
        );
        db.mark_share_confirmed(ROUND_ID, 0, 1, 1).unwrap();
        assert_eq!(
            db.share_phase(ROUND_ID, 0, 1, 1).unwrap(),
            SharePhase::Confirmed
        );
    }

    #[test]
    fn workflow_phase_mapping_and_strings_are_stable() {
        assert_eq!(
            WorkflowPhase::for_delegation(DelegationPhase::Prepared).as_str(),
            "prepared"
        );
        assert_eq!(
            WorkflowPhase::for_delegation(DelegationPhase::PcztBuilt).as_str(),
            "signed"
        );
        assert_eq!(
            WorkflowPhase::for_delegation(DelegationPhase::Proved).as_str(),
            "signed"
        );
        assert_eq!(
            WorkflowPhase::for_delegation(DelegationPhase::Submitted).as_str(),
            "submitted_delegation"
        );
        assert_eq!(
            WorkflowPhase::for_delegation(DelegationPhase::Confirmed).as_str(),
            "confirmed"
        );

        assert_eq!(
            WorkflowPhase::for_vote(VotePhase::Prepared).as_str(),
            "prepared"
        );
        assert_eq!(
            WorkflowPhase::for_vote(VotePhase::Committed).as_str(),
            "signed"
        );
        assert_eq!(
            WorkflowPhase::for_vote(VotePhase::Submitted).as_str(),
            "submitted_vote"
        );
        assert_eq!(
            WorkflowPhase::for_vote(VotePhase::Confirmed).as_str(),
            "confirmed"
        );

        assert_eq!(
            WorkflowPhase::for_share(SharePhase::Submitted).as_str(),
            "submitted_share"
        );
        assert_eq!(
            WorkflowPhase::for_share(SharePhase::Confirmed).as_str(),
            "confirmed"
        );
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

fn vote_phase_from_columns(
    has_tx_hash: bool,
    has_vc_position: bool,
    has_recovery_bundle: bool,
) -> VotePhase {
    if has_tx_hash && has_vc_position && has_recovery_bundle {
        VotePhase::Confirmed
    } else if has_tx_hash {
        VotePhase::Submitted
    } else if has_recovery_bundle {
        VotePhase::Committed
    } else {
        VotePhase::Prepared
    }
}
