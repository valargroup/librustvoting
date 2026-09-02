//! Atomic domain projection for a generation-validated confirmation.

use crate::types::VotingError;

use super::{
    generation::{DerivedChainSubmission, ExpectedTreeLayout},
    result::ValidatedChainSubmissionConfirmation,
    ChainSubmissionTarget,
};

/// Projects validated terminal evidence onto the rows locked by `derived`.
///
/// The caller must include this connection in its lifecycle transaction. The
/// function rejects confirmation positions that do not match the generation's
/// expected action layout and performs no network I/O. The caller must roll
/// back its transaction if this function returns an error.
pub(super) fn apply_confirmed_generation(
    conn: &rusqlite::Transaction<'_>,
    derived: &DerivedChainSubmission,
    confirmation: &ValidatedChainSubmissionConfirmation,
) -> Result<(), VotingError> {
    let generation = derived.generation();
    let identity = generation.identity();
    let round_id = hex::encode(identity.vote_round_id());
    let confirmation = confirmation.confirmation();
    let transaction_hash = confirmation.transaction_hash().map(|hash| hash.to_string());
    let positions = confirmation.vote_commitment_positions();

    match (identity.target(), derived.expected_layout()) {
        (ChainSubmissionTarget::Delegation, ExpectedTreeLayout::Delegation { .. })
            if positions.is_empty() =>
        {
            crate::confirmation::apply_delegation_confirmation_with_conn(
                conn,
                identity.wallet_id(),
                &round_id,
                identity.bundle_index(),
                transaction_hash.as_deref(),
                confirmation.final_van_position(),
            )
        }
        (ChainSubmissionTarget::Vote { proposal_id }, ExpectedTreeLayout::Vote { .. })
            if positions.len() == 1 =>
        {
            crate::confirmation::apply_vote_confirmation_with_conn(
                conn,
                identity.wallet_id(),
                &round_id,
                identity.bundle_index(),
                proposal_id,
                transaction_hash.as_deref(),
                confirmation.final_van_position(),
                positions[0],
            )
        }
        (
            ChainSubmissionTarget::VoteBatch {
                ordered_batch_digest,
            },
            ExpectedTreeLayout::VoteBatch {
                vote_commitments, ..
            },
        ) if positions.len() == vote_commitments.len()
            && derived.ordered_proposal_ids().len() == positions.len() =>
        {
            crate::confirmation::apply_vote_batch_confirmation_with_conn(
                conn,
                identity.wallet_id(),
                &round_id,
                identity.bundle_index(),
                ordered_batch_digest,
                transaction_hash.as_deref(),
                confirmation.final_van_position(),
                positions,
                Some(derived.ordered_proposal_ids()),
                None,
            )
        }
        _ => Err(VotingError::InvalidInput {
            message: "confirmed positions do not match the semantic generation layout".to_string(),
        }),
    }
}
