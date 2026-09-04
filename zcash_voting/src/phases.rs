//! Canonical per-artifact lifecycle phases.
//!
//! A voting round can contain multiple bundles. Each bundle may progress at a
//! different pace, so the stable API reports delegation status per bundle
//! instead of maintaining one lossy round-level phase.

use rusqlite::{named_params, Connection, OptionalExtension};

use crate::{
    chain_submission::{ChainSubmissionDiagnostic, ChainSubmissionDiagnosticKind},
    storage::VotingDb,
    types::VotingError,
};

mod authoritative_batch;

use authoritative_batch::load_authoritative_batch_phases;

/// Delegation phase of one bundle together with the diagnostic its
/// authoritative `chain_submissions` row stores, if any.
///
/// The diagnostic is always present for `SubmittedWithoutHash` and
/// `SubmissionRejected`, where it is the only evidence a host can show for a
/// terminal outcome, and may be present while `SubmissionManaged` describes an
/// ambiguity still under recovery.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DelegationSubmissionStatus {
    pub(crate) bundle_index: u32,
    pub(crate) phase: DelegationPhase,
    pub(crate) diagnostic: Option<ChainSubmissionDiagnostic>,
}

/// Vote phase of one `(bundle, proposal)` key together with the diagnostic of
/// its authoritative singleton or batch row, if any.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VoteSubmissionStatus {
    pub(crate) bundle_index: u32,
    pub(crate) proposal_id: u32,
    pub(crate) phase: VotePhase,
    pub(crate) diagnostic: Option<ChainSubmissionDiagnostic>,
    /// Digest of the authoritative batch row that owns this vote, when the
    /// phase comes from a batch rather than a singleton row or the domain
    /// columns. The batch row, not the member, holds the candidate hash.
    pub(crate) ordered_batch_digest: Option<[u8; 32]>,
}

/// Rebuilds the stored lifecycle diagnostic from its two projection columns.
///
/// Both columns are written together by the lifecycle store, so one without
/// the other or an unknown kind is corrupt state rather than "no diagnostic".
fn stored_submission_diagnostic(
    kind: Option<String>,
    message: Option<String>,
) -> Result<Option<ChainSubmissionDiagnostic>, VotingError> {
    match (kind, message) {
        (None, None) => Ok(None),
        (Some(kind), Some(message)) => ChainSubmissionDiagnosticKind::from_stable_name(&kind)
            .map(|kind| {
                Some(ChainSubmissionDiagnostic::from_redacted_message(
                    kind, message,
                ))
            })
            .ok_or_else(|| VotingError::Internal {
                message: format!("chain submission row stores unknown diagnostic kind {kind:?}"),
            }),
        _ => Err(VotingError::Internal {
            message: "chain submission row stores a diagnostic kind without a message or a message without a kind".to_string(),
        }),
    }
}

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
    /// Submission or reconciliation is owned by the chain lifecycle facade.
    SubmissionManaged,
    /// Dispatch is durably submitted without a usable transaction hash.
    SubmittedWithoutHash,
    /// The authoritative lifecycle row is terminally rejected.
    SubmissionRejected,
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
    /// Submission or reconciliation is owned by the chain lifecycle facade.
    SubmissionManaged,
    /// The authoritative lifecycle row is terminally rejected.
    SubmissionRejected,
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
    /// Submission or reconciliation is owned by the chain lifecycle facade.
    SubmissionManaged,
    /// Dispatch is durably submitted without a usable transaction hash.
    SubmittedWithoutHash,
    /// The authoritative lifecycle row is terminally rejected.
    SubmissionRejected,
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
            Self::SubmissionManaged => "submission_managed",
            Self::SubmittedWithoutHash => "submitted_without_hash",
            Self::SubmissionRejected => "submission_rejected",
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
    /// Returns the stable string used by FFI layers and UI state machines.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::PcztBuilt => "pczt_built",
            Self::Proved => "proved",
            Self::Submitted => "submitted",
            Self::SubmissionManaged => "submission_managed",
            Self::SubmittedWithoutHash => "submitted_without_hash",
            Self::SubmissionRejected => "submission_rejected",
            Self::Confirmed => "confirmed",
        }
    }

    /// Whether the bundle's durable generation already holds the ZKP #1
    /// delegation proof.
    ///
    /// Every phase a bundle can reach after proving answers `true`, including
    /// the lifecycle-owned and terminal submission phases: a submission row
    /// only exists once a signed bundle carrying the proof was dispatched, and
    /// the `chain_submissions` state overrides the artifact columns in
    /// [`VotingDb::delegation_phase`]. Callers use this to reuse a persisted
    /// proof instead of re-entering PIR.
    ///
    /// Exhaustive on purpose: a new phase must be classified here rather than
    /// defaulting into "re-prove".
    pub fn has_persisted_proof(self) -> bool {
        match self {
            Self::Proved
            | Self::Submitted
            | Self::SubmissionManaged
            | Self::SubmittedWithoutHash
            | Self::SubmissionRejected
            | Self::Confirmed => true,
            Self::Prepared | Self::PcztBuilt => false,
        }
    }
}

