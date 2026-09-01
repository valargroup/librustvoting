//! Canonical payload recovery and durable-generation validation.

use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};

use crate::{
    delegate::{self, DelegationSigner},
    types::VotingError,
    vote,
    wire::{DelegationSubmissionWire, VoteCommitmentBatchWire, VoteCommitmentWire},
};

/// Rebuilds one canonical submission payload from durable state.
///
/// The rebuild takes a connection rather than a [`crate::storage::VotingDb`] on
/// purpose: [`crate::storage::VotingDb::conn`] guards a single shared connection,
/// so a closure that could reach back through the database handle would deadlock
/// when called from inside the reservation transaction.
/// Rebuild closure for a delegation payload.
///
/// The spend-auth signature is not durable for a software signer, so it is
/// captured from the live call. Everything else is re-read from storage, which
/// is exactly the material a concurrent writer could have replaced.
pub(super) fn delegation_payload_rebuild(
    wallet_id: String,
    round_id: String,
    bundle_index: u32,
    spend_auth_sig: [u8; 64],
    sighash: [u8; 32],
) -> impl Fn(&rusqlite::Connection) -> Result<Vec<u8>, VotingError> + Send + Sync {
    move |conn| {
        let submission = delegate::submission_with_conn(
            conn,
            &wallet_id,
            &round_id,
            bundle_index,
            DelegationSigner::signature(spend_auth_sig, sighash),
        )?;
        Ok(DelegationSubmissionWire::try_from(&submission)?
            .to_json()?
            .into_bytes())
    }
}

pub(super) fn canonical_singleton_vote_payload(
    conn: &rusqlite::Connection,
    wallet_id: &str,
    round_id: &str,
    bundle_index: u32,
    proposal_id: u32,
) -> Result<Vec<u8>, VotingError> {
    let signed =
        vote::signed_commitment_with_conn(conn, wallet_id, round_id, bundle_index, proposal_id)?;
    Ok(VoteCommitmentWire::try_from(&signed)?
        .to_json()?
        .into_bytes())
}

pub(super) fn canonical_batch_payload(batch_json: &str) -> Result<Vec<u8>, VotingError> {
    let wire: VoteCommitmentBatchWire =
        serde_json::from_str(batch_json).map_err(|_| VotingError::Internal {
            message: "persisted atomic vote batch is not valid wire JSON".to_string(),
        })?;
    Ok(wire.to_json()?.into_bytes())
}

pub(super) type PayloadRebuild<'a> =
    &'a (dyn Fn(&rusqlite::Connection) -> Result<Vec<u8>, VotingError> + Send + Sync);

pub(super) fn stale_generation_error() -> VotingError {
    VotingError::InvalidInput {
        message: "durable recovery generation changed before chain dispatch; recover the current \
                  submission and retry"
            .to_string(),
    }
}

pub(super) fn decode_canonical_array<const N: usize>(
    encoded: &str,
    field: &str,
) -> Result<[u8; N], VotingError> {
    let bytes = BASE64_STANDARD
        .decode(encoded)
        .map_err(|error| VotingError::InvalidInput {
            message: format!("{field} is not valid standard Base64: {error}"),
        })?;
    if BASE64_STANDARD.encode(&bytes) != encoded {
        return Err(VotingError::InvalidInput {
            message: format!("{field} must use canonical padded standard Base64"),
        });
    }
    bytes
        .try_into()
        .map_err(|bytes: Vec<u8>| VotingError::InvalidInput {
            message: format!("{field} must be {N} bytes, got {}", bytes.len()),
        })
}
