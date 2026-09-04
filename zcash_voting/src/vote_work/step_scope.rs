//! Identity and diagnostics shared by every round step: the durable vote key
//! a step reports progress against, the canonical round id it executes
//! under, and the bounded message shape every failure carries.

use crate::{vote::CommittedVote, VotingError, MAX_CHAIN_SUBMISSION_DIAGNOSTIC_BYTES};

use super::VoteRecoveryKey;

/// The durable identity of `vote`, as progress and delivery reports name it.
pub(super) fn vote_key(vote: &CommittedVote) -> VoteRecoveryKey {
    VoteRecoveryKey {
        bundle_index: vote.bundle_index(),
        proposal_id: vote.proposal_id(),
    }
}

/// Decodes a canonical lowercase-hex round id into its 32 bytes, refusing
/// any other spelling with [`VotingError::InvalidInput`].
pub(super) fn parse_round_id(round_id: &str) -> Result<[u8; 32], VotingError> {
    crate::types::validate_vote_round_id_hex(round_id)?;
    let bytes = hex::decode(round_id).map_err(|error| VotingError::InvalidInput {
        message: format!("vote_round_id is not valid hex: {error}"),
    })?;
    bytes
        .try_into()
        .map_err(|bytes: Vec<u8>| VotingError::InvalidInput {
            message: format!("vote_round_id must be 32 bytes, got {}", bytes.len()),
        })
}

/// Escapes control characters and truncates `message` to the chain
/// submission diagnostic budget, so a failure message never smuggles a
/// newline or an unbounded response body into host logs.
pub(super) fn bounded_message(message: &str) -> String {
    let mut bounded =
        String::with_capacity(message.len().min(MAX_CHAIN_SUBMISSION_DIAGNOSTIC_BYTES));
    for character in message.chars() {
        let escaped = character.escape_default().collect::<String>();
        if bounded.len() + escaped.len() > MAX_CHAIN_SUBMISSION_DIAGNOSTIC_BYTES {
            break;
        }
        bounded.push_str(&escaped);
    }
    bounded
}
