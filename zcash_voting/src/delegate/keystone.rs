//! Reuse the exact durable transaction at the external signing boundary.
use super::*;

pub(super) fn request(
    prepared: &PreparedDelegationBundle,
    voting_db: &VotingDb,
    stages: &dyn DelegationProgressReporter,
) -> Result<KeystoneSigningRequest, VotingError> {
    let scoped_db = voting_db.scoped(&voting_db.wallet_id())?;
    let voting_db = &scoped_db;
    prepared.validate_snapshot_branch_id_provider()?;
    // A fresh bundle may race its background prover to persist setup. The
    // winner's complete PCZT is write-once; both callers reload it below.
    if voting_db.delegation_phase(&prepared.round_id, prepared.bundle_index)?
        == DelegationPhase::Prepared
    {
        match prepared.setup(voting_db, stages) {
            Ok(_) | Err(VotingError::SetupAlreadyPersisted { .. }) => {}
            Err(error) => return Err(error),
        }
    }
    // Check the exact notes and target before returning any persisted request.
    prepared.validate_persisted_proof(voting_db)?;
    let (pczt_bytes, stored_sighash, stored_rk) =
        voting_db.get_delegation_pczt_fields(&prepared.round_id, prepared.bundle_index)?;
    let persisted_sighash = array32("pczt_sighash", stored_sighash)?;
    let recomputed_sighash = pczt_sighash(&pczt_bytes)?;
    if recomputed_sighash != persisted_sighash {
        return Err(VotingError::Internal {
            message: "persisted delegation PCZT sighash does not match stored setup".to_string(),
        });
    }
    let rk = array32("rk", stored_rk)?;
    let action_index = crate::action::delegation_pczt_action_index(&pczt_bytes, &rk)?;
    let redacted_pczt_bytes = redact_delegation_pczt_for_signer(&pczt_bytes)?;
    let display_weight_zatoshi = crate::round::raw_bundle_weight(&prepared.bundle_note_infos)?;
    let display_memo = display_memo(&prepared.round_name, display_weight_zatoshi);
    let action_index =
        crate::wire::BoundedU32::try_from(action_index).map_err(|_| VotingError::InvalidInput {
            message: format!("action_index {action_index} does not fit u32"),
        })?;

    Ok(KeystoneSigningRequest {
        pczt_bytes,
        redacted_pczt_bytes,
        pczt_sighash: persisted_sighash.to_vec(),
        rk: rk.to_vec(),
        action_index: action_index.0,
        display_memo,
        eligible_weight_zatoshi: prepared.eligible_weight_zatoshi(),
        delegated_weight_zatoshi: prepared.delegated_weight_zatoshi()?,
        bundle_count: prepared.layout.bundle_count,
        bundle_index: prepared.bundle_index,
    })
}
