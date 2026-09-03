//! Internal transactional storage contract for the public submission lifecycle.

use crate::types::VotingError;

use super::{
    coordination::{SubmissionCoordination, SubmissionOperationKey},
    generation::DerivedChainSubmission,
    protocol::CommittedTransaction,
    recovery::RecoveryRetryAuthorization,
    state::{SubmissionObservation, SubmissionRecordState},
    ChainSubmissionDiagnostic, ChainSubmissionDiagnosticKind, ChainSubmissionFailure,
    ChainSubmissionFailureKind, ChainSubmissionGeneration, ChainSubmissionGenerationDigest,
    ChainSubmissionIdentity, ChainSubmissionPending, ChainSubmissionResult, ChainSubmissionState,
    ChainSubmissionTarget,
};

mod sqlite;
pub(super) use sqlite::SqliteChainSubmissionStore;

#[cfg(test)]
mod tests;

#[cfg(test)]
use super::{
    confirmation::{validate_hash_confirmation, validate_imported_delegation_confirmation},
    coordination::BundleOperationKey,
    result::ValidatedChainSubmissionConfirmation,
    state::apply_submission_observation,
};

/// Inputs from which storage reconstructs a closed semantic generation.
pub(super) enum SubmissionDerivationRequest {
    Delegation {
        identity: ChainSubmissionIdentity,
        spend_auth_signature: [u8; 64],
    },
    ImportedDelegation {
        identity: ChainSubmissionIdentity,
    },
    Vote {
        identity: ChainSubmissionIdentity,
    },
    VoteBatch {
        identity: ChainSubmissionIdentity,
    },
}

impl SubmissionDerivationRequest {
    pub(super) fn identity(&self) -> &ChainSubmissionIdentity {
        match self {
            Self::Delegation { identity, .. }
            | Self::ImportedDelegation { identity }
            | Self::Vote { identity }
            | Self::VoteBatch { identity } => identity,
        }
    }
}

/// One advancement request plus recovery-independent legacy identities.
pub(super) struct StoreAdvancementRequest {
    derivation: SubmissionDerivationRequest,
    /// Singleton member identities an atomic batch would overlap.
    ///
    /// Empty for delegation and for a singleton, whose own identity is already
    /// the authoritative one.
    member_identities: Vec<ChainSubmissionIdentity>,
    ordered_batch_proposal_ids: Option<Vec<u32>>,
}

impl StoreAdvancementRequest {
    pub(super) fn delegation(
        identity: ChainSubmissionIdentity,
        spend_auth_signature: [u8; 64],
    ) -> Self {
        Self {
            derivation: SubmissionDerivationRequest::Delegation {
                identity,
                spend_auth_signature,
            },
            member_identities: vec![],
            ordered_batch_proposal_ids: None,
        }
    }

    pub(super) fn imported_delegation(identity: ChainSubmissionIdentity) -> Self {
        Self {
            derivation: SubmissionDerivationRequest::ImportedDelegation { identity },
            member_identities: vec![],
            ordered_batch_proposal_ids: None,
        }
    }

    pub(super) fn vote(identity: ChainSubmissionIdentity) -> Self {
        Self {
            member_identities: vec![],
            derivation: SubmissionDerivationRequest::Vote { identity },
            ordered_batch_proposal_ids: None,
        }
    }

