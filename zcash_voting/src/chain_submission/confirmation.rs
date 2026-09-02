//! Atomic domain projection for a generation-validated confirmation.

use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};

use crate::{confirmation::TxEvent, types::VotingError};

use super::{
    generation::{DerivedChainSubmission, ExpectedTreeLayout},
    result::ValidatedChainSubmissionConfirmation,
    ChainSubmissionTarget,
};

const DELEGATE_VOTE_EVENT: &str = "delegate_vote";
const CAST_VOTE_EVENT: &str = "cast_vote";
const CAST_VOTE_BATCH_EVENT: &str = "cast_vote_batch";
const LEAF_INDEX_ATTRIBUTE: &str = "leaf_index";
const BATCH_DIGEST_ATTRIBUTE: &str = "batch_digest";
const BATCH_SIZE_ATTRIBUTE: &str = "batch_size";
const FINAL_VAN_LEAF_INDEX_ATTRIBUTE: &str = "final_van_leaf_index";
const VC_LEAF_INDICES_ATTRIBUTE: &str = "vote_commitment_leaf_indices";
const PROPOSAL_IDS_ATTRIBUTE: &str = "proposal_ids";
const VAN_NULLIFIERS_ATTRIBUTE: &str = "van_nullifiers";
const ROUND_ID_ATTRIBUTES: [&str; 2] = ["vote_round_id", "round_id"];

/// Validates committed hash evidence against one locked generation.
///
/// Lifecycle parsing is intentionally separate from the legacy public
/// confirmation wire values: lifecycle positions use the complete SQLite
/// `u64` range and are narrowed nowhere.
pub(super) fn validate_hash_confirmation(
    derived: &DerivedChainSubmission,
    transaction_hash: super::CandidateTransactionHash,
    events: &[TxEvent],
) -> Result<ValidatedChainSubmissionConfirmation, VotingError> {
    let identity = derived.generation().identity();
    let round_id = hex::encode(identity.vote_round_id());
    match (identity.target(), derived.expected_layout()) {
        (ChainSubmissionTarget::Delegation, ExpectedTreeLayout::Delegation { .. }) => {
            let event = required_event_for_round(events, DELEGATE_VOTE_EVENT, &round_id)?;
            let final_van_position = parse_compat_u64(
                required_attribute(event, DELEGATE_VOTE_EVENT, LEAF_INDEX_ATTRIBUTE)?,
                "delegate_vote leaf_index",
            )?;
            ValidatedChainSubmissionConfirmation::from_hash(
                transaction_hash,
                final_van_position,
                vec![],
            )
            .map_err(confirmation_error)
        }
        (ChainSubmissionTarget::Vote { .. }, ExpectedTreeLayout::Vote { .. }) => {
            let event = required_event_for_round(events, CAST_VOTE_EVENT, &round_id)?;
            let positions = required_attribute(event, CAST_VOTE_EVENT, LEAF_INDEX_ATTRIBUTE)?
                .split(',')
                .map(str::trim)
                .collect::<Vec<_>>();
            if positions.len() != 2 {
                return Err(VotingError::InvalidInput {
                    message: "cast_vote leaf_index must contain VAN and vote-commitment positions"
                        .to_string(),
                });
            }
            let final_van_position = parse_compat_u64(positions[0], "cast_vote VAN position")?;
            let vote_commitment_position =
                parse_compat_u64(positions[1], "cast_vote commitment position")?;
            ValidatedChainSubmissionConfirmation::from_hash(
                transaction_hash,
                final_van_position,
                vec![vote_commitment_position],
            )
            .map_err(confirmation_error)
        }
        (
            ChainSubmissionTarget::VoteBatch {
                ordered_batch_digest,
            },
            ExpectedTreeLayout::VoteBatch {
                vote_commitments, ..
            },
        ) => {
            let event = required_event_for_round(events, CAST_VOTE_BATCH_EVENT, &round_id)?;
            let event_digest = parse_canonical_hex32(
                required_attribute(event, CAST_VOTE_BATCH_EVENT, BATCH_DIGEST_ATTRIBUTE)?,
                "cast_vote_batch batch_digest",
            )?;
            if event_digest != ordered_batch_digest {
                return Err(VotingError::InvalidInput {
                    message: "cast_vote_batch digest does not match the locked generation"
                        .to_string(),
                });
            }
            let batch_size = parse_compat_u64(
                required_attribute(event, CAST_VOTE_BATCH_EVENT, BATCH_SIZE_ATTRIBUTE)?,
                "cast_vote_batch batch_size",
            )?;
            let batch_size =
                usize::try_from(batch_size).map_err(|_| VotingError::InvalidInput {
                    message: "cast_vote_batch batch_size does not fit usize".to_string(),
                })?;
            if batch_size == 0
                || batch_size > crate::vote::MAX_VOTE_BATCH_ACTIONS
                || batch_size != vote_commitments.len()
            {
                return Err(VotingError::InvalidInput {
                    message: "cast_vote_batch size does not match the locked generation"
                        .to_string(),
                });
            }
            let proposal_ids = parse_csv_u32(required_attribute(
                event,
                CAST_VOTE_BATCH_EVENT,
                PROPOSAL_IDS_ATTRIBUTE,
            )?)?;
            if proposal_ids != derived.ordered_proposal_ids() {
                return Err(VotingError::InvalidInput {
                    message: "cast_vote_batch proposal order does not match the locked generation"
                        .to_string(),
                });
            }
            let expected_nullifiers = match derived.request() {
                super::generation::ChainSubmissionRequest::VoteBatch(batch) => batch
                    .votes
                    .iter()
                    .map(|vote| {
                        let bytes = BASE64_STANDARD.decode(&vote.van_nullifier).map_err(|_| {
                            VotingError::Internal {
                                message: "derived batch nullifier is not canonical base64"
                                    .to_string(),
                            }
                        })?;
                        if bytes.len() != 32 {
                            return Err(VotingError::Internal {
                                message: "derived batch nullifier has invalid length".to_string(),
                            });
                        }
                        Ok(hex::encode(bytes))
                    })
                    .collect::<Result<Vec<_>, VotingError>>()?,
                _ => unreachable!("batch layout has batch request"),
            };
            let event_nullifiers = parse_csv_strings(required_attribute(
                event,
                CAST_VOTE_BATCH_EVENT,
                VAN_NULLIFIERS_ATTRIBUTE,
            )?)?;
            if event_nullifiers != expected_nullifiers {
                return Err(VotingError::InvalidInput {
                    message: "cast_vote_batch nullifier order does not match the locked generation"
                        .to_string(),
                });
            }
            let final_van_position = parse_compat_u64(
                required_attribute(event, CAST_VOTE_BATCH_EVENT, FINAL_VAN_LEAF_INDEX_ATTRIBUTE)?,
                "cast_vote_batch final VAN position",
            )?;
            let positions = parse_csv_u64(required_attribute(
                event,
                CAST_VOTE_BATCH_EVENT,
                VC_LEAF_INDICES_ATTRIBUTE,
            )?)?;
            if positions.len() != batch_size
                || positions.iter().enumerate().any(|(index, position)| {
                    final_van_position
                        .checked_add(index as u64 + 1)
                        .is_none_or(|expected| *position != expected)
                })
            {
                return Err(VotingError::InvalidInput {
                    message:
                        "cast_vote_batch positions must be the complete adjacent action layout"
                            .to_string(),
                });
            }
            ValidatedChainSubmissionConfirmation::from_hash(
                transaction_hash,
                final_van_position,
                positions,
            )
            .map_err(confirmation_error)
        }
        _ => Err(VotingError::Internal {
            message: "chain submission identity and expected layout disagree".to_string(),
        }),
    }
}

