use rusqlite::{named_params, Connection, OptionalExtension};

use super::{load_ballot_intent, load_vote_choice_for_intent_check};
use crate::helper::url::canonicalize_helper_base_url;
use crate::share::ShareDeliveryState;
use crate::types::{ShareDelegationRecord, VotingError};

pub(super) fn delete_for_replaced_vote(
    conn: &Connection,
    round_id: &str,
    wallet_id: &str,
    bundle_index: u32,
    proposal_id: u32,
) -> Result<(), VotingError> {
    conn.execute(
        "DELETE FROM share_delegations
         WHERE round_id = :round_id
           AND wallet_id = :wallet_id
           AND bundle_index = :bundle_index
           AND proposal_id = :proposal_id",
        named_params! {
            ":round_id": round_id,
            ":wallet_id": wallet_id,
            ":bundle_index": bundle_index as i64,
            ":proposal_id": proposal_id as i64,
        },
    )
    .map_err(|e| VotingError::Internal {
        message: format!("failed to clear stale share delegations: {}", e),
    })?;
    Ok(())
}

pub fn clear_stale_share_delegations_for_intent(
    conn: &Connection,
    round_id: &str,
    wallet_id: &str,
    proposal_id: u32,
    skipped: bool,
    choice: Option<u32>,
) -> Result<u64, VotingError> {
    let deleted_row_count = if skipped {
        conn.execute(
            "DELETE FROM share_delegations
             WHERE round_id = :round_id
               AND wallet_id = :wallet_id
               AND proposal_id = :proposal_id",
            named_params! {
                ":round_id": round_id,
                ":wallet_id": wallet_id,
                ":proposal_id": proposal_id as i64,
            },
        )
    } else if let Some(choice) = choice {
        conn.execute(
            "DELETE FROM share_delegations
             WHERE round_id = :round_id
               AND wallet_id = :wallet_id
               AND proposal_id = :proposal_id
               AND NOT EXISTS (
                   SELECT 1 FROM votes
                   WHERE votes.round_id = share_delegations.round_id
                     AND votes.wallet_id = share_delegations.wallet_id
                     AND votes.bundle_index = share_delegations.bundle_index
                     AND votes.proposal_id = share_delegations.proposal_id
                     AND votes.choice = :choice
               )",
            named_params! {
                ":round_id": round_id,
                ":wallet_id": wallet_id,
                ":proposal_id": proposal_id as i64,
                ":choice": choice as i64,
            },
        )
    } else {
        Ok(0)
    }
    .map_err(|e| VotingError::Internal {
        message: format!("failed to clear stale share delegations: {}", e),
    })?;
    Ok(deleted_row_count as u64)
}

/// Splits persisted helper identities into canonical entries and legacy
/// entries accepted by older schemas that no longer canonicalize. Legacy
/// entries are never contacted or counted, but rewrites preserve them verbatim
/// so recorded delivery history is not lost.
fn partition_stored_helper_urls(urls: &[String]) -> (Vec<String>, Vec<String>) {
    let mut canonical_urls = Vec::with_capacity(urls.len());
    let mut preserved_legacy_urls = Vec::new();
    for url in urls {
        match canonicalize_helper_base_url(url) {
            Ok(canonical_url) => {
                if !canonical_urls.contains(&canonical_url) {
                    canonical_urls.push(canonical_url);
                }
            }
            Err(_) => {
                if !preserved_legacy_urls.contains(url) {
                    preserved_legacy_urls.push(url.clone());
                }
            }
        }
    }
    (canonical_urls, preserved_legacy_urls)
}

fn parse_url_list(json: &str, name: &str) -> Result<Vec<String>, VotingError> {
    serde_json::from_str(json).map_err(|e| VotingError::Internal {
        message: format!("failed to deserialize {name}: {e}"),
    })
}

