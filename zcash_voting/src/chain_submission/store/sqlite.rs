//! SQLite authority for the chain-submission lifecycle.

use std::sync::Arc;

use rusqlite::{named_params, OptionalExtension, Transaction, TransactionBehavior};

use crate::storage::VotingDb;

use super::{
    abandoned_diagnostic, ensure_generation, map_generation_error, preserve_loaded_state,
    transition_failure, ChainSubmissionStore, ConfirmationCommit, StoreAdmission,
    StoreAdvancementRequest, StoredChainSubmission, SubmissionDerivationRequest,
};
use crate::chain_submission::{
    confirmation::{
        apply_confirmed_generation, validate_hash_confirmation,
        validate_imported_delegation_confirmation,
    },
    coordination::SubmissionCoordination,
    generation::{
        derive_delegation, derive_imported_delegation, derive_vote, derive_vote_batch,
        DerivedChainSubmission,
    },
    identity::{network_name, submission_identity_key},
    result::ValidatedChainSubmissionConfirmation,
    state::{apply_submission_observation, SubmissionObservation, SubmissionRecordState},
    CandidateTransactionHash, ChainSubmissionDiagnostic, ChainSubmissionDiagnosticKind,
    ChainSubmissionFailure, ChainSubmissionFailureKind, ChainSubmissionGeneration,
    ChainSubmissionGenerationDigest, ChainSubmissionIdentity, ChainSubmissionState,
    ChainSubmissionTarget,
};

pub(crate) struct SqliteChainSubmissionStore {
    db: Arc<VotingDb>,
}

impl SqliteChainSubmissionStore {
    pub(crate) fn new(db: Arc<VotingDb>) -> Self {
        Self { db }
    }