impl From<WorkflowPhase> for crate::wire::WorkflowPhaseView {
    fn from(phase: WorkflowPhase) -> Self {
        match phase {
            WorkflowPhase::Prepared => Self::Prepared,
            WorkflowPhase::Signed => Self::Signed,
            WorkflowPhase::SubmittedDelegation => Self::SubmittedDelegation,
            WorkflowPhase::SubmittedVote => Self::SubmittedVote,
            WorkflowPhase::SubmittedShare => Self::SubmittedShare,
            WorkflowPhase::SubmissionManaged => Self::SubmissionManaged,
            WorkflowPhase::SubmissionRejected => Self::SubmissionRejected,
            WorkflowPhase::Confirmed => Self::Confirmed,
        }
    }
}

impl WorkflowPhase {
    /// Converts a canonical delegation phase into the merged workflow phase.
    pub fn for_delegation(phase: DelegationPhase) -> Self {
        match phase {
            DelegationPhase::Prepared => Self::Prepared,
            DelegationPhase::PcztBuilt | DelegationPhase::Proved => Self::Signed,
            DelegationPhase::Submitted => Self::SubmittedDelegation,
            DelegationPhase::SubmissionManaged => Self::SubmissionManaged,
            DelegationPhase::SubmittedWithoutHash => Self::SubmittedDelegation,
            DelegationPhase::SubmissionRejected => Self::SubmissionRejected,
            DelegationPhase::Confirmed => Self::Confirmed,
        }
    }

