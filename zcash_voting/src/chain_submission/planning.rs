//! Read-only planning projections over the authoritative submission table.
//!
//! Planners and recovery snapshots must describe chain work from
//! `chain_submissions` rather than from the version-17 domain columns. Those
//! columns record a hash only once a transaction confirms, so a bundle whose
//! generation is `Submitting`, `Tracking`, or `Recovering` would otherwise look
//! unsubmitted and invite a second dispatch of a transaction already on the
//! wire.
//!
//! These helpers are deliberately narrow: they answer "what does the lifecycle
//! already know about this target" and never mutate.

use rusqlite::{named_params, Connection};

use crate::storage::VotingDb;
use crate::types::VotingError;

/// Chain-submission target a planner is asking about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlanningTarget {
    /// The bundle's delegation transaction.
    Delegation,
    /// One vote, matched against its own singleton generation.
    Vote { proposal_id: u32 },
    /// One atomic vote batch, matched by its ordered batch digest.
    VoteBatch { ordered_batch_digest: [u8; 32] },
}

/// Returns the transaction hash the lifecycle associates with `target`.
///
/// Matching is exact: a batch row names no members before confirmation, so
/// matching a vote against any batch on its bundle would attribute another
/// generation's hash to an unrelated vote. A batch member reports no hash until
/// confirmation writes the shared hash into its own projection column, which
/// the caller's fallback then reads. A caller that knows the batch digest asks
/// for the batch row itself, which is authoritative while the batch is in
/// flight.
///
/// Prefers the confirmed hash, then the candidate hash of an in-flight
/// generation. Returns `None` when no authoritative row exists, when the row
/// has no hash yet, or when confirmation came from tree matching rather than a
/// hash. Callers fall back to the legacy projection column so migrated
/// version-17 rows keep reporting their historical hash.
pub(crate) fn lifecycle_transaction_hash(
    conn: &Connection,
    wallet_id: &str,
    round_id: &str,
    bundle_index: u32,
    target: PlanningTarget,
) -> Result<Option<String>, VotingError> {
    // The SQL differs by target, and rusqlite rejects a named parameter the
    // statement does not reference, so each branch binds exactly its own.
    // `chain_submissions` is unique per (wallet, network, round, kind, bundle,
    // proposal), so one vote can own rows on more than one network. Planners
    // carry no network context and cannot tell which row is meant, so this asks for the
    // set of distinct hashes and refuses to guess when it is not a singleton:
    // reporting no hash is honest, attributing another chain's transaction to
    // this vote is not.
    const SELECT: &str =
        "SELECT DISTINCT COALESCE(cs.confirmed_transaction_hash, cs.candidate_transaction_hash)
           FROM chain_submissions cs
          WHERE cs.round_id = :round_id
            AND cs.wallet_id = :wallet_id
            AND cs.bundle_index = :bundle_index
            AND COALESCE(cs.confirmed_transaction_hash, cs.candidate_transaction_hash) IS NOT NULL
            AND ";
    const ORDER: &str = "
          LIMIT 2";

    let mut hashes: Vec<Vec<u8>> = Vec::new();
    {
        let sql = match target {
            PlanningTarget::Delegation => format!("{SELECT}cs.kind IN ('delegation','delegate_and_cast_vote_batch'){ORDER}"),
            PlanningTarget::Vote { .. } => {
                format!("{SELECT}cs.kind = 'vote' AND cs.proposal_id = :proposal{ORDER}")
            }
            PlanningTarget::VoteBatch { .. } => format!(
                "{SELECT}cs.kind IN ('vote_batch','delegate_and_cast_vote_batch') AND cs.ordered_batch_digest = :digest{ORDER}"
            ),
        };
        let mut statement = conn.prepare(&sql).map_err(|e| VotingError::Internal {
            message: format!("failed to prepare lifecycle transaction hash query: {e}"),
        })?;
        let mut rows = match target {
            PlanningTarget::Delegation => statement.query(named_params! {
                ":round_id": round_id,
                ":wallet_id": wallet_id,
                ":bundle_index": bundle_index as i64,
            }),
            PlanningTarget::Vote { proposal_id } => statement.query(named_params! {
                ":round_id": round_id,
                ":wallet_id": wallet_id,
                ":bundle_index": bundle_index as i64,
                ":proposal": proposal_id as i64,
            }),
            PlanningTarget::VoteBatch {
                ordered_batch_digest,
            } => statement.query(named_params! {
                ":round_id": round_id,
                ":wallet_id": wallet_id,
                ":bundle_index": bundle_index as i64,
                ":digest": ordered_batch_digest.as_slice(),
            }),
        }
        .map_err(|e| VotingError::Internal {
            message: format!("failed to load lifecycle transaction hash: {e}"),
        })?;
        while let Some(row) = rows.next().map_err(|e| VotingError::Internal {
            message: format!("failed to read lifecycle transaction hash: {e}"),
        })? {
            hashes.push(row.get(0).map_err(|e| VotingError::Internal {
                message: format!("failed to decode lifecycle transaction hash: {e}"),
            })?);
        }
    }
    let hash = match hashes.len() {
        1 => hashes.pop(),
        _ => None,
    };
    Ok(hash.map(hex::encode))
}