    fn transact<R>(
        &self,
        operation: impl FnOnce(&Transaction<'_>) -> Result<R, ChainSubmissionFailure>,
    ) -> Result<R, ChainSubmissionFailure> {
        let mut conn = self.db.conn();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        let result = operation(&tx)?;
        tx.commit().map_err(storage_error)?;
        Ok(result)
    }
}

#[cfg(test)]
mod tests;

fn storage_error(error: rusqlite::Error) -> ChainSubmissionFailure {
    ChainSubmissionFailure::without_state(ChainSubmissionFailureKind::Storage, error.to_string())
}

fn possible_dispatch_error(error: ChainSubmissionFailure) -> ChainSubmissionFailure {
    ChainSubmissionFailure::with_known_possible_dispatch(error.kind(), error.message())
}

fn derive(
    tx: &Transaction<'_>,
    request: &SubmissionDerivationRequest,
) -> Result<DerivedChainSubmission, ChainSubmissionFailure> {
    match request {
        SubmissionDerivationRequest::Delegation {
            identity,
            spend_auth_signature,
        } => derive_delegation(tx, identity, *spend_auth_signature),
        SubmissionDerivationRequest::ImportedDelegation { identity } => {
            derive_imported_delegation(tx, identity)
        }
        SubmissionDerivationRequest::Vote { identity } => derive_vote(tx, identity),
        SubmissionDerivationRequest::VoteBatch { identity } => derive_vote_batch(tx, identity),
    }
    .map_err(map_generation_error)
}

/// Loads the single authoritative row for one submission identity.
fn load_submission(
    tx: &Transaction<'_>,
    identity: &ChainSubmissionIdentity,
) -> Result<Option<StoredChainSubmission>, ChainSubmissionFailure> {
    load_one(
        tx,
        "SELECT identity_key, generation_digest, state, candidate_transaction_hash,
                committed_post_reservations, tracking_started_at, diagnostic_kind,
                diagnostic, confirmation_source, confirmed_transaction_hash,
                final_van_position, vote_commitment_positions, created_at, updated_at
           FROM chain_submissions WHERE identity_key = :key",
        named_params! { ":key": submission_identity_key(identity) },
        identity,
    )
}

fn load_one<P: rusqlite::Params>(
    tx: &Transaction<'_>,
    sql: &str,
    params: P,
    identity: &ChainSubmissionIdentity,
) -> Result<Option<StoredChainSubmission>, ChainSubmissionFailure> {
    tx.query_row(sql, params, |row| {
        let digest: Vec<u8> = row.get(1)?;
        let candidate: Option<Vec<u8>> = row.get(3)?;
        let attempts: i64 = row.get(4)?;
        let tracking: Option<i64> = row.get(5)?;
        let diagnostic_kind: Option<String> = row.get(6)?;
        let diagnostic_message: Option<String> = row.get(7)?;
        let source: Option<String> = row.get(8)?;
        let confirmed_hash: Option<Vec<u8>> = row.get(9)?;
        let final_van: Option<i64> = row.get(10)?;
        let positions: Option<Vec<u8>> = row.get(11)?;
        let diagnostic = match (diagnostic_kind.as_deref(), diagnostic_message) {
            (Some(kind), Some(message)) => Some(ChainSubmissionDiagnostic::from_redacted_message(
                parse_diagnostic_kind(kind)?,
                message,
            )),
            (None, None) => None,
            _ => return Err(rusqlite::Error::InvalidQuery),
        };
        let candidate = candidate
            .map(bytes32)
            .transpose()?
            .map(CandidateTransactionHash::from_bytes);
        let confirmation = if let Some(source) = source.as_deref() {
            let final_van = u64::try_from(final_van.ok_or(rusqlite::Error::InvalidQuery)?)
                .map_err(|_| rusqlite::Error::InvalidQuery)?;
            let positions = decode_positions(&positions.ok_or(rusqlite::Error::InvalidQuery)?)?;
            let hash = confirmed_hash
                .map(bytes32)
                .transpose()?
                .map(CandidateTransactionHash::from_bytes);
            Some((source, final_van, positions, hash))
        } else {
            None
        };
        let state_name: String = row.get(2)?;
        let state = match state_name.as_str() {
            "submitting" => SubmissionRecordState::Submitting,
            "tracking" => SubmissionRecordState::Tracking {
                candidate_transaction_hash: candidate.ok_or(rusqlite::Error::InvalidQuery)?,
            },
            "recovering" => SubmissionRecordState::Recovering {
                candidate_transaction_hash: candidate,
                ambiguity_diagnostic: diagnostic.clone().ok_or(rusqlite::Error::InvalidQuery)?,
            },
            "submitted_without_hash" => SubmissionRecordState::SubmittedWithoutHash(
                diagnostic.clone().ok_or(rusqlite::Error::InvalidQuery)?,
            ),
            "confirmed" => {
                let (source, final_van, positions, hash) =
                    confirmation.ok_or(rusqlite::Error::InvalidQuery)?;
                let value = match source {
                    "hash" => ValidatedChainSubmissionConfirmation::from_hash(
                        hash.ok_or(rusqlite::Error::InvalidQuery)?,
                        final_van,
                        positions,
                    ),
                    "tree" => ValidatedChainSubmissionConfirmation::from_tree(final_van, positions),
                    _ => return Err(rusqlite::Error::InvalidQuery),
                }
                .map_err(|_| rusqlite::Error::InvalidQuery)?;
                SubmissionRecordState::Confirmed(value)
            }
            "rejected" => SubmissionRecordState::Rejected(
                diagnostic.clone().ok_or(rusqlite::Error::InvalidQuery)?,
            ),
            _ => return Err(rusqlite::Error::InvalidQuery),
        };
        Ok(StoredChainSubmission {
            identity: identity.clone(),
            generation_digest: ChainSubmissionGenerationDigest::from_bytes(bytes32(digest)?),
            state,
            committed_post_reservations: u64::try_from(attempts)
                .map_err(|_| rusqlite::Error::InvalidQuery)?,
            tracking_started_at: tracking
                .map(u64::try_from)
                .transpose()
                .map_err(|_| rusqlite::Error::InvalidQuery)?,
            diagnostic,
            created_at: u64::try_from(row.get::<_, i64>(12)?)
                .map_err(|_| rusqlite::Error::InvalidQuery)?,
            updated_at: u64::try_from(row.get::<_, i64>(13)?)
                .map_err(|_| rusqlite::Error::InvalidQuery)?,
        })
    })
    .optional()
    .map_err(storage_error)
}

fn bytes32(value: Vec<u8>) -> rusqlite::Result<[u8; 32]> {
    value.try_into().map_err(|_| rusqlite::Error::InvalidQuery)
}

fn decode_positions(encoded: &[u8]) -> rusqlite::Result<Vec<u64>> {
    if encoded.len() < 5 || encoded[0] != 1 {
        return Err(rusqlite::Error::InvalidQuery);
    }
    let count = u32::from_be_bytes(encoded[1..5].try_into().unwrap()) as usize;
    if encoded.len() != 5 + count * 8 {
        return Err(rusqlite::Error::InvalidQuery);
    }
    Ok(encoded[5..]
        .chunks_exact(8)
        .map(|chunk| u64::from_be_bytes(chunk.try_into().unwrap()))
        .collect())
}

fn encode_positions(positions: &[u64]) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(5 + positions.len() * 8);
    encoded.push(1);
    encoded.extend_from_slice(&(positions.len() as u32).to_be_bytes());
    for position in positions {
        encoded.extend_from_slice(&position.to_be_bytes());
    }
    encoded
}