    /// Converts a canonical vote phase into the merged workflow phase.
    pub fn for_vote(phase: VotePhase) -> Self {
        match phase {
            VotePhase::Prepared => Self::Prepared,
            VotePhase::Committed => Self::Signed,
            VotePhase::Submitted => Self::SubmittedVote,
            VotePhase::SubmissionManaged => Self::SubmissionManaged,
            VotePhase::SubmittedWithoutHash => Self::SubmittedVote,
            VotePhase::SubmissionRejected => Self::SubmissionRejected,
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
        let wallet_id = self.wallet_id();
        self.delegation_phase_for_wallet(&wallet_id, round_id, bundle_index)
    }

    /// Loads one delegation phase under an immutable wallet scope captured by
    /// a longer-running operation.
    pub(crate) fn delegation_phase_for_wallet(
        &self,
        wallet_id: &str,
        round_id: &str,
        bundle_index: u32,
    ) -> Result<DelegationPhase, VotingError> {
        let conn = self.conn();
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
                        b.van_leaf_position IS NOT NULL,
                        (SELECT s.state FROM chain_submissions s
                         WHERE s.round_id=b.round_id AND s.wallet_id=b.wallet_id
                           AND s.bundle_index=b.bundle_index AND s.kind='delegation')
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
                        row.get::<_, Option<String>>(4)?.as_deref(),
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
        Ok(self
            .delegation_submission_statuses(round_id)?
            .into_iter()
            .map(|status| (status.bundle_index, status.phase))
            .collect())
    }

    /// Lists delegation phases with the stored lifecycle diagnostic of each
    /// bundle's authoritative submission row.
    ///
    /// Results are sorted by `bundle_index` and scoped to the current wallet id.
    pub(crate) fn delegation_submission_statuses(
        &self,
        round_id: &str,
    ) -> Result<Vec<DelegationSubmissionStatus>, VotingError> {
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
                        b.van_leaf_position IS NOT NULL,
                        s.state, s.diagnostic_kind, s.diagnostic
                 FROM bundles b
                 LEFT JOIN chain_submissions s
                   ON s.round_id = b.round_id
                  AND s.wallet_id = b.wallet_id
                  AND s.bundle_index = b.bundle_index
                  AND s.kind = 'delegation'
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
                            row.get::<_, Option<String>>(5)?.as_deref(),
                        ),
                        row.get::<_, Option<String>>(6)?,
                        row.get::<_, Option<String>>(7)?,
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

        rows.into_iter()
            .map(|(bundle_index, phase, diagnostic_kind, diagnostic)| {
                Ok(DelegationSubmissionStatus {
                    bundle_index,
                    phase,
                    diagnostic: stored_submission_diagnostic(diagnostic_kind, diagnostic)?,
                })
            })
            .collect()
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
        let phase = load_vote_submission_statuses(
            &conn,
            &wallet_id,
            round_id,
            Some(bundle_index),
            Some(proposal_id),
        )?
        .into_iter()
        .next()
        .map(|status| status.phase);

        phase.ok_or_else(|| VotingError::InvalidInput {
            message: format!(
                "vote not found for round {round_id} bundle {bundle_index} proposal {proposal_id}"
            ),
        })
    }

    /// Lists canonical vote phases for all votes in one round.
    pub fn vote_phases(&self, round_id: &str) -> Result<Vec<(u32, u32, VotePhase)>, VotingError> {
        Ok(self
            .vote_submission_statuses(round_id)?
            .into_iter()
            .map(|status| (status.bundle_index, status.proposal_id, status.phase))
            .collect())
    }