/// The lifecycle transaction hashes of every submission row in one round,
/// read once so a planner can answer [`lifecycle_transaction_hash`] for any
/// target from one consistent snapshot.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct LifecycleTransactionHashes {
    /// Distinct non-null hashes per target. Planners carry no network
    /// context, so a target that owns rows on more than one network has
    /// more than one hash here and reports none; see
    /// [`lifecycle_transaction_hash`].
    hashes: std::collections::BTreeMap<LifecycleHashKey, std::collections::BTreeSet<Vec<u8>>>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum LifecycleHashKey {
    Delegation {
        bundle_index: u32,
    },
    Vote {
        bundle_index: u32,
        proposal_id: u32,
    },
    VoteBatch {
        bundle_index: u32,
        ordered_batch_digest: [u8; 32],
    },
}

impl LifecycleTransactionHashes {
    /// The hash [`lifecycle_transaction_hash`] would report for `target` on
    /// `bundle_index`: the one distinct hash the lifecycle knows, or none.
    pub(crate) fn hash(&self, bundle_index: u32, target: PlanningTarget) -> Option<String> {
        let key = match target {
            PlanningTarget::Delegation => LifecycleHashKey::Delegation { bundle_index },
            PlanningTarget::Vote { proposal_id } => LifecycleHashKey::Vote {
                bundle_index,
                proposal_id,
            },
            PlanningTarget::VoteBatch {
                ordered_batch_digest,
            } => LifecycleHashKey::VoteBatch {
                bundle_index,
                ordered_batch_digest,
            },
        };
        let hashes = self.hashes.get(&key)?;
        match hashes.len() {
            1 => hashes.iter().next().map(hex::encode),
            _ => None,
        }
    }
}

