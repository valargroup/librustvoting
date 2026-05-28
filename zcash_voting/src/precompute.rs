//! Precomputation APIs for delegation inputs.
//!
//! Precompute operations prepare data that is expensive to derive during proof
//! generation: Orchard note witnesses from the wallet database and PIR-backed
//! non-membership proofs for nullifiers.

use std::borrow::Borrow;

use zcash_client_sqlite::WalletDb;

use crate::{
    round::VotingDb,
    types::{NoteInfo, VotingError, WitnessData},
};

/// Result of PIR precomputation for one delegation bundle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PirPrecomputeReport {
    pub cached: u32,
    pub fetched: u32,
}

/// Stores `tree_state_bytes`, generates Orchard witnesses, and caches them.
///
/// The tree state must be the exact snapshot anchor for the round. The wallet
/// database supplies historical note paths; voting state is persisted in
/// `db`.
pub fn note_witnesses<C, P, CL, R>(
    db: &VotingDb,
    round_id: &str,
    bundle_index: u32,
    tree_state_bytes: &[u8],
    notes: &[NoteInfo],
    wallet_db: &WalletDb<C, P, CL, R>,
) -> Result<Vec<WitnessData>, VotingError>
where
    C: Borrow<rusqlite::Connection>,
    P: zcash_protocol::consensus::Parameters,
{
    crate::witness::store_tree_state_and_generate_note_witnesses(
        db,
        round_id,
        bundle_index,
        tree_state_bytes,
        notes,
        wallet_db,
    )
}

/// Loads a round's cached tree state, generates Orchard witnesses, and caches them.
///
/// This is the FFI-friendly variant for callers that already persisted the
/// round tree state through [`VotingDb`] and should not reach into storage
/// query helpers.
pub fn stored_note_witnesses<C, P, CL, R>(
    db: &VotingDb,
    round_id: &str,
    bundle_index: u32,
    notes: &[NoteInfo],
    wallet_db: &WalletDb<C, P, CL, R>,
) -> Result<Vec<WitnessData>, VotingError>
where
    C: Borrow<rusqlite::Connection>,
    P: zcash_protocol::consensus::Parameters,
{
    let tree_state_bytes = {
        let conn = db.conn();
        let wallet_id = db.wallet_id();
        crate::storage::queries::load_tree_state(&conn, round_id, &wallet_id)?
    };
    note_witnesses(
        db,
        round_id,
        bundle_index,
        &tree_state_bytes,
        notes,
        wallet_db,
    )
}

/// Verifies an Orchard note witness against its stored root.
///
/// Returns `Ok(())` when the witness recomputes to the expected root and
/// [`VotingError::InvalidInput`] when the bytes are malformed or mismatched.
pub fn verify_witness(witness: &WitnessData) -> Result<(), VotingError> {
    if crate::witness::verify_witness(witness)? {
        Ok(())
    } else {
        Err(VotingError::InvalidInput {
            message: format!(
                "witness root mismatch at note position {}",
                witness.position
            ),
        })
    }
}

/// Fetches and persists PIR-backed IMT non-membership proofs for one bundle.
///
/// This must run after delegation setup, because padded-note secrets are
/// produced by the PCZT construction step.
#[cfg(feature = "pir")]
pub fn delegation_pir(
    db: &VotingDb,
    round_id: &str,
    bundle_index: u32,
    notes: &[NoteInfo],
    pir_client: &pir_client::PirClientBlocking,
    network: crate::types::Network,
) -> Result<PirPrecomputeReport, VotingError> {
    let result =
        db.precompute_delegation_pir(round_id, bundle_index, notes, pir_client, network.id())?;
    Ok(PirPrecomputeReport {
        cached: result.cached_count,
        fetched: result.fetched_count,
    })
}