/// Serializes canonical entries followed by preserved legacy entries.
fn serialize_url_list(
    canonical_urls: &[String],
    preserved_legacy_urls: &[String],
    name: &str,
) -> Result<String, VotingError> {
    let stored_urls: Vec<&String> = canonical_urls
        .iter()
        .chain(preserved_legacy_urls.iter())
        .collect();
    serde_json::to_string(&stored_urls).map_err(|e| VotingError::Internal {
        message: format!("failed to serialize {name}: {e}"),
    })
}

/// Durably marks a helper POST as in flight before the request is dispatched.
///
/// The helper URL must canonicalize. Ballot intent and existing share state
/// are validated before the row is updated; callers may dispatch only after
/// this returns `true`.
/// Returns `false` when this helper already has any recorded outcome.
pub fn add_attempting_server(
    conn: &Connection,
    round_id: &str,
    wallet_id: &str,
    bundle_index: u32,
    proposal_id: u32,
    share_index: u32,
    server_url: &str,
) -> Result<bool, VotingError> {
    ensure_share_matches_ballot_intent(conn, round_id, wallet_id, bundle_index, proposal_id)?;
    let (sent_json, ambiguous_json, attempting_json, confirmed): (String, String, String, bool) =
        conn.query_row(
            "SELECT sent_to_urls, ambiguous_urls, attempting_urls, confirmed
             FROM share_delegations
             WHERE round_id = :round_id AND wallet_id = :wallet_id
               AND bundle_index = :bundle_index AND proposal_id = :proposal_id
               AND share_index = :share_index",
            named_params! {
                ":round_id": round_id,
                ":wallet_id": wallet_id,
                ":bundle_index": bundle_index,
                ":proposal_id": proposal_id,
                ":share_index": share_index,
            },
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .map_err(|e| VotingError::Internal {
            message: format!("failed to read helper attempt state: {e}"),
        })?;
    let (definitely_accepted_urls, _) =
        partition_stored_helper_urls(&parse_url_list(&sent_json, "sent_to_urls")?);
    let (outcome_unknown_urls, _) =
        partition_stored_helper_urls(&parse_url_list(&ambiguous_json, "ambiguous_urls")?);
    let (in_flight_urls, preserved_legacy_in_flight_urls) =
        partition_stored_helper_urls(&parse_url_list(&attempting_json, "attempting_urls")?);
    let mut state = ShareDeliveryState::from_url_lists(
        &definitely_accepted_urls,
        &outcome_unknown_urls,
        &in_flight_urls,
    )?;
    if confirmed || !state.begin(server_url)? {
        return Ok(false);
    }
    let attempting_json = serialize_url_list(
        state.in_flight_urls(),
        &preserved_legacy_in_flight_urls,
        "attempting_urls",
    )?;
    conn.execute(
        "UPDATE share_delegations SET attempting_urls = :attempting_urls
         WHERE round_id = :round_id AND wallet_id = :wallet_id
           AND bundle_index = :bundle_index AND proposal_id = :proposal_id
           AND share_index = :share_index",
        named_params! {
            ":attempting_urls": attempting_json,
            ":round_id": round_id,
            ":wallet_id": wallet_id,
            ":bundle_index": bundle_index,
            ":proposal_id": proposal_id,
            ":share_index": share_index,
        },
    )
    .map_err(|e| VotingError::Internal {
        message: format!("failed to record helper attempt: {e}"),
    })?;
    Ok(true)
}

/// Clears an attempt with a definite non-acceptance so the helper remains
/// eligible for a later retry.
pub fn remove_attempting_server(
    conn: &Connection,
    round_id: &str,
    wallet_id: &str,
    bundle_index: u32,
    proposal_id: u32,
    share_index: u32,
    server_url: &str,
) -> Result<(), VotingError> {
    ensure_share_matches_ballot_intent(conn, round_id, wallet_id, bundle_index, proposal_id)?;
    let attempting_json: String = conn
        .query_row(
            "SELECT attempting_urls FROM share_delegations
             WHERE round_id = :round_id AND wallet_id = :wallet_id
               AND bundle_index = :bundle_index AND proposal_id = :proposal_id
               AND share_index = :share_index",
            named_params! {
                ":round_id": round_id,
                ":wallet_id": wallet_id,
                ":bundle_index": bundle_index,
                ":proposal_id": proposal_id,
                ":share_index": share_index,
            },
            |row| row.get(0),
        )
        .map_err(|e| VotingError::Internal {
            message: format!("failed to read attempting_urls: {e}"),
        })?;
    let stored_in_flight_urls: Vec<String> =
        serde_json::from_str(&attempting_json).map_err(|e| VotingError::Internal {
            message: format!("failed to deserialize attempting_urls: {e}"),
        })?;
    let (in_flight_urls, preserved_legacy_in_flight_urls) =
        partition_stored_helper_urls(&stored_in_flight_urls);
    let mut state = ShareDeliveryState::from_url_lists(&[], &[], &in_flight_urls)?;
    state.mark_definite_failure(server_url)?;
    let attempting_json = serialize_url_list(
        state.in_flight_urls(),
        &preserved_legacy_in_flight_urls,
        "attempting_urls",
    )?;
    conn.execute(
        "UPDATE share_delegations SET attempting_urls = :attempting_urls
         WHERE round_id = :round_id AND wallet_id = :wallet_id
           AND bundle_index = :bundle_index AND proposal_id = :proposal_id
           AND share_index = :share_index",
        named_params! {
            ":attempting_urls": attempting_json,
            ":round_id": round_id,
            ":wallet_id": wallet_id,
            ":bundle_index": bundle_index,
            ":proposal_id": proposal_id,
            ":share_index": share_index,
        },
    )
    .map_err(|e| VotingError::Internal {
        message: format!("failed to clear helper attempt: {e}"),
    })?;
    Ok(())
}

/// Creates or strengthens durable delivery evidence for one committed share.
///
/// This raw SQL helper is crate-internal because callers must provide a
/// nullifier that matches the persisted vote recovery bundle. Wallet
/// integrations should use `CommittedVote::submit_share_to_helpers`, which
/// derives that nullifier and owns journal-before-dispatch ordering.
///
/// All reported helper URLs must canonicalize. Existing evidence is merged
/// with definite acceptance taking precedence over outcome-unknown or
/// in-flight state; a conflicting nullifier leaves the row unchanged.
pub(crate) fn record_share_delegation(
    conn: &Connection,
    round_id: &str,
    wallet_id: &str,
    bundle_index: u32,
    proposal_id: u32,
    share_index: u32,
    sent_to_urls: &[String],
    ambiguous_urls: &[String],
    target_count: u32,
    nullifier: &[u8],
    submit_at: u64,
) -> Result<(), VotingError> {
    ensure_share_matches_ballot_intent(conn, round_id, wallet_id, bundle_index, proposal_id)?;
    let existing: Option<(String, String, String, u32, Vec<u8>)> = conn
        .query_row(
            "SELECT sent_to_urls, ambiguous_urls, attempting_urls, target_count, nullifier
             FROM share_delegations
             WHERE round_id = :round_id AND wallet_id = :wallet_id
               AND bundle_index = :bundle_index AND proposal_id = :proposal_id
               AND share_index = :share_index",
            named_params! {
                ":round_id": round_id,
                ":wallet_id": wallet_id,
                ":bundle_index": bundle_index,
                ":proposal_id": proposal_id,
                ":share_index": share_index,
            },
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()
        .map_err(|e| VotingError::Internal {
            message: format!("failed to read existing share delivery: {e}"),
        })?;

    let (
        stored_definite_acceptance_urls,
        stored_outcome_unknown_urls,
        stored_in_flight_urls,
        effective_target,
    ) = if let Some((
        sent_json,
        ambiguous_json,
        attempting_json,
        existing_target,
        existing_nullifier,
    )) = existing
    {
        if existing_nullifier != nullifier {
            return Err(VotingError::InvalidInput {
                message: format!(
                    "share nullifier conflict for round={round_id}, wallet={wallet_id}, bundle={bundle_index}, proposal={proposal_id}, share={share_index}"
                ),
            });
        }
        (
            parse_url_list(&sent_json, "sent_to_urls")?,
            parse_url_list(&ambiguous_json, "ambiguous_urls")?,
            parse_url_list(&attempting_json, "attempting_urls")?,
            existing_target.max(target_count),
        )
    } else {
        (Vec::new(), Vec::new(), Vec::new(), target_count)
    };
    let (definite_acceptance_urls, preserved_legacy_definite_acceptance_urls) =
        partition_stored_helper_urls(&stored_definite_acceptance_urls);
    let (outcome_unknown_urls, mut preserved_legacy_outcome_unknown_urls) =
        partition_stored_helper_urls(&stored_outcome_unknown_urls);
    let (in_flight_urls, mut preserved_legacy_in_flight_urls) =
        partition_stored_helper_urls(&stored_in_flight_urls);
    let mut state = ShareDeliveryState::from_url_lists(
        &definite_acceptance_urls,
        &outcome_unknown_urls,
        &in_flight_urls,
    )?;
    state.merge_persisted_report(sent_to_urls, ambiguous_urls)?;
    preserved_legacy_outcome_unknown_urls
        .retain(|url| !preserved_legacy_definite_acceptance_urls.contains(url));
    preserved_legacy_in_flight_urls.retain(|url| {
        !preserved_legacy_definite_acceptance_urls.contains(url)
            && !preserved_legacy_outcome_unknown_urls.contains(url)
    });

    let definite_acceptance_json = serialize_url_list(
        state.accepted_urls(),
        &preserved_legacy_definite_acceptance_urls,
        "sent_to_urls",
    )?;
    let ambiguous_json = serialize_url_list(
        state.outcome_unknown_urls(),
        &preserved_legacy_outcome_unknown_urls,
        "ambiguous_urls",
    )?;
    let attempting_json = serialize_url_list(
        state.in_flight_urls(),
        &preserved_legacy_in_flight_urls,
        "attempting_urls",
    )?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    conn.execute(
        "INSERT INTO share_delegations \
         (round_id, wallet_id, bundle_index, proposal_id, share_index, sent_to_urls, ambiguous_urls, attempting_urls, target_count, nullifier, confirmed, submit_at, created_at) \
         VALUES (:round_id, :wallet_id, :bundle_index, :proposal_id, :share_index, :sent_to_urls, :ambiguous_urls, :attempting_urls, :target_count, :nullifier, 0, :submit_at, :created_at) \
         ON CONFLICT (round_id, wallet_id, bundle_index, proposal_id, share_index) DO UPDATE SET \
         sent_to_urls = excluded.sent_to_urls, \
         ambiguous_urls = excluded.ambiguous_urls, \
         attempting_urls = excluded.attempting_urls, \
         target_count = excluded.target_count \
         WHERE share_delegations.nullifier = excluded.nullifier",
        named_params! {
            ":round_id": round_id,
            ":wallet_id": wallet_id,
            ":bundle_index": bundle_index,
            ":proposal_id": proposal_id,
            ":share_index": share_index,
            ":sent_to_urls": definite_acceptance_json,
            ":ambiguous_urls": ambiguous_json,
            ":attempting_urls": attempting_json,
            ":target_count": effective_target,
            ":nullifier": nullifier,
            ":submit_at": submit_at,
            ":created_at": now,
        },
    )
    .map_err(|e| VotingError::Internal {
        message: format!("failed to record share delegation: {}", e),
    })
    .and_then(|affected_row_count| {
        if affected_row_count == 0 {
            return Err(VotingError::InvalidInput {
                message: format!(
                    "share nullifier conflict for round={}, wallet={}, bundle={}, proposal={}, share={}",
                    round_id, wallet_id, bundle_index, proposal_id, share_index
                ),
            });
        }
        Ok(())
    })?;
    Ok(())
}

/// Load all share delegations for a round.
pub fn get_share_delegations(
    conn: &Connection,
    round_id: &str,
    wallet_id: &str,
) -> Result<Vec<ShareDelegationRecord>, VotingError> {
    load_share_delegations(
        conn,
        "SELECT bundle_index, proposal_id, share_index, sent_to_urls, ambiguous_urls, attempting_urls, target_count, nullifier, confirmed, submit_at, created_at, round_id \
         FROM share_delegations WHERE round_id = :round_id AND wallet_id = :wallet_id \
         ORDER BY proposal_id, share_index",
        round_id,
        wallet_id,
    )
}

/// Load only unconfirmed share delegations for a round.
pub fn get_unconfirmed_delegations(
    conn: &Connection,
    round_id: &str,
    wallet_id: &str,
) -> Result<Vec<ShareDelegationRecord>, VotingError> {
    load_share_delegations(
        conn,
        "SELECT bundle_index, proposal_id, share_index, sent_to_urls, ambiguous_urls, attempting_urls, target_count, nullifier, confirmed, submit_at, created_at, round_id \
         FROM share_delegations WHERE round_id = :round_id AND wallet_id = :wallet_id AND confirmed = 0 \
         ORDER BY proposal_id, share_index",
        round_id,
        wallet_id,
    )
}

/// Load each round with at least one unconfirmed helper share once.
pub fn pending_share_rounds(
    conn: &Connection,
    wallet_id: &str,
) -> Result<Vec<(String, Option<String>)>, VotingError> {
    let mut stmt = conn
        .prepare(
            "SELECT rounds.round_id, rounds.session_json
             FROM rounds
             WHERE rounds.wallet_id = :wallet_id
               AND EXISTS (
                   SELECT 1
                   FROM share_delegations
                   WHERE share_delegations.round_id = rounds.round_id
                     AND share_delegations.wallet_id = rounds.wallet_id
                     AND share_delegations.confirmed = 0
               )
             ORDER BY rounds.created_at DESC, rounds.round_id",
        )
        .map_err(|e| VotingError::Internal {
            message: format!("failed to prepare pending share round query: {e}"),
        })?;
    let pending_round_rows = stmt
        .query_map(named_params! { ":wallet_id": wallet_id }, |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .map_err(|e| VotingError::Internal {
            message: format!("failed to query pending share rounds: {e}"),
        })?;

    pending_round_rows
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| VotingError::Internal {
            message: format!("failed to read pending share round row: {e}"),
        })
}

fn load_share_delegations(
    conn: &Connection,
    sql: &str,
    round_id: &str,
    wallet_id: &str,
) -> Result<Vec<ShareDelegationRecord>, VotingError> {
    let mut stmt = conn.prepare(sql).map_err(|e| VotingError::Internal {
        message: format!("failed to prepare share delegation query: {}", e),
    })?;
    let share_delegation_rows = stmt
        .query_map(
            named_params! { ":round_id": round_id, ":wallet_id": wallet_id },
            |row| {
                let definite_acceptance_json: String = row.get(3)?;
                let outcome_unknown_json: String = row.get(4)?;
                let in_flight_json: String = row.get(5)?;
                let target_count: u32 = row.get(6)?;
                let nullifier_blob: Vec<u8> = row.get(7)?;
                let confirmed_int: i32 = row.get(8)?;
                let persisted_round_id: String = row.get(11)?;
                Ok((
                    row.get::<_, u32>(0)?,
                    row.get::<_, u32>(1)?,
                    row.get::<_, u32>(2)?,
                    definite_acceptance_json,
                    outcome_unknown_json,
                    in_flight_json,
                    target_count,
                    nullifier_blob,
                    confirmed_int != 0,
                    row.get::<_, u64>(9)?,
                    row.get::<_, u64>(10)?,
                    persisted_round_id,
                ))
            },
        )
        .map_err(|e| VotingError::Internal {
            message: format!("failed to query share delegations: {}", e),
        })?;

    let mut share_delegations = Vec::new();
    for share_delegation_row in share_delegation_rows {
        let (
            bundle_index,
            proposal_id,
            share_index,
            definite_acceptance_json,
            outcome_unknown_json,
            in_flight_json,
            target_count,
            nullifier,
            confirmed,
            submit_at,
            created_at,
            persisted_round_id,
        ) = share_delegation_row.map_err(|e| VotingError::Internal {
            message: format!("failed to read share delegation row: {}", e),
        })?;
        let sent_to_urls: Vec<String> =
            serde_json::from_str(&definite_acceptance_json).map_err(|e| VotingError::Internal {
                message: format!("failed to deserialize sent_to_urls: {}", e),
            })?;
        let ambiguous_urls: Vec<String> =
            serde_json::from_str(&outcome_unknown_json).map_err(|e| VotingError::Internal {
                message: format!("failed to deserialize ambiguous_urls: {}", e),
            })?;
        let attempting_urls: Vec<String> =
            serde_json::from_str(&in_flight_json).map_err(|e| VotingError::Internal {
                message: format!("failed to deserialize attempting_urls: {e}"),
            })?;
        let sent_to_urls = partition_stored_helper_urls(&sent_to_urls).0;
        let ambiguous_urls = partition_stored_helper_urls(&ambiguous_urls).0;
        let attempting_urls = partition_stored_helper_urls(&attempting_urls).0;
        let state =
            ShareDeliveryState::from_url_lists(&sent_to_urls, &ambiguous_urls, &attempting_urls)?;
        share_delegations.push(ShareDelegationRecord {
            round_id: persisted_round_id,
            bundle_index,
            proposal_id,
            share_index,
            sent_to_urls: state.accepted_urls().to_vec(),
            ambiguous_urls: state.outcome_unknown_urls().to_vec(),
            attempting_urls: state.in_flight_urls().to_vec(),
            target_count,
            nullifier,
            confirmed,
            submit_at,
            created_at,
        });
    }
    Ok(share_delegations)
}

/// Read the durable confirmation bit for one helper-share record.
pub fn share_is_confirmed(
    conn: &Connection,
    round_id: &str,
    wallet_id: &str,
    bundle_index: u32,
    proposal_id: u32,
    share_index: u32,
) -> Result<bool, VotingError> {
    ensure_share_matches_ballot_intent(conn, round_id, wallet_id, bundle_index, proposal_id)?;
    conn.query_row(
        "SELECT confirmed FROM share_delegations
         WHERE round_id = :round_id AND wallet_id = :wallet_id
           AND bundle_index = :bundle_index AND proposal_id = :proposal_id
           AND share_index = :share_index",
        named_params! {
            ":round_id": round_id,
            ":wallet_id": wallet_id,
            ":bundle_index": bundle_index,
            ":proposal_id": proposal_id,
            ":share_index": share_index,
        },
        |row| row.get(0),
    )
    .map_err(|e| VotingError::Internal {
        message: format!("failed to read helper-share confirmation: {e}"),
    })
}

/// Mark a share delegation as confirmed on-chain.
pub(crate) fn mark_share_confirmed(
    conn: &Connection,
    round_id: &str,
    wallet_id: &str,
    bundle_index: u32,
    proposal_id: u32,
    share_index: u32,
) -> Result<(), VotingError> {
    ensure_share_matches_ballot_intent(conn, round_id, wallet_id, bundle_index, proposal_id)?;
    let updated = conn
        .execute(
            "UPDATE share_delegations SET confirmed = 1 \
             WHERE round_id = :round_id AND wallet_id = :wallet_id \
             AND bundle_index = :bundle_index AND proposal_id = :proposal_id AND share_index = :share_index",
            named_params! {
                ":round_id": round_id,
                ":wallet_id": wallet_id,
                ":bundle_index": bundle_index,
                ":proposal_id": proposal_id,
                ":share_index": share_index,
            },
        )
        .map_err(|e| VotingError::Internal {
            message: format!("failed to mark share confirmed: {}", e),
        })?;
    if updated == 0 {
        return Err(VotingError::Internal {
            message: format!(
                "no share delegation found: round={}, bundle={}, proposal={}, share={}",
                round_id, bundle_index, proposal_id, share_index
            ),
        });
    }
    Ok(())
}

fn ensure_share_matches_ballot_intent(
    conn: &Connection,
    round_id: &str,
    wallet_id: &str,
    bundle_index: u32,
    proposal_id: u32,
) -> Result<(), VotingError> {
    let intent = load_ballot_intent(conn, round_id, wallet_id, proposal_id, "share delegation")?;
    let Some((skipped, choice)) = intent else {
        return Ok(());
    };
    if skipped != 0 {
        return Err(VotingError::InvalidInput {
            message: format!(
                "cannot record share delegation for skipped proposal round={}, wallet={}, bundle={}, proposal={}",
                round_id, wallet_id, bundle_index, proposal_id
            ),
        });
    }
    let Some(choice) = choice else {
        return Err(VotingError::InvalidInput {
            message: format!(
                "ballot intent choice missing for round={}, wallet={}, proposal={}",
                round_id, wallet_id, proposal_id
            ),
        });
    };
    let vote_choice = load_vote_choice_for_intent_check(
        conn,
        round_id,
        wallet_id,
        bundle_index,
        proposal_id,
        "share delegation",
    )?;
    if vote_choice == Some(choice) {
        return Ok(());
    }
    Err(VotingError::InvalidInput {
        message: format!(
            "share delegation conflicts with ballot intent for round={}, wallet={}, bundle={}, proposal={}",
            round_id, wallet_id, bundle_index, proposal_id
        ),
    })
}

/// Appends definite delivery evidence, supersedes weaker evidence for those
/// helpers, and makes the share immediately actionable.
pub(crate) fn add_sent_servers(
    conn: &Connection,
    round_id: &str,
    wallet_id: &str,
    bundle_index: u32,
    proposal_id: u32,
    share_index: u32,
    new_urls: &[String],
) -> Result<(), VotingError> {
    update_sent_servers(
        conn,
        round_id,
        wallet_id,
        bundle_index,
        proposal_id,
        share_index,
        new_urls,
        true,
    )
}

/// Append definite deliveries without changing their scheduled submit time.
pub fn add_sent_servers_preserving_schedule(
    conn: &Connection,
    round_id: &str,
    wallet_id: &str,
    bundle_index: u32,
    proposal_id: u32,
    share_index: u32,
    new_urls: &[String],
) -> Result<(), VotingError> {
    update_sent_servers(
        conn,
        round_id,
        wallet_id,
        bundle_index,
        proposal_id,
        share_index,
        new_urls,
        false,
    )
}

/// How [`merge_share_delegation_urls`] folds newly reported helpers into the
/// persisted delivery state.
enum HelperUrlMerge {
    /// Definite delivery evidence supersedes outcome-unknown state.
    DefiniteAcceptance,
    /// Outcome-unknown evidence cannot replace definite acceptance.
    OutcomeUnknown,
}

/// Shared read-modify-write for the per-share helper delivery lists. Every
/// merged helper also leaves `attempting_urls`, and legacy entries that no
/// longer canonicalize are preserved verbatim.
#[allow(clippy::too_many_arguments)]
fn merge_share_delegation_urls(
    conn: &Connection,
    round_id: &str,
    wallet_id: &str,
    bundle_index: u32,
    proposal_id: u32,
    share_index: u32,
    new_urls: &[String],
    merge: HelperUrlMerge,
    reset_submit_at: bool,
) -> Result<(), VotingError> {
    ensure_share_matches_ballot_intent(conn, round_id, wallet_id, bundle_index, proposal_id)?;
    let (sent_json, ambiguous_json, attempting_json): (String, String, String) = conn
        .query_row(
            "SELECT sent_to_urls, ambiguous_urls, attempting_urls FROM share_delegations \
             WHERE round_id = :round_id AND wallet_id = :wallet_id \
             AND bundle_index = :bundle_index AND proposal_id = :proposal_id AND share_index = :share_index",
            named_params! {
                ":round_id": round_id,
                ":wallet_id": wallet_id,
                ":bundle_index": bundle_index,
                ":proposal_id": proposal_id,
                ":share_index": share_index,
            },
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(|e| VotingError::Internal {
            message: format!("failed to read helper delivery state for update: {e}"),
        })?;
    let (definite_acceptance_urls, preserved_legacy_definite_acceptance_urls) =
        partition_stored_helper_urls(&parse_url_list(&sent_json, "sent_to_urls")?);
    let (outcome_unknown_urls, preserved_legacy_outcome_unknown_urls) =
        partition_stored_helper_urls(&parse_url_list(&ambiguous_json, "ambiguous_urls")?);
    let (in_flight_urls, preserved_legacy_in_flight_urls) =
        partition_stored_helper_urls(&parse_url_list(&attempting_json, "attempting_urls")?);
    let mut state = ShareDeliveryState::from_url_lists(
        &definite_acceptance_urls,
        &outcome_unknown_urls,
        &in_flight_urls,
    )?;
    for url in new_urls {
        match merge {
            HelperUrlMerge::DefiniteAcceptance => state.mark_accepted(url)?,
            HelperUrlMerge::OutcomeUnknown => state.mark_outcome_unknown(url)?,
        }
    }
    let updated_sent = serialize_url_list(
        state.accepted_urls(),
        &preserved_legacy_definite_acceptance_urls,
        "sent_to_urls",
    )?;
    let updated_ambiguous = serialize_url_list(
        state.outcome_unknown_urls(),
        &preserved_legacy_outcome_unknown_urls,
        "ambiguous_urls",
    )?;
    let updated_attempting = serialize_url_list(
        state.in_flight_urls(),
        &preserved_legacy_in_flight_urls,
        "attempting_urls",
    )?;
    conn.execute(
        "UPDATE share_delegations SET sent_to_urls = :sent_to_urls, ambiguous_urls = :ambiguous_urls, \
         attempting_urls = :attempting_urls, submit_at = iif(:reset_submit_at, 0, submit_at) \
         WHERE round_id = :round_id AND wallet_id = :wallet_id \
         AND bundle_index = :bundle_index AND proposal_id = :proposal_id AND share_index = :share_index",
        named_params! {
            ":sent_to_urls": updated_sent,
            ":ambiguous_urls": updated_ambiguous,
            ":attempting_urls": updated_attempting,
            ":reset_submit_at": reset_submit_at,
            ":round_id": round_id,
            ":wallet_id": wallet_id,
            ":bundle_index": bundle_index,
            ":proposal_id": proposal_id,
            ":share_index": share_index,
        },
    )
    .map_err(|e| VotingError::Internal {
        message: format!("failed to update helper delivery state: {e}"),
    })?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn update_sent_servers(
    conn: &Connection,
    round_id: &str,
    wallet_id: &str,
    bundle_index: u32,
    proposal_id: u32,
    share_index: u32,
    new_urls: &[String],
    reset_submit_at: bool,
) -> Result<(), VotingError> {
    merge_share_delegation_urls(
        conn,
        round_id,
        wallet_id,
        bundle_index,
        proposal_id,
        share_index,
        new_urls,
        HelperUrlMerge::DefiniteAcceptance,
        reset_submit_at,
    )
}

/// Append outcome-unknown helper attempts without overriding definite deliveries.
/// `reset_submit_at` makes overdue recovery immediately actionable; early
/// replenishment leaves the delayed schedule intact.
pub fn add_ambiguous_servers(
    conn: &Connection,
    round_id: &str,
    wallet_id: &str,
    bundle_index: u32,
    proposal_id: u32,
    share_index: u32,
    new_urls: &[String],
    reset_submit_at: bool,
) -> Result<(), VotingError> {
    merge_share_delegation_urls(
        conn,
        round_id,
        wallet_id,
        bundle_index,
        proposal_id,
        share_index,
        new_urls,
        HelperUrlMerge::OutcomeUnknown,
        reset_submit_at,
    )
}
