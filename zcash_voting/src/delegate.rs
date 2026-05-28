//! Delegation lifecycle API.
//!
//! The functions in this module are the stable wallet-facing delegation flow:
//! build a governance PCZT, precompute proof inputs, prove delegation, assemble
//! chain submission data, and record chain recovery data.

pub use crate::phases::DelegationPhase;

use crate::{
    round::VotingDb,
    types::{Network, NoteInfo, ProgressReporter, VotingError},
};

/// Wallet-derived keys and chain parameters needed to build a delegation PCZT.
#[derive(Clone, Debug)]
pub struct DelegationKeys {
    pub fvk_bytes: Vec<u8>,
    pub hotkey_raw_address: [u8; 43],
    pub seed_fingerprint: [u8; 32],
    pub account_index: u32,
    pub address_index: u32,
    pub consensus_branch_id: u32,
    pub coin_type: u32,
    pub round_name: String,
}

/// PCZT setup output that callers hand to a signer or QR encoder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DelegationSetup {
    pub pczt_bytes: Vec<u8>,
    pub pczt_sighash: [u8; 32],
    pub rk: [u8; 32],
    pub action_index: usize,
    pub action_bytes: Vec<u8>,
}

/// Generated delegation proof and public submission fields for one bundle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DelegationProof {
    pub bytes: Vec<u8>,
    pub rk: [u8; 32],
    pub nf_signed: [u8; 32],
    pub cmx_new: [u8; 32],
    pub van_comm: [u8; 32],
    pub gov_nullifiers: [[u8; 32]; 5],
}

/// Signature source used when assembling a delegation transaction payload.
pub enum DelegationSigner<'a> {
    Seed {
        seed: &'a [u8],
        network: Network,
        account_index: u32,
    },
    Keystone {
        sig: [u8; 64],
        sighash: [u8; 32],
    },
}

/// Chain-ready delegation transaction fields for one bundle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DelegationSubmission {
    pub proof: Vec<u8>,
    pub rk: [u8; 32],
    pub nf_signed: [u8; 32],
    pub cmx_new: [u8; 32],
    pub gov_comm: [u8; 32],
    pub gov_nullifiers: [[u8; 32]; 5],
    pub alpha: [u8; 32],
    pub vote_round_id: String,
    pub spend_auth_sig: [u8; 64],
    pub sighash: [u8; 32],
}

/// Builds and persists a governance PCZT for one bundle.
///
/// The bundle must already exist via [`VotingDb::ensure_bundles`]. The returned
/// sighash is the exact message that Keystone or the seed signer must sign.
pub fn setup(
    db: &VotingDb,
    round_id: &str,
    bundle_index: u32,
    notes: &[NoteInfo],
    keys: &DelegationKeys,
) -> Result<DelegationSetup, VotingError> {
    let pczt = db.build_governance_pczt(
        round_id,
        bundle_index,
        notes,
        &keys.fvk_bytes,
        &keys.hotkey_raw_address,
        keys.consensus_branch_id,
        keys.coin_type,
        &keys.seed_fingerprint,
        keys.account_index,
        &keys.round_name,
        keys.address_index,
    )?;

    Ok(DelegationSetup {
        pczt_bytes: pczt.pczt_bytes,
        pczt_sighash: array32("pczt_sighash", pczt.pczt_sighash)?,
        rk: array32("rk", pczt.rk)?,
        action_index: pczt.action_index,
        action_bytes: pczt.action_bytes,
    })
}

/// Generates and persists the delegation proof for one bundle.
///
/// Witnesses and PIR proof precompute data must already be present. The proof
/// result is checked against PCZT-derived public fields before persistence.
#[cfg(feature = "pir")]
pub fn prove(
    db: &VotingDb,
    round_id: &str,
    bundle_index: u32,
    notes: &[NoteInfo],
    hotkey_raw_address: &[u8; 43],
    pir_client: &pir_client::PirClientBlocking,
    network: Network,
    progress: &dyn ProgressReporter,
) -> Result<DelegationProof, VotingError> {
    let proof = db.build_and_prove_delegation(
        round_id,
        bundle_index,
        notes,
        hotkey_raw_address,
        pir_client,
        network.id(),
        progress,
    )?;

    Ok(DelegationProof {
        bytes: proof.proof,
        rk: array32("rk", proof.rk)?,
        nf_signed: array32("nf_signed", proof.nf_signed)?,
        cmx_new: array32("cmx_new", proof.cmx_new)?,
        van_comm: array32("van_comm", proof.van_comm)?,
        gov_nullifiers: array32x5("gov_nullifiers", proof.gov_nullifiers)?,
    })
}