fn parse_diagnostic_kind(value: &str) -> rusqlite::Result<ChainSubmissionDiagnosticKind> {
    match value {
        "ambiguous_dispatch" => Ok(ChainSubmissionDiagnosticKind::AmbiguousDispatch),
        "ambiguous_attempts_exhausted" => {
            Ok(ChainSubmissionDiagnosticKind::AmbiguousAttemptsExhausted)
        }
        "nullifier_already_spent" => Ok(ChainSubmissionDiagnosticKind::NullifierAlreadySpent),
        "tracking_window_expired" => Ok(ChainSubmissionDiagnosticKind::TrackingWindowExpired),
        "chain_rejected" => Ok(ChainSubmissionDiagnosticKind::ChainRejected),
        "reconciliation_pending" => Ok(ChainSubmissionDiagnosticKind::ReconciliationPending),
        "invalid_protocol_response" => Ok(ChainSubmissionDiagnosticKind::InvalidProtocolResponse),
        "storage_failure" => Ok(ChainSubmissionDiagnosticKind::StorageFailure),
        _ => Err(rusqlite::Error::InvalidQuery),
    }
}

fn diagnostic_name(value: ChainSubmissionDiagnosticKind) -> &'static str {
    match value {
        ChainSubmissionDiagnosticKind::AmbiguousDispatch => "ambiguous_dispatch",
        ChainSubmissionDiagnosticKind::AmbiguousAttemptsExhausted => "ambiguous_attempts_exhausted",
        ChainSubmissionDiagnosticKind::NullifierAlreadySpent => "nullifier_already_spent",
        ChainSubmissionDiagnosticKind::TrackingWindowExpired => "tracking_window_expired",
        ChainSubmissionDiagnosticKind::ChainRejected => "chain_rejected",
        ChainSubmissionDiagnosticKind::ReconciliationPending => "reconciliation_pending",
        ChainSubmissionDiagnosticKind::InvalidProtocolResponse => "invalid_protocol_response",
        ChainSubmissionDiagnosticKind::StorageFailure => "storage_failure",
    }
}

fn insert_fresh(
    tx: &Transaction<'_>,
    record: &StoredChainSubmission,
) -> Result<(), ChainSubmissionFailure> {
    let identity = record.identity();
    let (kind, proposal, batch): (&str, Option<u32>, Option<Vec<u8>>) = match identity.target() {
        ChainSubmissionTarget::Delegation => ("delegation", None, None),
        ChainSubmissionTarget::Vote { proposal_id } => ("vote", Some(proposal_id), None),
        ChainSubmissionTarget::VoteBatch {
            ordered_batch_digest,
        } => ("vote_batch", None, Some(ordered_batch_digest.to_vec())),
    };
    tx.execute(
        "INSERT INTO chain_submissions
         (identity_key, round_id, wallet_id, network, bundle_index, kind,
          proposal_id, ordered_batch_digest, generation_digest, state,
          committed_post_reservations, created_at, updated_at)
         VALUES (:key, :round, :wallet, :network, :bundle, :kind, :proposal,
                 :batch, :digest, 'submitting', :attempts, :created, :created)",
        named_params! {
            ":key": submission_identity_key(identity), ":round": hex::encode(identity.vote_round_id()),
            ":wallet": identity.wallet_id(), ":network": network_name(identity.network()),
            ":bundle": identity.bundle_index(), ":kind": kind,
            ":proposal": proposal, ":batch": batch,
            ":digest": record.generation_digest().as_bytes(),
            ":attempts": record.committed_post_reservations(), ":created": record.created_at(),
        },
    )
    .map_err(storage_error)?;
    Ok(())
}

fn insert_imported_delegation(
    tx: &Transaction<'_>,
    record: &StoredChainSubmission,
    candidate_transaction_hash: CandidateTransactionHash,
) -> Result<(), ChainSubmissionFailure> {
    let identity = record.identity();
    tx.execute(
        "INSERT INTO chain_submissions
         (identity_key, round_id, wallet_id, network, bundle_index, kind,
          generation_digest, state, candidate_transaction_hash,
          committed_post_reservations, tracking_started_at, created_at, updated_at)
         VALUES (:key, :round, :wallet, :network, :bundle, 'delegation',
                 :digest, 'tracking', :candidate, 0, :created, :created, :created)",
        named_params! {
            ":key": submission_identity_key(identity),
            ":round": hex::encode(identity.vote_round_id()),
            ":wallet": identity.wallet_id(),
            ":network": network_name(identity.network()),
            ":bundle": identity.bundle_index(),
            ":digest": record.generation_digest().as_bytes(),
            ":candidate": candidate_transaction_hash.as_bytes(),
            ":created": record.created_at(),
        },
    )
    .map_err(storage_error)?;
    Ok(())
}