fn required_event_for_round<'a>(
    events: &'a [TxEvent],
    event_type: &str,
    round_id: &str,
) -> Result<&'a TxEvent, VotingError> {
    let mut matching = None;
    let mut wrong_round = None;
    let mut missing_round = false;
    for event in events.iter().filter(|event| event.event_type == event_type) {
        let round_values = event
            .attributes
            .iter()
            .filter(|attribute| ROUND_ID_ATTRIBUTES.contains(&attribute.key.as_str()))
            .map(|attribute| attribute.value.as_str())
            .collect::<Vec<_>>();
        if round_values.len() > 1 {
            return Err(VotingError::InvalidInput {
                message: format!(
                    "{event_type} event must contain exactly one round identity attribute"
                ),
            });
        }
        let event_round = round_values.first().copied();
        match event_round {
            Some(value) if round_id_matches(value, round_id) => {
                if matching.replace(event).is_some() {
                    return Err(VotingError::InvalidInput {
                        message: format!(
                            "ambiguous {event_type} events for round {round_id}; expected one"
                        ),
                    });
                }
            }
            Some(value) => wrong_round = Some(value),
            None => missing_round = true,
        }
    }
    matching.ok_or_else(|| VotingError::InvalidInput {
        message: if let Some(value) = wrong_round {
            format!("{event_type} round mismatch: expected {round_id}, got {value}")
        } else if missing_round {
            format!("{event_type} event is missing its round identity")
        } else {
            format!("missing {event_type} event for round {round_id}")
        },
    })
}

fn required_attribute<'a>(
    event: &'a TxEvent,
    event_type: &str,
    key: &str,
) -> Result<&'a str, VotingError> {
    let values = event
        .attributes
        .iter()
        .filter(|attribute| attribute.key == key)
        .map(|attribute| attribute.value.as_str())
        .collect::<Vec<_>>();
    match values.as_slice() {
        [value] => Ok(value),
        [] => Err(VotingError::InvalidInput {
            message: format!("missing {event_type} {key} in transaction events"),
        }),
        _ => Err(VotingError::InvalidInput {
            message: format!(
                "ambiguous {event_type} {key} attributes in transaction events; expected one"
            ),
        }),
    }
}