    /// Lists vote phases with the stored lifecycle diagnostic of each vote's
    /// authoritative singleton or batch submission row.
    pub(crate) fn vote_submission_statuses(
        &self,
        round_id: &str,
    ) -> Result<Vec<VoteSubmissionStatus>, VotingError> {
        let conn = self.conn();
        let wallet_id = self.wallet_id();
        load_vote_submission_statuses(&conn, &wallet_id, round_id, None, None)
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

struct VotePhaseEvidence {
    bundle_index: u32,
    proposal_id: u32,
    has_tx_hash: bool,
    has_vc_position: bool,
    has_recovery_bundle: bool,
    singleton_submission_state: Option<String>,
    singleton_diagnostic_kind: Option<String>,
    singleton_diagnostic: Option<String>,
}

/// Projects vote rows through their authoritative singleton or atomic-batch
/// submission. Batch membership is accepted only after the persisted signed
/// batch and its complete generation digest have been re-derived.
fn load_vote_submission_statuses(
    conn: &Connection,
    wallet_id: &str,
    round_id: &str,
    bundle_index: Option<u32>,
    proposal_id: Option<u32>,
) -> Result<Vec<VoteSubmissionStatus>, VotingError> {
    let vote_evidence = {
        let mut statement = conn
            .prepare(
                "SELECT v.bundle_index, v.proposal_id, v.tx_hash IS NOT NULL,
                        v.vc_tree_position IS NOT NULL,
                        v.commitment_bundle_json IS NOT NULL,
                        singleton.state, singleton.diagnostic_kind, singleton.diagnostic
                   FROM votes v
                   LEFT JOIN chain_submissions singleton
                     ON singleton.round_id=v.round_id
                    AND singleton.wallet_id=v.wallet_id
                    AND singleton.bundle_index=v.bundle_index
                    AND singleton.kind='vote'
                    AND singleton.proposal_id=v.proposal_id
                  WHERE v.round_id=:round_id AND v.wallet_id=:wallet_id
                    AND (:bundle_index IS NULL OR v.bundle_index=:bundle_index)
                    AND (:proposal_id IS NULL OR v.proposal_id=:proposal_id)
                  ORDER BY v.bundle_index, v.proposal_id",
            )
            .map_err(|error| VotingError::Internal {
                message: format!("failed to prepare vote phases query: {error}"),
            })?;
        let evidence = statement
            .query_map(
                named_params! {
                    ":round_id": round_id,
                    ":wallet_id": wallet_id,
                    ":bundle_index": bundle_index.map(i64::from),
                    ":proposal_id": proposal_id.map(i64::from),
                },
                |row| {
                    Ok(VotePhaseEvidence {
                        bundle_index: row.get::<_, i64>(0)? as u32,
                        proposal_id: row.get::<_, i64>(1)? as u32,
                        has_tx_hash: row.get::<_, i64>(2)? != 0,
                        has_vc_position: row.get::<_, i64>(3)? != 0,
                        has_recovery_bundle: row.get::<_, i64>(4)? != 0,
                        singleton_submission_state: row.get(5)?,
                        singleton_diagnostic_kind: row.get(6)?,
                        singleton_diagnostic: row.get(7)?,
                    })
                },
            )
            .map_err(|error| VotingError::Internal {
                message: format!("failed to query vote phases: {error}"),
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| VotingError::Internal {
                message: format!("failed to read vote phase row: {error}"),
            })?;
        evidence
    };

    let batch_members = load_authoritative_batch_phases(conn, wallet_id, round_id, bundle_index)?;
    vote_evidence
        .into_iter()
        .map(|evidence| {
            let singleton_phase =
                authoritative_submission_phase(evidence.singleton_submission_state.as_deref());
            let batch_member = batch_members.get(&(evidence.bundle_index, evidence.proposal_id));
            if singleton_phase.is_some() && batch_member.is_some() {
                return Err(VotingError::Internal {
                    message: format!(
                        "vote has overlapping authoritative singleton and batch submissions for round={round_id}, bundle={}, proposal={}",
                        evidence.bundle_index, evidence.proposal_id
                    ),
                });
            }
            let (phase, diagnostic, ordered_batch_digest) = match (singleton_phase, batch_member)
            {
                (Some(phase), _) => (
                    phase,
                    stored_submission_diagnostic(
                        evidence.singleton_diagnostic_kind,
                        evidence.singleton_diagnostic,
                    )?,
                    None,
                ),
                (None, Some(member)) => (
                    member.phase,
                    member.diagnostic.clone(),
                    Some(member.ordered_batch_digest),
                ),
                (None, None) => (
                    vote_phase_from_columns(
                        evidence.has_tx_hash,
                        evidence.has_vc_position,
                        evidence.has_recovery_bundle,
                    ),
                    None,
                    None,
                ),
            };
            Ok(VoteSubmissionStatus {
                bundle_index: evidence.bundle_index,
                proposal_id: evidence.proposal_id,
                phase,
                diagnostic,
                ordered_batch_digest,
            })
        })
        .collect()
}

fn authoritative_submission_phase(state: Option<&str>) -> Option<VotePhase> {
    match state {
        Some("submitting" | "tracking" | "recovering") => Some(VotePhase::SubmissionManaged),
        Some("submitted_without_hash") => Some(VotePhase::SubmittedWithoutHash),
        Some("rejected") => Some(VotePhase::SubmissionRejected),
        Some("confirmed") => Some(VotePhase::Confirmed),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    /// Serde label of the wire view, so the tests keep asserting the stable
    /// strings hosts saw before the view became an enum.
    fn label(phase: super::WorkflowPhase) -> String {
        serde_json::to_value(crate::wire::WorkflowPhaseView::from(phase))
            .unwrap()
            .as_str()
            .unwrap()
            .to_string()
    }
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
    fn every_post_proof_delegation_phase_reports_a_persisted_proof() {
        // The lifecycle states override the artifact columns in
        // `delegation_phase`, so a bundle whose proof is durably stored is
        // reported as one of these rather than as `Proved`. Missing one makes
        // callers re-prove and re-enter PIR for a proof they already hold.
        for phase in [
            DelegationPhase::Proved,
            DelegationPhase::Submitted,
            DelegationPhase::SubmissionManaged,
            DelegationPhase::SubmittedWithoutHash,
            DelegationPhase::SubmissionRejected,
            DelegationPhase::Confirmed,
        ] {
            assert!(phase.has_persisted_proof(), "{phase:?}");
        }
        for phase in [DelegationPhase::Prepared, DelegationPhase::PcztBuilt] {
            assert!(!phase.has_persisted_proof(), "{phase:?}");
        }
    }

    #[test]
    fn a_lifecycle_owned_bundle_still_reports_its_persisted_proof() {
        // The regression this guards: a `chain_submissions` state hides the
        // `Proved` projection, so a caller that only accepted
        // `Proved | Submitted | Confirmed` re-proved a bundle it had already
        // proved — and re-entered PIR to do it.
        let db = db_with_bundle();
        crate::storage::queries::store_proof(&db.conn(), ROUND_ID, WALLET_ID, 0, &[0xAC; 96])
            .unwrap();
        assert_eq!(
            db.delegation_phase(ROUND_ID, 0).unwrap(),
            DelegationPhase::Proved
        );

        for (state, expected) in [
            ("submitting", DelegationPhase::SubmissionManaged),
            ("tracking", DelegationPhase::SubmissionManaged),
            ("recovering", DelegationPhase::SubmissionManaged),
            (
                "submitted_without_hash",
                DelegationPhase::SubmittedWithoutHash,
            ),
            ("rejected", DelegationPhase::SubmissionRejected),
        ] {
            set_delegation_submission_state(&db, state);
            let phase = db.delegation_phase(ROUND_ID, 0).unwrap();
            assert_eq!(phase, expected);
            assert!(phase.has_persisted_proof(), "{state}");
        }
    }

    /// Replaces the bundle's authoritative delegation submission row with one
    /// in `state`, so the lifecycle projection wins over the artifact columns.
    fn set_delegation_submission_state(db: &VotingDb, state: &str) {
        let conn = db.conn();
        conn.execute(
            "DELETE FROM chain_submissions
             WHERE round_id = :round_id AND wallet_id = :wallet_id
               AND bundle_index = 0 AND kind = 'delegation'",
            named_params! { ":round_id": ROUND_ID, ":wallet_id": WALLET_ID },
        )
        .unwrap();
        // The schema pins each state's evidence: `tracking` needs a candidate
        // hash, and the terminal states need a stored diagnostic.
        let (candidate_hash, tracking_started_at) = match state {
            "tracking" => (Some(&[0x22u8; 32][..]), Some(9i64)),
            _ => (None, None),
        };
        let diagnostic = match state {
            "submitted_without_hash" => Some(("ambiguous_dispatch", "dispatch outcome unknown")),
            "rejected" => Some(("chain_rejected", "vote chain rejected the generation")),
            _ => None,
        };
        conn.execute(
            "INSERT INTO chain_submissions
                (identity_key, round_id, wallet_id, network, bundle_index, kind,
                 proposal_id, ordered_batch_digest, generation_digest, state,
                 candidate_transaction_hash, tracking_started_at,
                 diagnostic_kind, diagnostic,
                 committed_post_reservations, created_at, updated_at)
             VALUES (:identity_key, :round_id, :wallet_id, 'testnet', 0, 'delegation',
                     NULL, NULL, :generation_digest, :state,
                     :candidate_hash, :tracking_started_at,
                     :diagnostic_kind, :diagnostic, 1, 9, 9)",
            named_params! {
                ":identity_key": format!("{ROUND_ID}-delegation-{state}"),
                ":round_id": ROUND_ID,
                ":wallet_id": WALLET_ID,
                ":generation_digest": &[0x11u8; 32][..],
                ":state": state,
                ":candidate_hash": candidate_hash,
                ":tracking_started_at": tracking_started_at,
                ":diagnostic_kind": diagnostic.map(|(kind, _)| kind),
                ":diagnostic": diagnostic.map(|(_, message)| message),
            },
        )
        .unwrap();
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
            label(WorkflowPhase::for_delegation(DelegationPhase::Prepared)),
            "prepared"
        );
        assert_eq!(
            label(WorkflowPhase::for_delegation(DelegationPhase::PcztBuilt)),
            "signed"
        );
        assert_eq!(
            label(WorkflowPhase::for_delegation(DelegationPhase::Proved)),
            "signed"
        );
        assert_eq!(
            label(WorkflowPhase::for_delegation(
                DelegationPhase::SubmissionManaged
            )),
            "submission_managed"
        );
        assert_eq!(
            label(WorkflowPhase::for_delegation(
                DelegationPhase::SubmissionRejected
            )),
            "submission_rejected"
        );
        assert_eq!(
            label(WorkflowPhase::for_delegation(DelegationPhase::Submitted)),
            "submitted_delegation"
        );
        assert_eq!(
            label(WorkflowPhase::for_delegation(DelegationPhase::Confirmed)),
            "confirmed"
        );

        assert_eq!(
            label(WorkflowPhase::for_vote(VotePhase::Prepared)),
            "prepared"
        );
        assert_eq!(
            label(WorkflowPhase::for_vote(VotePhase::Committed)),
            "signed"
        );
        assert_eq!(
            label(WorkflowPhase::for_vote(VotePhase::Submitted)),
            "submitted_vote"
        );
        assert_eq!(
            label(WorkflowPhase::for_vote(VotePhase::SubmissionManaged)),
            "submission_managed"
        );
        assert_eq!(
            label(WorkflowPhase::for_vote(VotePhase::SubmissionRejected)),
            "submission_rejected"
        );
        assert_eq!(
            label(WorkflowPhase::for_vote(VotePhase::Confirmed)),
            "confirmed"
        );

        assert_eq!(
            label(WorkflowPhase::for_share(SharePhase::Submitted)),
            "submitted_share"
        );
        assert_eq!(
            label(WorkflowPhase::for_share(SharePhase::Confirmed)),
            "confirmed"
        );
    }
}

fn phase_from_columns(
    has_pczt: bool,
    has_proof: bool,
    has_tx_hash: bool,
    has_van_position: bool,
    authoritative_state: Option<&str>,
) -> DelegationPhase {
    match authoritative_state {
        Some("submitting" | "tracking" | "recovering") => DelegationPhase::SubmissionManaged,
        Some("submitted_without_hash") => DelegationPhase::SubmittedWithoutHash,
        Some("rejected") => DelegationPhase::SubmissionRejected,
        Some("confirmed") => DelegationPhase::Confirmed,
        _ if has_van_position => DelegationPhase::Confirmed,
        _ if has_tx_hash => DelegationPhase::Submitted,
        _ if has_proof => DelegationPhase::Proved,
        _ if has_pczt => DelegationPhase::PcztBuilt,
        _ => DelegationPhase::Prepared,
    }
}

/// Projects one vote's workflow phase from its version-17 domain columns.
///
/// This is the fallback for a vote with no authoritative `chain_submissions`
/// row; whenever such a row exists it wins, whether unresolved or confirmed by
/// hash or tree. Pre-upgrade rounds keep displaying through this projection.
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