fn persist_mutable(
    tx: &Transaction<'_>,
    record: &StoredChainSubmission,
) -> Result<(), ChainSubmissionFailure> {
    let (state, candidate, diagnostic, confirmation) = match record.state() {
        SubmissionRecordState::Submitting => ("submitting", None, None, None),
        SubmissionRecordState::Tracking {
            candidate_transaction_hash,
        } => (
            "tracking",
            Some(*candidate_transaction_hash),
            record.diagnostic(),
            None,
        ),
        SubmissionRecordState::Recovering {
            candidate_transaction_hash,
            ambiguity_diagnostic,
        } => (
            "recovering",
            *candidate_transaction_hash,
            Some(ambiguity_diagnostic),
            None,
        ),
        SubmissionRecordState::SubmittedWithoutHash(diagnostic) => {
            ("submitted_without_hash", None, Some(diagnostic), None)
        }
        SubmissionRecordState::Confirmed(value) => (
            "confirmed",
            value.confirmation().transaction_hash(),
            None,
            Some(value.confirmation()),
        ),
        SubmissionRecordState::Rejected(value) => ("rejected", None, Some(value), None),
    };
    let (source, confirmed_hash, final_van, positions) =
        confirmation.map_or((None, None, None, None), |value| {
            let source = match value.source() {
                crate::chain_submission::ChainSubmissionConfirmationSource::Hash => "hash",
                crate::chain_submission::ChainSubmissionConfirmationSource::Tree => "tree",
            };
            (
                Some(source),
                value.transaction_hash(),
                Some(value.final_van_position()),
                Some(encode_positions(value.vote_commitment_positions())),
            )
        });
    tx.execute(
        "UPDATE chain_submissions SET state=:state, candidate_transaction_hash=:candidate,
          committed_post_reservations=:attempts, tracking_started_at=:tracking,
          diagnostic_kind=:diagnostic_kind, diagnostic=:diagnostic,
          confirmation_source=:source, confirmed_transaction_hash=:confirmed_hash,
          final_van_position=:final_van, vote_commitment_positions=:positions, updated_at=:updated
          WHERE identity_key=:key",
        named_params! {
            ":state": state, ":candidate": candidate.map(|v| v.as_bytes().to_vec()),
            ":attempts": record.committed_post_reservations(), ":tracking": record.tracking_started_at(),
            ":diagnostic_kind": diagnostic.map(|v| diagnostic_name(v.kind())),
            ":diagnostic": diagnostic.map(ChainSubmissionDiagnostic::message),
            ":source": source, ":confirmed_hash": confirmed_hash.map(|v| v.as_bytes().to_vec()),
            ":final_van": final_van, ":positions": positions, ":updated": record.updated_at(),
            ":key": submission_identity_key(record.identity()),
        },
    ).map_err(storage_error)?;
    Ok(())
}

fn normalize_abandoned(
    tx: &Transaction<'_>,
    mut record: StoredChainSubmission,
    now: u64,
) -> Result<StoredChainSubmission, ChainSubmissionFailure> {
    record.state = apply_submission_observation(
        Some(record.state),
        SubmissionObservation::AbandonedSubmitting(abandoned_diagnostic()),
    )
    .map_err(|error| {
        possible_dispatch_error(transition_failure(ChainSubmissionState::Submitting, error))
    })?
    .ok_or_else(|| {
        possible_dispatch_error(transition_failure(
            ChainSubmissionState::Submitting,
            "abandoned reservation normalization removed the durable row",
        ))
    })?;
    record.diagnostic = match &record.state {
        SubmissionRecordState::Recovering {
            ambiguity_diagnostic,
            ..
        } => Some(ambiguity_diagnostic.clone()),
        _ => None,
    };
    record.updated_at = now.max(record.updated_at);
    persist_mutable(tx, &record).map_err(possible_dispatch_error)?;
    Ok(record)
}

impl ChainSubmissionStore for SqliteChainSubmissionStore {
    fn coordination(&self) -> &SubmissionCoordination {
        &self.db.chain_submission_coordination
    }