fn round_id_matches(event_round_id: &str, expected_round_id: &str) -> bool {
    event_round_id == expected_round_id
        || BASE64_STANDARD.encode(event_round_id.as_bytes()) == expected_round_id
}

fn parse_compat_u64(raw: &str, field: &str) -> Result<u64, VotingError> {
    let raw = raw.trim();
    if let Ok(value) = raw.parse() {
        return Ok(value);
    }
    if !raw.is_ascii() {
        if let Ok(value) = BASE64_STANDARD.encode(raw.as_bytes()).parse() {
            return Ok(value);
        }
    }
    Err(VotingError::InvalidInput {
        message: format!("{field} must be an unsigned 64-bit integer, got {raw:?}"),
    })
}

fn parse_csv_u64(raw: &str) -> Result<Vec<u64>, VotingError> {
    parse_csv(raw, |value| {
        parse_compat_u64(value, "cast_vote_batch position")
    })
}

fn parse_csv_u32(raw: &str) -> Result<Vec<u32>, VotingError> {
    parse_csv(raw, |value| {
        let parsed = parse_compat_u64(value, "cast_vote_batch proposal id")?;
        u32::try_from(parsed).map_err(|_| VotingError::InvalidInput {
            message: "cast_vote_batch proposal id does not fit u32".to_string(),
        })
    })
}

fn parse_csv_strings(raw: &str) -> Result<Vec<String>, VotingError> {
    let values = raw.split(',').map(str::trim).collect::<Vec<_>>();
    if values.is_empty() || values.iter().any(|value| value.is_empty()) {
        return Err(VotingError::InvalidInput {
            message: "cast_vote_batch list must be nonempty".to_string(),
        });
    }
    for value in &values {
        parse_canonical_hex32(value, "cast_vote_batch VAN nullifier")?;
    }
    Ok(values.into_iter().map(str::to_string).collect())
}

fn parse_csv<T>(
    raw: &str,
    mut parse: impl FnMut(&str) -> Result<T, VotingError>,
) -> Result<Vec<T>, VotingError> {
    let values = raw.split(',').map(str::trim).collect::<Vec<_>>();
    if values.is_empty() || values.iter().any(|value| value.is_empty()) {
        return Err(VotingError::InvalidInput {
            message: "cast_vote_batch list must be nonempty".to_string(),
        });
    }
    values.into_iter().map(&mut parse).collect()
}

fn parse_canonical_hex32(raw: &str, field: &str) -> Result<[u8; 32], VotingError> {
    if raw.len() != 64
        || !raw
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(VotingError::InvalidInput {
            message: format!("{field} must be 32-byte canonical lowercase hex"),
        });
    }
    let bytes = hex::decode(raw).map_err(|_| VotingError::InvalidInput {
        message: format!("{field} must be 32-byte canonical lowercase hex"),
    })?;
    bytes.try_into().map_err(|_| VotingError::InvalidInput {
        message: format!("{field} must be 32-byte canonical lowercase hex"),
    })
}

fn confirmation_error(error: super::ChainSubmissionConfirmationError) -> VotingError {
    VotingError::InvalidInput {
        message: error.to_string(),
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::confirmation::TxEventAttribute;

    fn event(attributes: &[(&str, &str)]) -> TxEvent {
        TxEvent {
            event_type: CAST_VOTE_EVENT.to_string(),
            attributes: attributes
                .iter()
                .map(|(key, value)| TxEventAttribute {
                    key: (*key).to_string(),
                    value: (*value).to_string(),
                })
                .collect(),
        }
    }

    #[test]
    fn event_round_requires_exactly_one_supported_attribute() {
        let duplicate = event(&[
            ("vote_round_id", "round"),
            ("vote_round_id", "round"),
            (LEAF_INDEX_ATTRIBUTE, "1,2"),
        ]);
        let both_aliases = event(&[
            ("vote_round_id", "round"),
            ("round_id", "round"),
            (LEAF_INDEX_ATTRIBUTE, "1,2"),
        ]);

        for malformed in [duplicate, both_aliases] {
            assert!(required_event_for_round(&[malformed], CAST_VOTE_EVENT, "round").is_err());
        }
    }

    #[test]
    fn confirmation_attribute_requires_exactly_one_value() {
        let duplicate = event(&[
            ("vote_round_id", "round"),
            (LEAF_INDEX_ATTRIBUTE, "1,2"),
            (LEAF_INDEX_ATTRIBUTE, "1,2"),
        ]);

        assert!(required_attribute(&duplicate, CAST_VOTE_EVENT, LEAF_INDEX_ATTRIBUTE).is_err());
    }

    #[test]
    fn matching_event_for_round_must_be_unique() {
        let first = event(&[("vote_round_id", "round"), (LEAF_INDEX_ATTRIBUTE, "1,2")]);
        let second = first.clone();

        assert!(required_event_for_round(&[first, second], CAST_VOTE_EVENT, "round").is_err());
    }
}