/// Reads every lifecycle transaction hash of `round_id` for `wallet_id`.
pub(crate) fn lifecycle_transaction_hashes(
    conn: &Connection,
    wallet_id: &str,
    round_id: &str,
) -> Result<LifecycleTransactionHashes, VotingError> {
    let mut statement = conn
        .prepare(
            "SELECT cs.kind, cs.bundle_index, cs.proposal_id, cs.ordered_batch_digest,
                    COALESCE(cs.confirmed_transaction_hash, cs.candidate_transaction_hash)
               FROM chain_submissions cs
              WHERE cs.round_id = :round_id
                AND cs.wallet_id = :wallet_id
                AND COALESCE(cs.confirmed_transaction_hash, cs.candidate_transaction_hash) IS NOT NULL",
        )
        .map_err(|e| VotingError::Internal {
            message: format!("failed to prepare lifecycle transaction hashes query: {e}"),
        })?;
    let rows = statement
        .query_map(
            named_params! { ":round_id": round_id, ":wallet_id": wallet_id },
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)? as u32,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, Option<Vec<u8>>>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                ))
            },
        )
        .map_err(|e| VotingError::Internal {
            message: format!("failed to load lifecycle transaction hashes: {e}"),
        })?;
    let mut hashes = LifecycleTransactionHashes::default();
    for row in rows {
        let (kind, bundle_index, proposal_id, digest, hash) =
            row.map_err(|e| VotingError::Internal {
                message: format!("failed to read lifecycle transaction hash: {e}"),
            })?;
        let key = match (kind.as_str(), proposal_id, digest) {
            ("delegation", _, _) => LifecycleHashKey::Delegation { bundle_index },
            ("vote", Some(proposal_id), _) => LifecycleHashKey::Vote {
                bundle_index,
                proposal_id: proposal_id as u32,
            },
            ("vote_batch" | "delegate_and_cast_vote_batch", _, Some(digest)) => {
                LifecycleHashKey::VoteBatch {
                    bundle_index,
                    ordered_batch_digest: digest.try_into().map_err(|_| VotingError::Internal {
                        message: "stored ordered batch digest is not 32 bytes".to_string(),
                    })?,
                }
            }
            (kind, _, _) => {
                return Err(VotingError::Internal {
                    message: format!("chain submission row has unexpected shape for kind {kind}"),
                })
            }
        };
        // A combined row owns both the batch and its delegation prerequisite.
        // Keep the snapshot projection equivalent to the single-target query.
        if kind == "delegate_and_cast_vote_batch" {
            hashes
                .hashes
                .entry(LifecycleHashKey::Delegation { bundle_index })
                .or_default()
                .insert(hash.clone());
        }
        hashes.hashes.entry(key).or_default().insert(hash);
    }
    Ok(hashes)
}

/// Transaction hash to report for one bundle's delegation.
///
/// Prefers the authoritative row so an in-flight generation is visible before
/// confirmation, then falls back to the version-17 projection column so
/// migrated rows keep their historical hash.
pub(crate) fn delegation_transaction_hash(
    db: &VotingDb,
    round_id: &str,
    bundle_index: u32,
) -> Result<Option<String>, VotingError> {
    if let Some(hash) = lifecycle_transaction_hash(
        &db.conn(),
        &db.wallet_id(),
        round_id,
        bundle_index,
        PlanningTarget::Delegation,
    )? {
        return Ok(Some(hash));
    }
    db.get_delegation_tx_hash(round_id, bundle_index)
}

/// Transaction hash to report for one atomic vote batch.
///
/// The batch row is authoritative while the batch is in flight: its members
/// own no lifecycle rows and their projection columns stay empty until
/// confirmation. Falls back to the anchor member's own hash so a confirmed or
/// migrated batch keeps reporting the hash its projection columns hold.
pub(crate) fn vote_batch_transaction_hash(
    db: &VotingDb,
    round_id: &str,
    bundle_index: u32,
    ordered_batch_digest: [u8; 32],
    anchor_proposal_id: u32,
) -> Result<Option<String>, VotingError> {
    if let Some(hash) = lifecycle_transaction_hash(
        &db.conn(),
        &db.wallet_id(),
        round_id,
        bundle_index,
        PlanningTarget::VoteBatch {
            ordered_batch_digest,
        },
    )? {
        return Ok(Some(hash));
    }
    vote_transaction_hash(db, round_id, bundle_index, anchor_proposal_id)
}

/// Transaction hash to report for one vote, singleton or batch member.
///
/// Uses the same authority and fallback as [`delegation_transaction_hash`].
pub(crate) fn vote_transaction_hash(
    db: &VotingDb,
    round_id: &str,
    bundle_index: u32,
    proposal_id: u32,
) -> Result<Option<String>, VotingError> {
    if let Some(hash) = lifecycle_transaction_hash(
        &db.conn(),
        &db.wallet_id(),
        round_id,
        bundle_index,
        PlanningTarget::Vote { proposal_id },
    )? {
        return Ok(Some(hash));
    }
    db.get_vote_tx_hash(round_id, bundle_index, proposal_id)
}