/// Assembles chain-ready delegation submission fields for one bundle.
///
/// Seed signers derive the spend authorization key internally. Keystone signers
/// must provide the signature over the stored PCZT sighash.
pub fn submission(
    db: &VotingDb,
    round_id: &str,
    bundle_index: u32,
    signer: DelegationSigner<'_>,
) -> Result<DelegationSubmission, VotingError> {
    let data = match signer {
        DelegationSigner::Seed {
            seed,
            network,
            account_index,
        } => {
            db.get_delegation_submission(round_id, bundle_index, seed, network.id(), account_index)
        }
        DelegationSigner::Keystone { sig, sighash } => {
            db.get_delegation_submission_with_keystone_sig(round_id, bundle_index, &sig, &sighash)
        }
    }?;

    Ok(DelegationSubmission {
        proof: data.proof,
        rk: array32("rk", data.rk)?,
        nf_signed: array32("nf_signed", data.nf_signed)?,
        cmx_new: array32("cmx_new", data.cmx_new)?,
        gov_comm: array32("gov_comm", data.gov_comm)?,
        gov_nullifiers: array32x5("gov_nullifiers", data.gov_nullifiers)?,
        alpha: array32("alpha", data.alpha)?,
        vote_round_id: data.vote_round_id,
        spend_auth_sig: array64("spend_auth_sig", data.spend_auth_sig)?,
        sighash: array32("sighash", data.sighash)?,
    })
}

/// Records the submitted delegation transaction hash for recovery.
pub fn record_submission(
    db: &VotingDb,
    round_id: &str,
    bundle_index: u32,
    tx_hash: &str,
) -> Result<(), VotingError> {
    db.store_delegation_tx_hash(round_id, bundle_index, tx_hash)
}

/// Records the confirmed VAN leaf position for a delegated bundle.
pub fn record_van_position(
    db: &VotingDb,
    round_id: &str,
    bundle_index: u32,
    position: u32,
) -> Result<(), VotingError> {
    db.store_van_position(round_id, bundle_index, position)
}

/// Extracts the ZIP-244 shielded sighash from a serialized voting PCZT.
pub fn pczt_sighash(pczt_bytes: &[u8]) -> Result<[u8; 32], VotingError> {
    crate::action::extract_pczt_sighash(pczt_bytes)
}

/// Extracts one SpendAuth signature from a Keystone-signed voting PCZT.
pub fn spend_auth_signature(
    signed_pczt_bytes: &[u8],
    action_index: usize,
) -> Result<[u8; 64], VotingError> {
    crate::action::extract_spend_auth_sig(signed_pczt_bytes, action_index)
}

fn array32(label: &str, value: Vec<u8>) -> Result<[u8; 32], VotingError> {
    value
        .try_into()
        .map_err(|value: Vec<u8>| VotingError::Internal {
            message: format!("{label} must be 32 bytes, got {}", value.len()),
        })
}

fn array64(label: &str, value: Vec<u8>) -> Result<[u8; 64], VotingError> {
    value
        .try_into()
        .map_err(|value: Vec<u8>| VotingError::Internal {
            message: format!("{label} must be 64 bytes, got {}", value.len()),
        })
}

fn array32x5(label: &str, values: Vec<Vec<u8>>) -> Result<[[u8; 32]; 5], VotingError> {
    if values.len() != 5 {
        return Err(VotingError::Internal {
            message: format!("{label} must contain 5 entries, got {}", values.len()),
        });
    }

    let arrays = values
        .into_iter()
        .enumerate()
        .map(|(idx, value)| array32(&format!("{label}[{idx}]"), value))
        .collect::<Result<Vec<_>, _>>()?;

    arrays
        .try_into()
        .map_err(|arrays: Vec<[u8; 32]>| VotingError::Internal {
            message: format!("{label} must contain 5 entries, got {}", arrays.len()),
        })
}