    pub(super) fn vote_batch(
        identity: ChainSubmissionIdentity,
        ordered_proposal_ids: Vec<u32>,
    ) -> Result<Self, ChainSubmissionFailure> {
        if !matches!(identity.target(), ChainSubmissionTarget::VoteBatch { .. })
            || ordered_proposal_ids.is_empty()
        {
            return Err(ChainSubmissionFailure::without_state(
                ChainSubmissionFailureKind::InvalidInput,
                "vote-batch requests require a batch identity and a non-empty ordered proposal roster",
            ));
        }
        if ordered_proposal_ids.len() > crate::vote::MAX_VOTE_BATCH_ACTIONS {
            return Err(ChainSubmissionFailure::without_state(
                ChainSubmissionFailureKind::InvalidInput,
                format!(
                    "vote-batch proposal roster exceeds the {}-action protocol maximum",
                    crate::vote::MAX_VOTE_BATCH_ACTIONS
                ),
            ));
        }
        let mut member_identities = ordered_proposal_ids
            .iter()
            .map(|proposal_id| {
                ChainSubmissionIdentity::new(
                    identity.wallet_id(),
                    identity.network(),
                    *identity.vote_round_id(),
                    identity.bundle_index(),
                    ChainSubmissionTarget::Vote {
                        proposal_id: *proposal_id,
                    },
                )
                .map_err(|error| {
                    ChainSubmissionFailure::without_state(
                        ChainSubmissionFailureKind::InvalidInput,
                        error.to_string(),
                    )
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        member_identities.sort_by_key(SubmissionOperationKey::from_identity);
        let original_len = member_identities.len();
        member_identities.dedup();
        if member_identities.len() != original_len {
            return Err(ChainSubmissionFailure::without_state(
                ChainSubmissionFailureKind::InvalidInput,
                "vote-batch proposal roster contains duplicates",
            ));
        }
        Ok(Self {
            derivation: SubmissionDerivationRequest::VoteBatch { identity },
            member_identities,
            ordered_batch_proposal_ids: Some(ordered_proposal_ids),
        })
    }

    pub(super) fn derivation(&self) -> &SubmissionDerivationRequest {
        &self.derivation
    }

    pub(super) fn identity(&self) -> &ChainSubmissionIdentity {
        self.derivation.identity()
    }

    pub(super) fn applicable_identities(&self) -> Vec<ChainSubmissionIdentity> {
        self.member_identities
            .iter()
            .cloned()
            .chain(std::iter::once(self.identity().clone()))
            .collect()
    }

    fn is_batch(&self) -> bool {
        matches!(
            self.derivation,
            SubmissionDerivationRequest::VoteBatch { .. }
        )
    }

    pub(super) fn is_imported_delegation(&self) -> bool {
        matches!(
            self.derivation,
            SubmissionDerivationRequest::ImportedDelegation { .. }
        )
    }

    fn verify_batch_roster(
        &self,
        authoritative_ordered_proposal_ids: &[u32],
    ) -> Result<(), ChainSubmissionFailure> {
        if let Some(expected) = &self.ordered_batch_proposal_ids {
            if authoritative_ordered_proposal_ids != expected {
                return Err(ChainSubmissionFailure::without_state(
                    ChainSubmissionFailureKind::InvalidInput,
                    "vote-batch proposal roster does not match the complete persisted batch",
                ));
            }
        }
        Ok(())
    }

    fn validate_target(&self) -> Result<(), ChainSubmissionFailure> {
        let valid = matches!(
            (&self.derivation, self.identity().target()),
            (
                SubmissionDerivationRequest::Delegation { .. },
                ChainSubmissionTarget::Delegation
            ) | (
                SubmissionDerivationRequest::ImportedDelegation { .. },
                ChainSubmissionTarget::Delegation
            ) | (
                SubmissionDerivationRequest::Vote { .. },
                ChainSubmissionTarget::Vote { .. }
            ) | (
                SubmissionDerivationRequest::VoteBatch { .. },
                ChainSubmissionTarget::VoteBatch { .. }
            )
        );
        if valid {
            Ok(())
        } else {
            Err(ChainSubmissionFailure::without_state(
                ChainSubmissionFailureKind::InvalidInput,
                "submission request kind does not match its typed identity target",
            ))
        }
    }
}

/// Complete durable record used by the coordinator and SQLite adapter.
#[derive(Clone)]
pub(super) struct StoredChainSubmission {
    identity: ChainSubmissionIdentity,
    generation_digest: ChainSubmissionGenerationDigest,
    state: SubmissionRecordState,
    committed_post_reservations: u64,
    tracking_started_at: Option<u64>,
    diagnostic: Option<ChainSubmissionDiagnostic>,
    created_at: u64,
    updated_at: u64,
}

impl StoredChainSubmission {
    fn fresh(
        generation: &ChainSubmissionGeneration,
        committed_post_reservations: u64,
        now: u64,
    ) -> Self {
        Self {
            identity: generation.identity().clone(),
            generation_digest: generation.digest(),
            state: SubmissionRecordState::Submitting,
            committed_post_reservations,
            tracking_started_at: None,
            diagnostic: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn adopted_imported_delegation(
        generation: &ChainSubmissionGeneration,
        candidate_transaction_hash: super::CandidateTransactionHash,
        now: u64,
    ) -> Self {
        Self {
            identity: generation.identity().clone(),
            generation_digest: generation.digest(),
            state: SubmissionRecordState::Tracking {
                candidate_transaction_hash,
            },
            committed_post_reservations: 0,
            tracking_started_at: Some(now),
            diagnostic: None,
            created_at: now,
            updated_at: now,
        }
    }

    pub(super) fn identity(&self) -> &ChainSubmissionIdentity {
        &self.identity
    }

    pub(super) fn generation_digest(&self) -> ChainSubmissionGenerationDigest {
        self.generation_digest
    }

    pub(super) fn state(&self) -> &SubmissionRecordState {
        &self.state
    }

    pub(super) fn durable_state(&self) -> ChainSubmissionState {
        self.state.durable_state()
    }

    pub(super) fn tracking_started_at(&self) -> Option<u64> {
        self.tracking_started_at
    }

    pub(super) fn committed_post_reservations(&self) -> u64 {
        self.committed_post_reservations
    }

    pub(super) fn diagnostic(&self) -> Option<&ChainSubmissionDiagnostic> {
        self.diagnostic.as_ref()
    }

    pub(super) fn created_at(&self) -> u64 {
        self.created_at
    }

    pub(super) fn updated_at(&self) -> u64 {
        self.updated_at
    }

    pub(super) fn public_result(&self) -> Result<ChainSubmissionResult, ChainSubmissionFailure> {
        match self.state.clone() {
            SubmissionRecordState::Submitting => Err(ChainSubmissionFailure::with_durable_state(
                ChainSubmissionFailureKind::InvariantViolation,
                ChainSubmissionState::Submitting,
                "Submitting must be normalized before returning from an advancement pass",
            )),
            SubmissionRecordState::Tracking {
                candidate_transaction_hash,
            } => Ok(ChainSubmissionResult::Pending(
                ChainSubmissionPending::Tracking {
                    candidate_transaction_hash,
                },
            )),
            SubmissionRecordState::Recovering {
                candidate_transaction_hash,
                ambiguity_diagnostic,
            } => Ok(ChainSubmissionResult::Pending(
                ChainSubmissionPending::Recovering {
                    candidate_transaction_hash,
                    diagnostic: ambiguity_diagnostic,
                },
            )),
            SubmissionRecordState::Confirmed(confirmation) => {
                Ok(ChainSubmissionResult::Confirmed(confirmation.into_public()))
            }
            SubmissionRecordState::Rejected(diagnostic) => {
                Ok(ChainSubmissionResult::Rejected(diagnostic))
            }
        }
    }
}

pub(super) enum StoreAdmission {
    NoAuthoritativeState,
    Authoritative(StoredChainSubmission),
    Ready {
        derived: Box<DerivedChainSubmission>,
        record: StoredChainSubmission,
        fresh_reservation: bool,
    },
}

pub(super) enum ConfirmationCommit {
    Interrupted(StoredChainSubmission),
    Confirmed(StoredChainSubmission),
}

/// Atomic persistence operations required by one bounded lifecycle pass.
pub(super) trait ChainSubmissionStore: Send + Sync {
    /// Returns the process-local coordination authority inseparable from this
    /// database authority.
    fn coordination(&self) -> &SubmissionCoordination;

    /// Loads guards before derivation, normalizes abandoned reservations, and
    /// optionally creates the first committed POST reservation.
    fn admit(
        &self,
        request: &StoreAdvancementRequest,
        work_allowed: bool,
        reservation_ordinal: u64,
        now: u64,
    ) -> Result<StoreAdmission, ChainSubmissionFailure>;

    /// Removes the current call's fresh reservation after a definite non-send.
    fn remove_fresh_reservation(
        &self,
        generation: &ChainSubmissionGeneration,
        now: u64,
    ) -> Result<(), ChainSubmissionFailure>;

    /// Durably classifies one POST before any later network work or return.
    fn classify_post(
        &self,
        generation: &ChainSubmissionGeneration,
        observation: SubmissionObservation,
        now: u64,
    ) -> Result<StoredChainSubmission, ChainSubmissionFailure>;

    /// Applies a status observation without changing the semantic generation.
    fn reconcile(
        &self,
        generation: &ChainSubmissionGeneration,
        observation: SubmissionObservation,
        diagnostic: Option<ChainSubmissionDiagnostic>,
        now: u64,
    ) -> Result<StoredChainSubmission, ChainSubmissionFailure>;

    /// Re-derives and atomically confirms a generation. `commit_allowed` is
    /// checked after derivation and immediately before event validation; no
    /// cancellation check is permitted after validation succeeds.
    fn confirm_committed(
        &self,
        request: &StoreAdvancementRequest,
        expected_generation: &ChainSubmissionGeneration,
        candidate: super::CandidateTransactionHash,
        committed: &CommittedTransaction,
        commit_allowed: &dyn Fn() -> bool,
        now: u64,
    ) -> Result<ConfirmationCommit, ChainSubmissionFailure>;

    /// Re-derives and atomically confirms an exact tree-layout match.
    fn confirm_tree(
        &self,
        request: &StoreAdvancementRequest,
        expected_generation: &ChainSubmissionGeneration,
        final_van_position: u64,
        vote_commitment_positions: Vec<u64>,
        commit_allowed: &dyn Fn() -> bool,
        now: u64,
    ) -> Result<ConfirmationCommit, ChainSubmissionFailure>;

    /// Atomically consumes a continuously-held complete no-match proof,
    /// retires its candidate, and reserves one same-generation recovery POST.
    fn reserve_recovery_retry(
        &self,
        request: &StoreAdvancementRequest,
        authorization: RecoveryRetryAuthorization<'_>,
        now: u64,
    ) -> Result<StoredChainSubmission, ChainSubmissionFailure>;
}

pub(super) fn abandoned_diagnostic() -> ChainSubmissionDiagnostic {
    ChainSubmissionDiagnostic::from_redacted_message(
        ChainSubmissionDiagnosticKind::AmbiguousDispatch,
        "an unclassified submission reservation survived process interruption",
    )
}

pub(super) fn ensure_generation(
    record: &StoredChainSubmission,
    generation: &ChainSubmissionGeneration,
) -> Result<(), ChainSubmissionFailure> {
    if record.identity() != generation.identity()
        || record.generation_digest() != generation.digest()
    {
        return Err(ChainSubmissionFailure::with_durable_state(
            ChainSubmissionFailureKind::InvalidInput,
            record.durable_state(),
            "stored submission belongs to a different semantic generation",
        ));
    }
    Ok(())
}

pub(super) fn transition_failure(
    state: ChainSubmissionState,
    error: impl std::fmt::Display,
) -> ChainSubmissionFailure {
    ChainSubmissionFailure::with_durable_state(
        ChainSubmissionFailureKind::InvariantViolation,
        state,
        error.to_string(),
    )
}

pub(super) fn preserve_loaded_state(
    error: ChainSubmissionFailure,
    record: Option<&StoredChainSubmission>,
) -> ChainSubmissionFailure {
    match (error.strongest_state(), record) {
        (None, Some(record)) => ChainSubmissionFailure::with_durable_state(
            error.kind(),
            record.durable_state(),
            error.message(),
        ),
        _ => error,
    }
}

pub(super) fn map_generation_error(error: VotingError) -> ChainSubmissionFailure {
    ChainSubmissionFailure::without_state(
        match error {
            VotingError::InvalidInput { .. } => ChainSubmissionFailureKind::InvalidInput,
            _ => ChainSubmissionFailureKind::Storage,
        },
        error.to_string(),
    )
}

#[cfg(test)]
pub(super) mod memory {
    use std::{
        collections::HashMap,
        sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Mutex,
        },
    };

    use super::*;

    #[derive(Clone, Default)]
    struct MemoryState {
        records: HashMap<ChainSubmissionIdentity, StoredChainSubmission>,
        derivations: HashMap<ChainSubmissionIdentity, DerivedChainSubmission>,
        batch_rosters: HashMap<ChainSubmissionIdentity, Vec<u32>>,
        projections: HashMap<ChainSubmissionIdentity, super::super::ChainSubmissionConfirmation>,
        fail_before_commit: bool,
        fail_before_commit_without_state: bool,
    }

    pub(in crate::chain_submission) struct InMemoryChainSubmissionStore {
        state: Mutex<MemoryState>,
        coordination: SubmissionCoordination,
        fail_confirmation: AtomicBool,
        batch_roster_reads: AtomicUsize,
        confirmation_validated_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
        required_admission_locks: Mutex<Option<Vec<ChainSubmissionIdentity>>>,
    }

    impl Default for InMemoryChainSubmissionStore {
        fn default() -> Self {
            Self {
                state: Mutex::new(MemoryState::default()),
                coordination: SubmissionCoordination::default(),
                fail_confirmation: AtomicBool::new(false),
                batch_roster_reads: AtomicUsize::new(0),
                confirmation_validated_hook: Mutex::new(None),
                required_admission_locks: Mutex::new(None),
            }
        }
    }

    impl InMemoryChainSubmissionStore {
        pub(in crate::chain_submission) fn seed_derivation(&self, derived: DerivedChainSubmission) {
            let mut state = self.state.lock().unwrap();
            if matches!(
                derived.generation().identity().target(),
                ChainSubmissionTarget::VoteBatch { .. }
            ) {
                state.batch_rosters.insert(
                    derived.generation().identity().clone(),
                    derived.ordered_proposal_ids().to_vec(),
                );
            }
            state
                .derivations
                .insert(derived.generation().identity().clone(), derived);
        }

        pub(in crate::chain_submission) fn seed_batch_roster(
            &self,
            identity: ChainSubmissionIdentity,
            ordered_proposal_ids: Vec<u32>,
        ) {
            self.state
                .lock()
                .unwrap()
                .batch_rosters
                .insert(identity, ordered_proposal_ids);
        }

        pub(in crate::chain_submission) fn require_admission_identity_locks(
            &self,
            identities: Vec<ChainSubmissionIdentity>,
        ) {
            *self.required_admission_locks.lock().unwrap() = Some(identities);
        }

        pub(in crate::chain_submission) fn batch_roster_reads(&self) -> usize {
            self.batch_roster_reads.load(Ordering::SeqCst)
        }

        pub(in crate::chain_submission) fn seed_record(&self, record: StoredChainSubmission) {
            self.state
                .lock()
                .unwrap()
                .records
                .insert(record.identity().clone(), record);
        }

        pub(in crate::chain_submission) fn record(
            &self,
            identity: &ChainSubmissionIdentity,
        ) -> Option<StoredChainSubmission> {
            self.state.lock().unwrap().records.get(identity).cloned()
        }

        pub(in crate::chain_submission) fn projection(
            &self,
            identity: &ChainSubmissionIdentity,
        ) -> Option<super::super::ChainSubmissionConfirmation> {
            self.state
                .lock()
                .unwrap()
                .projections
                .get(identity)
                .cloned()
        }

        pub(in crate::chain_submission) fn fail_next_commit(&self) {
            self.state.lock().unwrap().fail_before_commit = true;
        }

        pub(in crate::chain_submission) fn fail_next_commit_without_state(&self) {
            self.state.lock().unwrap().fail_before_commit_without_state = true;
        }

        pub(in crate::chain_submission) fn fail_next_confirmation(&self) {
            self.fail_confirmation.store(true, Ordering::SeqCst);
        }

        pub(in crate::chain_submission) fn after_next_confirmation_validation(
            &self,
            hook: impl FnOnce() + Send + 'static,
        ) {
            *self.confirmation_validated_hook.lock().unwrap() = Some(Box::new(hook));
        }

        fn transact<R>(
            &self,
            failure_identity: Option<&ChainSubmissionIdentity>,
            operation: impl FnOnce(&mut MemoryState) -> Result<R, ChainSubmissionFailure>,
        ) -> Result<R, ChainSubmissionFailure> {
            let mut state = self.state.lock().map_err(|_| {
                ChainSubmissionFailure::without_state(
                    ChainSubmissionFailureKind::Storage,
                    "in-memory submission store is poisoned",
                )
            })?;
            let mut staged = state.clone();
            let result = operation(&mut staged)?;
            if state.fail_before_commit_without_state {
                state.fail_before_commit_without_state = false;
                return Err(ChainSubmissionFailure::without_state(
                    ChainSubmissionFailureKind::Storage,
                    "injected stateless submission transaction commit failure",
                ));
            }
            if state.fail_before_commit {
                state.fail_before_commit = false;
                let message = "injected submission transaction commit failure";
                return Err(
                    match failure_identity
                        .and_then(|identity| state.records.get(identity))
                        .map(StoredChainSubmission::durable_state)
                    {
                        Some(durable_state) => ChainSubmissionFailure::with_durable_state(
                            ChainSubmissionFailureKind::Storage,
                            durable_state,
                            message,
                        ),
                        None => ChainSubmissionFailure::without_state(
                            ChainSubmissionFailureKind::Storage,
                            message,
                        ),
                    },
                );
            }
            *state = staged;
            Ok(result)
        }

        fn derive(
            state: &MemoryState,
            request: &SubmissionDerivationRequest,
        ) -> Result<DerivedChainSubmission, ChainSubmissionFailure> {
            state
                .derivations
                .get(request.identity())
                .cloned()
                .ok_or_else(|| {
                    ChainSubmissionFailure::without_state(
                        ChainSubmissionFailureKind::InvalidInput,
                        "semantic generation inputs are unavailable",
                    )
                })
        }

        fn unresolved_bundle_predecessor<'a>(
            state: &'a MemoryState,
            requested: &ChainSubmissionIdentity,
        ) -> Option<&'a StoredChainSubmission> {
            state.records.values().find(|record| {
                record.identity() != requested
                    && BundleOperationKey::from_identity(record.identity())
                        == BundleOperationKey::from_identity(requested)
                    && match record.state() {
                        SubmissionRecordState::Rejected(_) => false,
                        // A confirmed predecessor is authoritative once its
                        // domain projection has been applied.
                        SubmissionRecordState::Confirmed(_) => {
                            !state.projections.contains_key(record.identity())
                        }
                        SubmissionRecordState::Submitting
                        | SubmissionRecordState::Tracking { .. }
                        | SubmissionRecordState::Recovering { .. } => true,
                    }
            })
        }

        /// A confirmed vote or batch has consumed the bundle's delegation
        /// output, so a new delegation generation is refused before derivation.
        fn delegation_is_superseded(
            state: &MemoryState,
            requested: &ChainSubmissionIdentity,
        ) -> bool {
            matches!(requested.target(), ChainSubmissionTarget::Delegation)
                && state.records.values().any(|successor| {
                    BundleOperationKey::from_identity(successor.identity())
                        == BundleOperationKey::from_identity(requested)
                        && matches!(
                            successor.identity().target(),
                            ChainSubmissionTarget::Vote { .. }
                                | ChainSubmissionTarget::VoteBatch { .. }
                        )
                        && matches!(successor.state(), SubmissionRecordState::Confirmed(_))
                })
        }

        fn candidate_owner<'a>(
            state: &'a MemoryState,
            requested: &ChainSubmissionIdentity,
            candidate: super::super::CandidateTransactionHash,
        ) -> Option<&'a StoredChainSubmission> {
            state.records.values().find(|record| {
                record.identity() != requested
                    && match record.state() {
                        SubmissionRecordState::Tracking {
                            candidate_transaction_hash,
                        } => *candidate_transaction_hash == candidate,
                        SubmissionRecordState::Recovering {
                            candidate_transaction_hash,
                            ..
                        } => *candidate_transaction_hash == Some(candidate),
                        SubmissionRecordState::Confirmed(confirmation) => {
                            confirmation.confirmation().transaction_hash() == Some(candidate)
                        }
                        _ => false,
                    }
            })
        }
    }

    impl ChainSubmissionStore for InMemoryChainSubmissionStore {
        fn coordination(&self) -> &SubmissionCoordination {
            &self.coordination
        }

        fn admit(
            &self,
            request: &StoreAdvancementRequest,
            work_allowed: bool,
            reservation_ordinal: u64,
            now: u64,
        ) -> Result<StoreAdmission, ChainSubmissionFailure> {
            request.validate_target()?;
            if work_allowed && reservation_ordinal == 0 {
                return Err(ChainSubmissionFailure::without_state(
                    ChainSubmissionFailureKind::InvariantViolation,
                    "a committed POST reservation ordinal must be nonzero",
                ));
            }
            let normalizes_abandoned_reservation = self
                .state
                .lock()
                .map_err(|_| {
                    ChainSubmissionFailure::without_state(
                        ChainSubmissionFailureKind::Storage,
                        "in-memory submission store is poisoned",
                    )
                })?
                .records
                .get(request.identity())
                .is_some_and(|record| matches!(record.state(), SubmissionRecordState::Submitting));
            let required_admission_locks = self.required_admission_locks.lock().unwrap().take();

            self.transact(Some(request.identity()), |state| {
                if let Some(required) = &required_admission_locks {
                    if !self.coordination.identity_locks_are_held(required)? {
                        return Err(ChainSubmissionFailure::without_state(
                            ChainSubmissionFailureKind::InvariantViolation,
                            "admission began without every required submission-identity lock",
                        ));
                    }
                }
                let existing = state.records.get(request.identity()).cloned();
                if !work_allowed {
                    if let Some(mut record) = existing {
                        if matches!(record.state, SubmissionRecordState::Submitting) {
                            record.state = apply_submission_observation(
                                Some(record.state),
                                SubmissionObservation::AbandonedSubmitting(abandoned_diagnostic()),
                            )
                            .map_err(|error| {
                                transition_failure(ChainSubmissionState::Submitting, error)
                            })?
                            .expect("abandoned reservation remains durable");
                            record.diagnostic = match &record.state {
                                SubmissionRecordState::Recovering {
                                    ambiguity_diagnostic,
                                    ..
                                } => Some(ambiguity_diagnostic.clone()),
                                _ => None,
                            };
                            record.updated_at = now;
                            state
                                .records
                                .insert(request.identity().clone(), record.clone());
                        }
                        return Ok(StoreAdmission::Authoritative(record));
                    }
                    return Ok(StoreAdmission::NoAuthoritativeState);
                }

                if request.is_batch() {
                    self.batch_roster_reads.fetch_add(1, Ordering::SeqCst);
                    let authoritative_roster = state
                        .batch_rosters
                        .get(request.identity())
                        .ok_or_else(|| {
                            preserve_loaded_state(
                                ChainSubmissionFailure::without_state(
                                    ChainSubmissionFailureKind::InvalidInput,
                                    "complete persisted vote-batch membership is unavailable",
                                ),
                                existing.as_ref(),
                            )
                        })?;
                    request
                        .verify_batch_roster(authoritative_roster)
                        .map_err(|error| preserve_loaded_state(error, existing.as_ref()))?;
                }

                if let Some(mut record) = existing {
                    if matches!(record.state, SubmissionRecordState::Submitting) {
                        record.state = apply_submission_observation(
                            Some(record.state),
                            SubmissionObservation::AbandonedSubmitting(abandoned_diagnostic()),
                        )
                        .map_err(|error| {
                            transition_failure(ChainSubmissionState::Submitting, error)
                        })?
                        .expect("abandoned reservation remains durable");
                        record.diagnostic = match &record.state {
                            SubmissionRecordState::Recovering {
                                ambiguity_diagnostic,
                                ..
                            } => Some(ambiguity_diagnostic.clone()),
                            _ => record.diagnostic,
                        };
                        record.updated_at = now;
                        state
                            .records
                            .insert(request.identity().clone(), record.clone());
                        return Ok(StoreAdmission::Authoritative(record));
                    }
                    let derived = Self::derive(state, request.derivation()).map_err(|error| {
                        ChainSubmissionFailure::with_durable_state(
                            error.kind(),
                            record.durable_state(),
                            error.message(),
                        )
                    })?;
                    request
                        .verify_batch_roster(derived.ordered_proposal_ids())
                        .map_err(|error| preserve_loaded_state(error, Some(&record)))?;
                    ensure_generation(&record, derived.generation())?;
                    if matches!(
                        record.state,
                        SubmissionRecordState::Confirmed(_) | SubmissionRecordState::Rejected(_)
                    ) {
                        return Ok(StoreAdmission::Authoritative(record));
                    }
                    return Ok(StoreAdmission::Ready {
                        derived: Box::new(derived),
                        record,
                        fresh_reservation: false,
                    });
                }

                if Self::unresolved_bundle_predecessor(state, request.identity()).is_some() {
                    return Err(ChainSubmissionFailure::without_state(
                        ChainSubmissionFailureKind::InvalidInput,
                        "another submission for this bundle has not established an authoritative successor",
                    ));
                }
                if Self::delegation_is_superseded(state, request.identity()) {
                    return Err(ChainSubmissionFailure::without_state(
                        ChainSubmissionFailureKind::InvalidInput,
                        "a confirmed vote already succeeds this bundle's delegation",
                    ));
                }

                let derived = Self::derive(state, request.derivation())?;
                request.verify_batch_roster(derived.ordered_proposal_ids())?;
                if request.is_imported_delegation() {
                    let candidate = derived.imported_candidate().ok_or_else(|| {
                        ChainSubmissionFailure::without_state(
                            ChainSubmissionFailureKind::InvariantViolation,
                            "imported delegation derivation omitted its transaction hash",
                        )
                    })?;
                    if Self::candidate_owner(state, request.identity(), candidate).is_some() {
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
                    state
                        .records
                        .insert(request.identity().clone(), record.clone());
                    return Ok(StoreAdmission::Ready {
                        derived: Box::new(derived),
                        record,
                        fresh_reservation: false,
                    });
                }
                let record = StoredChainSubmission::fresh(
                    derived.generation(),
                    reservation_ordinal,
                    now,
                );
                state
                    .records
                    .insert(request.identity().clone(), record.clone());
                Ok(StoreAdmission::Ready {
                    derived: Box::new(derived),
                    record,
                    fresh_reservation: true,
                })
            })
            .map_err(|error| {
                if normalizes_abandoned_reservation {
                    ChainSubmissionFailure::with_known_possible_dispatch(
                        error.kind(),
                        error.message(),
                    )
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
            self.transact(Some(generation.identity()), |state| {
                let record = state.records.get(generation.identity()).ok_or_else(|| {
                    ChainSubmissionFailure::without_state(
                        ChainSubmissionFailureKind::InvariantViolation,
                        "fresh submission reservation is missing",
                    )
                })?;
                ensure_generation(record, generation)?;
                if !matches!(record.state(), SubmissionRecordState::Submitting) {
                    return Err(transition_failure(
                        record.durable_state(),
                        "only a fresh Submitting reservation can be removed",
                    ));
                }
                state.records.remove(generation.identity());
                Ok(())
            })
        }

        fn classify_post(
            &self,
            generation: &ChainSubmissionGeneration,
            observation: SubmissionObservation,
            now: u64,
        ) -> Result<StoredChainSubmission, ChainSubmissionFailure> {
            self.transact(Some(generation.identity()), |state| {
                let mut record = state
                    .records
                    .get(generation.identity())
                    .cloned()
                    .ok_or_else(|| {
                        ChainSubmissionFailure::without_state(
                            ChainSubmissionFailureKind::InvariantViolation,
                            "submission reservation disappeared before classification",
                        )
                })?;
                ensure_generation(&record, generation)?;
                let previous_state = record.durable_state();
                let observation = match observation {
                    SubmissionObservation::UsableCandidateHash(candidate)
                        if Self::candidate_owner(state, generation.identity(), candidate)
                            .is_some() =>
                    {
                        SubmissionObservation::PossiblyDispatched(
                            ChainSubmissionDiagnostic::from_redacted_message(
                                ChainSubmissionDiagnosticKind::InvalidProtocolResponse,
                                "vote-chain returned a transaction hash already bound to another semantic generation",
                            ),
                        )
                    }
                    observation => observation,
                };
                record.state = apply_submission_observation(Some(record.state), observation)
                    .map_err(|error| transition_failure(previous_state, error))?
                    .ok_or_else(|| {
                        transition_failure(
                            previous_state,
                            "classification unexpectedly removed row",
                        )
                    })?;
                if matches!(record.state, SubmissionRecordState::Tracking { .. })
                    && record.tracking_started_at.is_none()
                {
                    record.tracking_started_at = Some(now);
                }
                record.diagnostic = match &record.state {
                    SubmissionRecordState::Recovering {
                        ambiguity_diagnostic,
                        ..
                    }
                    | SubmissionRecordState::Rejected(ambiguity_diagnostic) => {
                        Some(ambiguity_diagnostic.clone())
                    }
                    _ => record.diagnostic,
                };
                record.updated_at = now;
                state
                    .records
                    .insert(generation.identity().clone(), record.clone());
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
            self.transact(Some(generation.identity()), |state| {
                let mut record = state
                    .records
                    .get(generation.identity())
                    .cloned()
                    .ok_or_else(|| {
                        ChainSubmissionFailure::without_state(
                            ChainSubmissionFailureKind::InvariantViolation,
                            "submission disappeared during reconciliation",
                        )
                    })?;
                ensure_generation(&record, generation)?;
                let previous_state = record.durable_state();
                record.state = apply_submission_observation(Some(record.state), observation)
                    .map_err(|error| transition_failure(previous_state, error))?
                    .expect("reconciliation cannot remove a row");
                if let Some(diagnostic) = diagnostic {
                    record.diagnostic = Some(diagnostic);
                }
                record.updated_at = now;
                state
                    .records
                    .insert(generation.identity().clone(), record.clone());
                Ok(record)
            })
        }

        fn confirm_committed(
            &self,
            request: &StoreAdvancementRequest,
            expected_generation: &ChainSubmissionGeneration,
            candidate: super::super::CandidateTransactionHash,
            committed: &CommittedTransaction,
            commit_allowed: &dyn Fn() -> bool,
            now: u64,
        ) -> Result<ConfirmationCommit, ChainSubmissionFailure> {
            self.transact(Some(expected_generation.identity()), |state| {
                let record = state
                    .records
                    .get(expected_generation.identity())
                    .cloned()
                    .ok_or_else(|| {
                        ChainSubmissionFailure::without_state(
                            ChainSubmissionFailureKind::InvariantViolation,
                            "submission disappeared before confirmation",
                        )
                    })?;
                ensure_generation(&record, expected_generation)?;
                let derived = Self::derive(state, request.derivation()).map_err(|error| {
                    ChainSubmissionFailure::with_durable_state(
                        error.kind(),
                        record.durable_state(),
                        error.message(),
                    )
                })?;
                if Self::candidate_owner(state, expected_generation.identity(), candidate).is_some()
                {
                    return Err(ChainSubmissionFailure::with_durable_state(
                        ChainSubmissionFailureKind::Protocol,
                        record.durable_state(),
                        "candidate transaction hash is bound to another semantic generation",
                    ));
                }
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
                if let Some(hook) = self.confirmation_validated_hook.lock().unwrap().take() {
                    hook();
                }
                if self.fail_confirmation.swap(false, Ordering::SeqCst) {
                    return Err(ChainSubmissionFailure::without_state(
                        ChainSubmissionFailureKind::Storage,
                        "injected atomic confirmation failure",
                    ));
                }
                let previous_state = record.durable_state();
                let next_state = apply_submission_observation(
                    Some(record.state.clone()),
                    SubmissionObservation::Confirmed(confirmation.clone()),
                )
                .map_err(|error| transition_failure(previous_state, error))?
                .expect("confirmation remains durable");
                let public_confirmation = confirmation.confirmation().clone();
                let mut confirmed = record;
                confirmed.state = next_state;
                confirmed.diagnostic = None;
                confirmed.updated_at = now;
                state
                    .projections
                    .insert(expected_generation.identity().clone(), public_confirmation);
                state
                    .records
                    .insert(expected_generation.identity().clone(), confirmed.clone());
                Ok(ConfirmationCommit::Confirmed(confirmed))
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
            self.transact(Some(expected_generation.identity()), |state| {
                let record = state
                    .records
                    .get(expected_generation.identity())
                    .cloned()
                    .ok_or_else(|| {
                        ChainSubmissionFailure::without_state(
                            ChainSubmissionFailureKind::InvariantViolation,
                            "submission disappeared before tree confirmation",
                        )
                    })?;
                ensure_generation(&record, expected_generation)?;
                let derived = Self::derive(state, request.derivation())
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
                let confirmation = ValidatedChainSubmissionConfirmation::from_tree(
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
                if self.fail_confirmation.swap(false, Ordering::SeqCst) {
                    return Err(ChainSubmissionFailure::without_state(
                        ChainSubmissionFailureKind::Storage,
                        "injected atomic confirmation failure",
                    ));
                }
                let previous = record.durable_state();
                let mut confirmed = record;
                confirmed.state = apply_submission_observation(
                    Some(confirmed.state),
                    SubmissionObservation::Confirmed(confirmation.clone()),
                )
                .map_err(|error| transition_failure(previous, error))?
                .expect("tree confirmation remains durable");
                confirmed.diagnostic = None;
                confirmed.updated_at = now.max(confirmed.updated_at);
                state.projections.insert(
                    expected_generation.identity().clone(),
                    confirmation.confirmation().clone(),
                );
                state
                    .records
                    .insert(expected_generation.identity().clone(), confirmed.clone());
                Ok(ConfirmationCommit::Confirmed(confirmed))
            })
        }

        fn reserve_recovery_retry(
            &self,
            request: &StoreAdvancementRequest,
            authorization: RecoveryRetryAuthorization<'_>,
            now: u64,
        ) -> Result<StoredChainSubmission, ChainSubmissionFailure> {
            let identity = authorization.operation().identity().clone();
            self.transact(Some(&identity), |state| {
                if request.identity() != &identity {
                    return Err(ChainSubmissionFailure::without_state(
                        ChainSubmissionFailureKind::InvalidInput,
                        "recovery request does not match its authorization identity",
                    ));
                }
                let mut record = state.records.get(&identity).cloned().ok_or_else(|| {
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
                let derived = Self::derive(state, request.derivation())
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
                state.records.insert(identity.clone(), record.clone());
                Ok(record)
            })
        }
    }
}
