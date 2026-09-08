//! Load the validated signing context through a single read snapshot.

use super::*;

/// The transaction pins the validation inputs and returned PCZT to one setup,
/// even when another connection replaces the bundle during these reads.
pub(super) fn load(
    tx: &rusqlite::Transaction<'_>,
    identity: &DelegationProofIdentity,
    notes: &[NoteInfo],
    keys: &DelegationKeys,
) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>), VotingError> {
    let (params, network) =
        queries::load_round_params_with_network(tx, identity.round_id(), identity.wallet_id())?;
    validate_delegation_keys_for_round(&params, network, keys)?;
    queries::require_bundle_notes(
        tx,
        identity.round_id(),
        identity.wallet_id(),
        identity.bundle_index(),
        notes,
    )?;
    validate_delegation_target_for_bundle(tx, &params, network, identity, keys)?;
    queries::load_delegation_pczt_fields(
        tx,
        identity.round_id(),
        identity.wallet_id(),
        identity.bundle_index(),
    )
}
