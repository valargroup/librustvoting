//! Reuse the exact durable transaction at the external signing boundary.
use super::*;
use crate::action::{
    orchard::note::NoteVersion,
    pczt::{orchard::EncCiphertext, roles::redactor::Redactor, Pczt},
};

pub(super) fn request(
    prepared: &PreparedDelegationBundle,
    voting_db: &VotingDb,
    stages: &dyn DelegationProgressReporter,
    observations: &crate::ObservationScope,
) -> Result<KeystoneSigningRequest, VotingError> {
    let scoped_db = voting_db.scoped(&voting_db.wallet_id())?;
    let voting_db = &scoped_db;
    prepared.validate_snapshot_branch_id_provider()?;
    // The setup lease can return Busy during background setup. A later retry
    // reloads the winner's exact PCZT without waiting for proof generation.
    if voting_db.delegation_phase(&prepared.round_id, prepared.bundle_index)?
        == DelegationPhase::Prepared
    {
        match prepared.observe_setup(voting_db, stages, observations) {
            Ok(_) | Err(VotingError::SetupAlreadyPersisted { .. }) => {}
            Err(error) => return Err(error),
        }
    }
    // Validation and loading must observe the same setup if another connection
    // replaces an unbroadcast bundle for a different hotkey.
    let identity = DelegationProofIdentity::new(
        voting_db.sidecar_id(),
        voting_db.wallet_id(),
        &prepared.round_id,
        prepared.bundle_index,
    );
    let (pczt_bytes, stored_sighash, stored_rk) = voting_db.validated_delegation_pczt(
        &identity,
        &prepared.bundle_note_infos,
        &prepared.delegation_keys,
    )?;
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
    let display_memo = persisted_display_memo(&pczt_bytes, action_index)?;
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

/// Recover the signed memo without changing the persisted or signer-facing PCZT.
fn persisted_display_memo(pczt_bytes: &[u8], action_index: usize) -> Result<String, VotingError> {
    let pczt = Pczt::parse(pczt_bytes).map_err(|error| VotingError::Internal {
        message: format!("failed to parse persisted delegation PCZT: {error:?}"),
    })?;
    let pczt = Redactor::new(pczt)
        .redact_ironwood_with(|mut bundle| {
            bundle.redact_action(action_index, |mut action| {
                action.replace_enc_ciphertext_with_decrypted_memo_plaintext(NoteVersion::V3);
            });
        })
        .finish();
    let action =
        pczt.ironwood()
            .actions()
            .get(action_index)
            .ok_or_else(|| VotingError::Internal {
                message: "persisted delegation PCZT has no governance action".to_string(),
            })?;
    let EncCiphertext::MemoPlaintext(memo) = action.output().enc_ciphertext() else {
        return Err(VotingError::Internal {
            message: "persisted delegation PCZT memo cannot be recovered".to_string(),
        });
    };
    std::str::from_utf8(memo.as_stripped_bytes())
        .map(str::to_owned)
        .map_err(|_| VotingError::Internal {
            message: "persisted delegation PCZT memo is not UTF-8 text".to_string(),
        })
}
