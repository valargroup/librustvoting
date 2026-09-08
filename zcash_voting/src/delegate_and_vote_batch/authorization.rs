//! Consensus encoding of the combined cast authorization.

use crate::{vote_commitment::CastVoteBatchSighashAction, VotingError};

const DOMAIN: &[u8] = b"SVOTE_DELEGATE_AND_CAST_VOTE_BATCH_SIGHASH_V1";

/// Binds the initial delegation VAN and every ordered cast effect.
///
/// Matches vote-sdk's `ComputeDelegateAndCastVoteBatchSighash`. Integers are
/// little-endian u32 values padded to 32 bytes. Proofs and signatures have
/// separate verification and do not enter this authorization digest.
pub(crate) fn delegate_and_vote_batch_sighash(
    round_id: &[u8; 32],
    delegation_van: &[u8; 32],
    actions: &[CastVoteBatchSighashAction<'_>],
) -> Result<[u8; 32], VotingError> {
    if actions.is_empty() || actions.len() > crate::vote::MAX_VOTE_BATCH_ACTIONS {
        return Err(invalid(
            "combined batch action count is outside the protocol bounds",
        ));
    }
    let mut transcript = Vec::with_capacity(DOMAIN.len() + 32 * (3 + 6 * actions.len()));
    transcript.extend_from_slice(DOMAIN);
    transcript.extend_from_slice(round_id);
    transcript.extend_from_slice(delegation_van);
    append_integer(&mut transcript, actions.len() as u32);
    let mut proposals = std::collections::BTreeSet::new();
    for (index, action) in actions.iter().enumerate() {
        crate::types::validate_proposal_id(action.proposal_id)?;
        if !proposals.insert(action.proposal_id) {
            return Err(invalid("combined batch contains duplicate proposals"));
        }
        append_integer(&mut transcript, index as u32);
        for field in [
            action.r_vpk,
            action.van_nullifier,
            action.vote_authority_note_new,
            action.vote_commitment,
        ] {
            if field.len() != 32 {
                return Err(invalid(
                    "combined cast effect must contain exactly 32 bytes",
                ));
            }
            transcript.extend_from_slice(field);
        }
        append_integer(&mut transcript, action.proposal_id);
    }
    let hash = blake2b_simd::Params::new()
        .hash_length(32)
        .hash(&transcript);
    let mut digest = [0; 32];
    digest.copy_from_slice(hash.as_bytes());
    Ok(digest)
}

fn append_integer(transcript: &mut Vec<u8>, integer: u32) {
    let mut encoded = [0; 32];
    encoded[..4].copy_from_slice(&integer.to_le_bytes());
    transcript.extend_from_slice(&encoded);
}

fn invalid(message: &str) -> VotingError {
    VotingError::InvalidInput {
        message: message.to_owned(),
    }
}