    fn admit(
        &self,
        request: &StoreAdvancementRequest,
        work_allowed: bool,
        reservation_ordinal: u64,
        now: u64,
    ) -> Result<StoreAdmission, ChainSubmissionFailure> {
        request.validate_target()?;
        let mut normalizes_abandoned_reservation = false;
        let result = self.transact(|tx| {
            let existing = load_submission(tx, request.identity())?;
            if !work_allowed {
                if let Some(record) = existing {
                    if matches!(record.state(), SubmissionRecordState::Submitting) {
                        normalizes_abandoned_reservation = true;
                        return Ok(StoreAdmission::Authoritative(normalize_abandoned(
                            tx, record, now,
                        )?));
                    }
                    return Ok(StoreAdmission::Authoritative(record));
                }
                return Ok(StoreAdmission::NoAuthoritativeState);
            }
            if existing
                .as_ref()
                .is_some_and(|record| matches!(record.state(), SubmissionRecordState::Submitting))
            {
                normalizes_abandoned_reservation = true;
                return Ok(StoreAdmission::Authoritative(normalize_abandoned(
                    tx,
                    existing.expect("submitting record was just observed"),
                    now,
                )?));
            }
            let mut batch_derived = if request.is_batch() {
                let derived = derive(tx, request.derivation())
                    .map_err(|error| preserve_loaded_state(error, existing.as_ref()))?;
                request
                    .verify_batch_roster(derived.ordered_proposal_ids())
                    .map_err(|error| preserve_loaded_state(error, existing.as_ref()))?;
                Some(derived)
            } else {
                None
            };
            if let Some(record) = existing {
                let derived = match batch_derived.take() {
                    Some(derived) => derived,
                    None => derive(tx, request.derivation())
                        .map_err(|error| preserve_loaded_state(error, Some(&record)))?,
                };
                request.verify_batch_roster(derived.ordered_proposal_ids()).map_err(|error| preserve_loaded_state(error, Some(&record)))?;
                ensure_generation(&record, derived.generation())?;
                if matches!(
                    record.state(),
                    SubmissionRecordState::Confirmed(_)
                        | SubmissionRecordState::Rejected(_)
                        | SubmissionRecordState::SubmittedWithoutHash(_)
                ) {
                    return Ok(StoreAdmission::Authoritative(record));
                }
                return Ok(StoreAdmission::Ready { derived: Box::new(derived), record, fresh_reservation: false });
            }
            let predecessor: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM chain_submissions WHERE wallet_id=:wallet AND network=:network
                  AND round_id=:round AND bundle_index=:bundle
                  AND state IN ('submitting','tracking','recovering','submitted_without_hash')) ",
                named_params! { ":wallet": request.identity().wallet_id(), ":network": network_name(request.identity().network()),
                    ":round": hex::encode(request.identity().vote_round_id()), ":bundle": request.identity().bundle_index() },
                |row| row.get(0),
            ).map_err(storage_error)?;
            if predecessor { return Err(ChainSubmissionFailure::without_state(ChainSubmissionFailureKind::InvalidInput, "another submission for this bundle has not established an authoritative successor")); }
            if matches!(request.identity().target(), ChainSubmissionTarget::Delegation) {
                // A confirmed vote already consumed this bundle's delegation
                // output, so a new delegation generation can never be needed
                // and would only leave an unresolvable row beside it.
                let superseded: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM chain_submissions WHERE wallet_id=:wallet AND network=:network
                      AND round_id=:round AND bundle_index=:bundle
                      AND kind IN ('vote','vote_batch') AND state='confirmed')",
                    named_params! { ":wallet": request.identity().wallet_id(), ":network": network_name(request.identity().network()),
                        ":round": hex::encode(request.identity().vote_round_id()), ":bundle": request.identity().bundle_index() },
                    |row| row.get(0),
                ).map_err(storage_error)?;
                if superseded { return Err(ChainSubmissionFailure::without_state(ChainSubmissionFailureKind::InvalidInput, "a confirmed vote already succeeds this bundle's delegation")); }
            }
            if request.is_imported_delegation() {
                let derived = derive(tx, request.derivation())?;
                let candidate = derived.imported_candidate().ok_or_else(|| {
                    ChainSubmissionFailure::without_state(
                        ChainSubmissionFailureKind::InvariantViolation,
                        "imported delegation derivation omitted its transaction hash",
                    )
                })?;
                let owned_elsewhere: bool = tx
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM chain_submissions
                          WHERE candidate_transaction_hash = :candidate
                             OR confirmed_transaction_hash = :candidate)",
                        named_params! { ":candidate": candidate.as_bytes() },
                        |row| row.get(0),
                    )
                    .map_err(storage_error)?;
                if owned_elsewhere {
                    return Err(ChainSubmissionFailure::without_state(
                        ChainSubmissionFailureKind::InvalidInput,
                        "imported delegation transaction hash belongs to another submission",
                    ));
                }
                let record = StoredChainSubmission::adopted_imported_delegation(
                    derived.generation(),
                    candidate,
                    now,
                );
                insert_imported_delegation(tx, &record, candidate)?;
                return Ok(StoreAdmission::Ready {
                    derived: Box::new(derived),
                    record,
                    fresh_reservation: false,
                });
            }
            let derived = match batch_derived {
                Some(derived) => derived,
                None => derive(tx, request.derivation())?,
            };
            request.verify_batch_roster(derived.ordered_proposal_ids())?;
            let record = StoredChainSubmission::fresh(derived.generation(), reservation_ordinal, now);
            insert_fresh(tx, &record)?;
            Ok(StoreAdmission::Ready { derived: Box::new(derived), record, fresh_reservation: true })
        });
        result.map_err(|error| {
            if normalizes_abandoned_reservation {
                possible_dispatch_error(error)
            } else {
                error
            }
        })
    }

    fn remove_fresh_reservation(
        &self,
        generation: &ChainSubmissionGeneration,
        _now: u64,
    ) -> Result<(), ChainSubmissionFailure> {
        self.transact(|tx| {
            let record = load_submission(tx, generation.identity())?.ok_or_else(|| {
                ChainSubmissionFailure::without_state(
                    ChainSubmissionFailureKind::InvariantViolation,
                    "fresh reservation disappeared",
                )
            })?;
            ensure_generation(&record, generation)?;
            if !matches!(record.state(), SubmissionRecordState::Submitting) {
                return Err(ChainSubmissionFailure::with_durable_state(
                    ChainSubmissionFailureKind::InvariantViolation,
                    record.durable_state(),
                    "only a fresh Submitting reservation can be removed",
                ));
            }
            tx.execute(
                "DELETE FROM chain_submissions WHERE identity_key=?1",
                [submission_identity_key(generation.identity())],
            )
            .map_err(storage_error)?;
            Ok(())
        })
    }

    fn classify_post(
        &self,
        generation: &ChainSubmissionGeneration,
        observation: SubmissionObservation,
        now: u64,
    ) -> Result<StoredChainSubmission, ChainSubmissionFailure> {
        self.apply_observation(generation, observation, None, now)
    }

    fn reserve_ambiguous_retry(
        &self,
        generation: &ChainSubmissionGeneration,
        now: u64,
    ) -> Result<StoredChainSubmission, ChainSubmissionFailure> {
        self.transact(|tx| {
            let mut record = load_submission(tx, generation.identity())?.ok_or_else(|| {
                ChainSubmissionFailure::without_state(
                    ChainSubmissionFailureKind::InvariantViolation,
                    "submission disappeared before ambiguous retry reservation",
                )
            })?;
            ensure_generation(&record, generation)?;
            if !record.state().permits_ambiguous_retry() {
                return Err(ChainSubmissionFailure::with_durable_state(
                    ChainSubmissionFailureKind::InvariantViolation,
                    record.durable_state(),
                    "ambiguous retry requires a hashless possibly-dispatched recovery row",
                ));
            }
            record.committed_post_reservations = record
                .committed_post_reservations
                .checked_add(1)
                .filter(|count| *count <= i64::MAX as u64)
                .ok_or_else(|| {
                    ChainSubmissionFailure::with_durable_state(
                        ChainSubmissionFailureKind::InvariantViolation,
                        ChainSubmissionState::Recovering,
                        "submission reservation counter overflowed",
                    )
                })?;
            record.updated_at = now.max(record.updated_at);
            persist_mutable(tx, &record)?;
            Ok(record)
        })
    }

    fn reconcile(
        &self,
        generation: &ChainSubmissionGeneration,
        observation: SubmissionObservation,
        diagnostic: Option<ChainSubmissionDiagnostic>,
        now: u64,
    ) -> Result<StoredChainSubmission, ChainSubmissionFailure> {
        self.apply_observation(generation, observation, diagnostic, now)
    }

    fn confirm_committed(
        &self,
        request: &StoreAdvancementRequest,
        expected_generation: &ChainSubmissionGeneration,
        candidate: CandidateTransactionHash,
        committed: &crate::chain_submission::protocol::CommittedTransaction,
        commit_allowed: &dyn Fn() -> bool,
        now: u64,
    ) -> Result<ConfirmationCommit, ChainSubmissionFailure> {
        self.transact(|tx| {
            let mut record =
                load_submission(tx, expected_generation.identity())?.ok_or_else(|| {
                    ChainSubmissionFailure::without_state(
                        ChainSubmissionFailureKind::InvariantViolation,
                        "submission disappeared before confirmation",
                    )
                })?;
            ensure_generation(&record, expected_generation)?;
            let derived = derive(tx, request.derivation())
                .map_err(|error| preserve_loaded_state(error, Some(&record)))?;
            if derived.generation() != expected_generation {
                return Err(ChainSubmissionFailure::with_durable_state(
                    ChainSubmissionFailureKind::InvalidInput,
                    record.durable_state(),
                    "semantic generation changed before confirmation",
                ));
            }
            if !commit_allowed() {
                return Ok(ConfirmationCommit::Interrupted(record));
            }
            let confirmation = if request.is_imported_delegation() {
                validate_imported_delegation_confirmation(
                    derived.bound(),
                    candidate,
                    &committed.events,
                )
            } else {
                validate_hash_confirmation(&derived, candidate, &committed.events)
            }
            .map_err(|error| {
                ChainSubmissionFailure::with_durable_state(
                    ChainSubmissionFailureKind::Protocol,
                    record.durable_state(),
                    error.to_string(),
                )
            })?;
            let previous = record.durable_state();
            record.state = apply_submission_observation(
                Some(record.state),
                SubmissionObservation::Confirmed(confirmation.clone()),
            )
            .map_err(|error| transition_failure(previous, error))?
            .unwrap();
            record.diagnostic = None;
            record.updated_at = now.max(record.updated_at);
            apply_confirmed_generation(tx, derived.bound(), &confirmation)
                .map_err(map_generation_error)?;
            persist_mutable(tx, &record)?;
            Ok(ConfirmationCommit::Confirmed(record))
        })
    }

    fn confirm_tree(
        &self,
        request: &StoreAdvancementRequest,
        expected_generation: &ChainSubmissionGeneration,
        final_van_position: u64,
        vote_commitment_positions: Vec<u64>,
        commit_allowed: &dyn Fn() -> bool,
        now: u64,
    ) -> Result<ConfirmationCommit, ChainSubmissionFailure> {
        self.transact(|tx| {
            let mut record =
                load_submission(tx, expected_generation.identity())?.ok_or_else(|| {
                    ChainSubmissionFailure::without_state(
                        ChainSubmissionFailureKind::InvariantViolation,
                        "submission disappeared before tree confirmation",
                    )
                })?;
            ensure_generation(&record, expected_generation)?;
            let derived = derive(tx, request.derivation())
                .map_err(|error| preserve_loaded_state(error, Some(&record)))?;
            request
                .verify_batch_roster(derived.ordered_proposal_ids())
                .map_err(|error| preserve_loaded_state(error, Some(&record)))?;
            if derived.generation() != expected_generation {
                return Err(ChainSubmissionFailure::with_durable_state(
                    ChainSubmissionFailureKind::InvalidInput,
                    record.durable_state(),
                    "semantic generation changed before tree confirmation",
                ));
            }
            if !commit_allowed() {
                return Ok(ConfirmationCommit::Interrupted(record));
            }
            let confirmation =
                crate::chain_submission::result::ValidatedChainSubmissionConfirmation::from_tree(
                    final_van_position,
                    vote_commitment_positions,
                )
                .map_err(|error| {
                    ChainSubmissionFailure::with_durable_state(
                        ChainSubmissionFailureKind::Protocol,
                        record.durable_state(),
                        error.to_string(),
                    )
                })?;
            let previous = record.durable_state();
            record.state = apply_submission_observation(
                Some(record.state),
                SubmissionObservation::Confirmed(confirmation.clone()),
            )
            .map_err(|error| transition_failure(previous, error))?
            .expect("tree confirmation remains durable");
            record.diagnostic = None;
            record.updated_at = now.max(record.updated_at);
            apply_confirmed_generation(tx, derived.bound(), &confirmation)
                .map_err(map_generation_error)?;
            persist_mutable(tx, &record)?;
            Ok(ConfirmationCommit::Confirmed(record))
        })
    }

    fn reserve_recovery_retry(
        &self,
        request: &StoreAdvancementRequest,
        authorization: crate::chain_submission::recovery::RecoveryRetryAuthorization<'_>,
        now: u64,
    ) -> Result<StoredChainSubmission, ChainSubmissionFailure> {
        self.transact(|tx| {
            let identity = authorization.operation().identity();
            if request.identity() != identity {
                return Err(ChainSubmissionFailure::without_state(
                    ChainSubmissionFailureKind::InvalidInput,
                    "recovery request does not match its authorization identity",
                ));
            }
            let mut record = load_submission(tx, identity)?.ok_or_else(|| {
                ChainSubmissionFailure::without_state(
                    ChainSubmissionFailureKind::InvariantViolation,
                    "submission disappeared before recovery retry reservation",
                )
            })?;
            if record.generation_digest() != authorization.generation_digest() {
                return Err(ChainSubmissionFailure::with_durable_state(
                    ChainSubmissionFailureKind::InvalidInput,
                    record.durable_state(),
                    "recovery authorization belongs to a different generation",
                ));
            }
            let derived = derive(tx, request.derivation())
                .map_err(|error| preserve_loaded_state(error, Some(&record)))?;
            request
                .verify_batch_roster(derived.ordered_proposal_ids())
                .map_err(|error| preserve_loaded_state(error, Some(&record)))?;
            ensure_generation(&record, derived.generation())?;
            match record.state() {
                SubmissionRecordState::Recovering {
                    candidate_transaction_hash,
                    ..
                } if *candidate_transaction_hash == authorization.candidate() => {}
                _ => {
                    return Err(ChainSubmissionFailure::with_durable_state(
                        ChainSubmissionFailureKind::InvariantViolation,
                        record.durable_state(),
                        "recovery authorization no longer matches durable state",
                    ));
                }
            }
            record.committed_post_reservations = record
                .committed_post_reservations
                .checked_add(1)
                .filter(|value| *value <= i64::MAX as u64)
                .ok_or_else(|| {
                    ChainSubmissionFailure::with_durable_state(
                        ChainSubmissionFailureKind::InvariantViolation,
                        ChainSubmissionState::Recovering,
                        "recovery reservation counter overflowed",
                    )
                })?;
            if let SubmissionRecordState::Recovering {
                candidate_transaction_hash,
                ..
            } = &mut record.state
            {
                *candidate_transaction_hash = None;
            }
            record.updated_at = now.max(record.updated_at);
            persist_mutable(tx, &record)?;
            Ok(record)
        })
    }
}

impl SqliteChainSubmissionStore {
    fn apply_observation(
        &self,
        generation: &ChainSubmissionGeneration,
        observation: SubmissionObservation,
        diagnostic: Option<ChainSubmissionDiagnostic>,
        now: u64,
    ) -> Result<StoredChainSubmission, ChainSubmissionFailure> {
        self.transact(|tx| {
            let mut record = load_submission(tx, generation.identity())?.ok_or_else(|| {
                ChainSubmissionFailure::without_state(
                    ChainSubmissionFailureKind::InvariantViolation,
                    "submission disappeared during lifecycle transition",
                )
            })?;
            ensure_generation(&record, generation)?;
            let previous = record.durable_state();
            let observation = match observation {
                SubmissionObservation::UsableCandidateHash(candidate) => {
                    let owned_elsewhere: bool = tx
                        .query_row(
                            "SELECT EXISTS(SELECT 1 FROM chain_submissions
                              WHERE identity_key != :key
                                AND (candidate_transaction_hash = :candidate
                                     OR confirmed_transaction_hash = :candidate))",
                            named_params! {
                                ":key": submission_identity_key(generation.identity()),
                                ":candidate": candidate.as_bytes(),
                            },
                            |row| row.get(0),
                        )
                        .map_err(storage_error)?;
                    if owned_elsewhere {
                        SubmissionObservation::PossiblyDispatched(
                            ChainSubmissionDiagnostic::from_redacted_message(
                                ChainSubmissionDiagnosticKind::InvalidProtocolResponse,
                                "vote-chain returned a transaction hash owned by another semantic generation",
                            ),
                        )
                    } else {
                        SubmissionObservation::UsableCandidateHash(candidate)
                    }
                }
                observation => observation,
            };
            record.state = apply_submission_observation(Some(record.state), observation)
                .map_err(|error| transition_failure(previous, error))?
                .ok_or_else(|| {
                    transition_failure(previous, "transition unexpectedly removed durable row")
                })?;
            if let Some(value) = diagnostic {
                record.diagnostic = Some(value);
            }
            record.diagnostic = match &record.state {
                SubmissionRecordState::Recovering {
                    ambiguity_diagnostic,
                    ..
                }
                | SubmissionRecordState::SubmittedWithoutHash(ambiguity_diagnostic)
                | SubmissionRecordState::Rejected(ambiguity_diagnostic) => {
                    Some(ambiguity_diagnostic.clone())
                }
                _ => record.diagnostic,
            };
            let effective_now = now.max(record.updated_at);
            if matches!(record.state(), SubmissionRecordState::Tracking { .. })
                && record.tracking_started_at.is_none()
            {
                record.tracking_started_at = Some(effective_now);
            }
            record.updated_at = effective_now;
            persist_mutable(tx, &record)?;
            Ok(record)
        })
    }
}
