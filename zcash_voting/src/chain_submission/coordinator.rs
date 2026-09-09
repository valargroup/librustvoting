//! One bounded, state-driven advancement of chain submission.

use std::{sync::Arc, time::Duration};

use super::{
    client::ChainRecoveryMode,
    coordination::{CapturedSubmissionOperation, SubmissionOperationLease},
    generation::{ChainSubmissionRequest, DerivedChainSubmission},
    protocol::{
        ChainProtocolClient, ChainRejectionKind, LookupFailure, PostAttemptOutcome,
        TransactionStatusObservation,
    },
    recovery::{
        scan_exact_layout, RecoveryRetryAuthorization, RecoveryScanFailure, RecoveryScanOutcome,
    },
    state::{SubmissionObservation, SubmissionRecordState},
    store::{
        ChainSubmissionStore, ConfirmationCommit, StoreAdmission, StoreAdvancementRequest,
        StoredChainSubmission,
    },
    ChainPostDispatch, ChainSubmissionDiagnostic, ChainSubmissionDiagnosticKind,
    ChainSubmissionFailure, ChainSubmissionFailureKind, ChainSubmissionResult,
    ChainSubmissionState, ChainTransport,
};

const MAX_POST_ATTEMPTS_PER_PASS: usize = 8;
const MAX_RETRY_BACKOFF: Duration = Duration::from_secs(10 * 60);
const CONTROL_CHECK_INTERVAL: Duration = Duration::from_millis(25);

/// Finite policy for one bounded coordinator instance.
#[derive(Clone)]
pub(super) struct CoordinatorPolicy {
    tracking_window_seconds: u64,
    maximum_post_attempts: usize,
    retry_backoffs: Vec<Duration>,
}

impl CoordinatorPolicy {
    pub(super) fn new(
        tracking_window: Duration,
        maximum_post_attempts: usize,
        retry_backoffs: Vec<Duration>,
    ) -> Result<Self, ChainSubmissionFailure> {
        let tracking_window_seconds = tracking_window.as_secs();
        if tracking_window_seconds == 0
            || tracking_window != Duration::from_secs(tracking_window_seconds)
            || maximum_post_attempts == 0
            || maximum_post_attempts > MAX_POST_ATTEMPTS_PER_PASS
            || retry_backoffs.len() + 1 != maximum_post_attempts
            || retry_backoffs
                .iter()
                .any(|delay| delay.is_zero() || *delay > MAX_RETRY_BACKOFF)
        {
            return Err(ChainSubmissionFailure::without_state(
                ChainSubmissionFailureKind::InvalidInput,
                "chain coordinator requires a nonzero whole-second tracking window, one to eight attempts, and one bounded nonzero backoff between attempts",
            ));
        }
        Ok(Self {
            tracking_window_seconds,
            maximum_post_attempts,
            retry_backoffs,
        })
    }
}

/// Durable wall-clock source. Tests substitute a restart-stable manual clock.
pub(super) trait ChainSubmissionClock: Send + Sync {
    fn now_seconds(&self) -> Result<u64, ChainSubmissionFailure>;
}

pub(super) struct SystemChainSubmissionClock;

impl ChainSubmissionClock for SystemChainSubmissionClock {
    fn now_seconds(&self) -> Result<u64, ChainSubmissionFailure> {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .map_err(|_| {
                ChainSubmissionFailure::without_state(
                    ChainSubmissionFailureKind::Storage,
                    "system clock is before the Unix epoch",
                )
            })
    }
}

/// Host cancellation and operation-epoch authority sampled at every boundary.
pub(super) trait SubmissionControl: Send + Sync {
    fn observations(&self) -> crate::ObservationScope {
        crate::ObservationScope::disabled()
    }

    fn is_cancelled(&self) -> bool;
    fn operation_epoch(&self) -> u64;
}

/// Internal lifecycle engine backing the public chain-submission client.
pub(super) struct ChainSubmissionCoordinator<T, S, C> {
    protocol: ChainProtocolClient<T>,
    store: Arc<S>,
    clock: C,
    policy: CoordinatorPolicy,
}

struct ConfirmationContext<'a> {
    request: &'a StoreAdvancementRequest,
    operation: &'a CapturedSubmissionOperation,
    derived: &'a DerivedChainSubmission,
    candidate: super::CandidateTransactionHash,
    committed: &'a super::protocol::CommittedTransaction,
    durable_state: ChainSubmissionState,
    control: &'a dyn SubmissionControl,
}

impl<T, S, C> ChainSubmissionCoordinator<T, S, C>
where
    T: ChainTransport,
    S: ChainSubmissionStore,
    C: ChainSubmissionClock,
{
    pub(super) fn new(
        protocol: ChainProtocolClient<T>,
        store: Arc<S>,
        clock: C,
        policy: CoordinatorPolicy,
    ) -> Result<Self, ChainSubmissionFailure> {
        Ok(Self {
            protocol,
            store,
            clock,
            policy,
        })
    }

    /// Advances a delegation or singleton vote by one bounded pass.
    pub(super) async fn advance(
        &self,
        request: StoreAdvancementRequest,
        control: &dyn SubmissionControl,
    ) -> Result<ChainSubmissionResult, ChainSubmissionFailure> {
        self.advance_with_recovery(request, ChainRecoveryMode::StatusOnly, control)
            .await
    }

    pub(super) async fn advance_with_recovery(
        &self,
        request: StoreAdvancementRequest,
        recovery: ChainRecoveryMode,
        control: &dyn SubmissionControl,
    ) -> Result<ChainSubmissionResult, ChainSubmissionFailure> {
        let operation_epoch = control.operation_epoch();
        self.advance_in_epoch(request, recovery, control, operation_epoch)
            .await
    }

    /// One bounded pass for work that began earlier under `operation_epoch`.
    ///
    /// The captured operation carries that epoch rather than the control's
    /// current one, so a host epoch change between the caller's own check and
    /// this pass is observed by the coordinator's interruption checks instead
    /// of being adopted as the pass's own epoch.
    pub(super) async fn advance_in_epoch(
        &self,
        request: StoreAdvancementRequest,
        recovery: ChainRecoveryMode,
        control: &dyn SubmissionControl,
        operation_epoch: u64,
    ) -> Result<ChainSubmissionResult, ChainSubmissionFailure> {
        let operation =
            CapturedSubmissionOperation::new(request.identity().clone(), operation_epoch);
        let applicable_identities = request.applicable_identities();
        let lease = self
            .store
            .coordination()
            .acquire(&operation, &applicable_identities)
            .await?;

        let work_allowed = interruption(&operation, control).is_none();
        let admission = self
            .store
            .admit(&request, work_allowed, 1, self.clock.now_seconds()?)?;
        match admission {
            StoreAdmission::NoAuthoritativeState => match interruption(&operation, control) {
                Some(Interruption::Cancelled) => Ok(ChainSubmissionResult::Cancelled),
                Some(Interruption::StaleEpoch) => Err(ChainSubmissionFailure::without_state(
                    ChainSubmissionFailureKind::InvalidInput,
                    "host operation epoch changed before chain submission",
                )),
                None => Err(ChainSubmissionFailure::without_state(
                    ChainSubmissionFailureKind::InvariantViolation,
                    "admission returned no state for an active operation",
                )),
            },
            StoreAdmission::Authoritative(record) => record.public_result(),
            StoreAdmission::Ready {
                derived,
                record,
                fresh_reservation,
            } if fresh_reservation => {
                self.submit_reserved_attempts(
                    &request, &operation, &lease, *derived, record, recovery, 0, control,
                )
                .await
            }
            StoreAdmission::Ready {
                derived, record, ..
            } => {
                // Status-only advancement may re-POST a dispatch-ambiguity
                // row directly. Exact-tree advancement falls through to
                // reconciliation, which scans the tree before any POST so a
                // generation that already landed is confirmed rather than
                // redispatched into a "nullifier already spent" rejection.
                if recovery == ChainRecoveryMode::StatusOnly
                    && is_retryable_dispatch_ambiguity(&record)
                    && !request.is_imported_delegation()
                    && interruption(&operation, control).is_none()
                {
                    let now = self.clock.now_seconds().map_err(|error| {
                        ChainSubmissionFailure::with_durable_state(
                            error.kind(),
                            record.durable_state(),
                            error.message(),
                        )
                    })?;
                    let reserved = self
                        .store
                        .reserve_ambiguous_retry(derived.generation(), now)
                        .map_err(|error| {
                            if error.strongest_state().is_some() {
                                error
                            } else {
                                ChainSubmissionFailure::with_durable_state(
                                    error.kind(),
                                    record.durable_state(),
                                    error.message(),
                                )
                            }
                        })?;
                    return self
                        .submit_reserved_attempts(
                            &request, &operation, &lease, *derived, reserved, recovery, 0, control,
                        )
                        .await;
                }
                self.reconcile_existing(
                    &request, &operation, &lease, *derived, record, recovery, 0, control,
                )
                .await
            }
        }
    }

    /// Runs the bounded POST loop for a generation whose next attempt is
    /// already durably reserved.
    ///
    /// `first_attempt_index` is the number of POST attempts this invocation
    /// has already consumed; the loop runs the remaining budget. A fresh
    /// `Submitting` reservation enters with index 0, as does a hashless
    /// dispatch-ambiguity row reserved directly under status-only advancement.
    /// A row whose exact-tree recovery retry was itself ambiguous enters later
    /// and continues with the same backoff and reservation discipline.
    ///
    /// Exhausting the budget never ends the generation: the row stays hashless
    /// `Recovering` with its last attempt's dispatch diagnostic, and a later
    /// invocation receives a fresh budget. Only chain rejection code 2 after
    /// unresolved dispatch is terminal, and under exact-tree advancement one
    /// tree pass precedes even that.
    #[allow(
        clippy::too_many_arguments,
        reason = "the attempt loop keeps each captured authority explicit"
    )]
    async fn submit_reserved_attempts(
        &self,
        request: &StoreAdvancementRequest,
        operation: &CapturedSubmissionOperation,
        lease: &SubmissionOperationLease,
        mut derived: DerivedChainSubmission,
        mut reserved: StoredChainSubmission,
        recovery: ChainRecoveryMode,
        first_attempt_index: usize,
        control: &dyn SubmissionControl,
    ) -> Result<ChainSubmissionResult, ChainSubmissionFailure> {
        let observations = control.observations();

        let mut ambiguity_seen = is_retryable_dispatch_ambiguity(&reserved);
        let mut refusing_endpoints = std::collections::BTreeSet::new();
        // A router refusal is an immediate answer from a node that will never
        // serve the route, not congestion, so rotating to the next node waits
        // for nothing.
        let mut rotate_without_backoff = false;
        for attempt_index in first_attempt_index..self.policy.maximum_post_attempts {
            let observations = observations
                .attempt(u32::try_from(attempt_index.saturating_add(1)).unwrap_or(u32::MAX));
            if let Some(reason) = interruption(operation, control) {
                if ambiguity_seen {
                    return reserved.public_result();
                }
                self.remove_fresh_reservation(derived.generation())?;
                return interrupted_without_state(reason);
            }

            let _in_flight = match self
                .store
                .coordination()
                .register_in_flight(derived.generation().identity())
            {
                Ok(in_flight) => in_flight,
                Err(error) => {
                    if !ambiguity_seen {
                        self.remove_fresh_reservation(derived.generation())?;
                    }
                    return Err(error);
                }
            };
            let ordinal =
                usize::try_from(reserved.committed_post_reservations()).unwrap_or(usize::MAX);
            let endpoint_index = ordinal.saturating_sub(1) % self.protocol.endpoint_count();
            let outcome = {
                let dispatch = ChainPostDispatch::default();
                let post = self.submit_to_endpoint(
                    endpoint_index,
                    &derived,
                    dispatch.clone(),
                    &observations,
                );
                tokio::pin!(post);
                tokio::select! {
                    biased;
                    reason = wait_for_interruption(operation, control) => {
                        if !dispatch.is_possible() {
                            if ambiguity_seen {
                                return reserved.public_result();
                            }
                            self.remove_fresh_reservation(derived.generation())?;
                            return interrupted_without_state(reason);
                        }
                        let diagnostic = ChainSubmissionDiagnostic::from_redacted_message(
                            ChainSubmissionDiagnosticKind::AmbiguousDispatch,
                            reason.after_dispatch_message(),
                        );
                        PostAttemptOutcome::PossiblyDispatched(diagnostic)
                    }
                    outcome = &mut post => outcome,
                }
            };

            match outcome {
                PostAttemptOutcome::Accepted(candidate) => {
                    let record = self.classify_dispatched_post(
                        derived.generation(),
                        SubmissionObservation::UsableCandidateHash(candidate),
                    )?;
                    if returned_candidate_is_unusable(&record) {
                        reserved = record;
                        ambiguity_seen = true;
                    } else {
                        return self
                            .reconcile_existing(
                                request,
                                operation,
                                lease,
                                derived,
                                record,
                                recovery,
                                attempt_index + 1,
                                control,
                            )
                            .await;
                    }
                }
                PostAttemptOutcome::Rejected {
                    kind, diagnostic, ..
                } => {
                    if ambiguity_seen && kind == ChainRejectionKind::NullifierAlreadySpent {
                        return self
                            .settle_nullifier_spent_after_dispatch(
                                request, operation, lease, &derived, &reserved, recovery, control,
                            )
                            .await;
                    }
                    // A combined delegation-and-cast batch rejected on its
                    // first POST, with no attempt possibly dispatched, is
                    // chain evidence that nothing landed: the store retires
                    // the generation and frees the delegation for a fresh
                    // batch. A code-2 rejection is excluded because it says
                    // the delegation notes are spent by something else, which
                    // only tree recovery can explain, and a recast would fail
                    // the same way. Standalone generations keep the recoverable
                    // classification: their members stay locked until the
                    // ballot decides otherwise.
                    let terminal = !ambiguity_seen
                        && derived.generation().identity().target().is_combined()
                        && kind != ChainRejectionKind::NullifierAlreadySpent;
                    let observation = if terminal {
                        SubmissionObservation::TerminalRejection(diagnostic.clone())
                    } else {
                        SubmissionObservation::DefiniteRejection(diagnostic.clone())
                    };
                    let record =
                        self.classify_dispatched_post(derived.generation(), observation)?;
                    if ambiguity_seen {
                        return Err(ChainSubmissionFailure::with_durable_state(
                            ChainSubmissionFailureKind::Protocol,
                            record.durable_state(),
                            diagnostic.message(),
                        ));
                    }
                    return record.public_result();
                }
                PostAttemptOutcome::PossiblyDispatched(diagnostic) => {
                    reserved = self.classify_dispatched_post(
                        derived.generation(),
                        SubmissionObservation::PossiblyDispatched(diagnostic),
                    )?;
                    ambiguity_seen = true;
                    if interruption(operation, control).is_some() {
                        return reserved.public_result();
                    }
                }
                PostAttemptOutcome::LocalFailure(diagnostic) => {
                    if !ambiguity_seen {
                        self.remove_fresh_reservation(derived.generation())?;
                        return Err(ChainSubmissionFailure::without_state(
                            ChainSubmissionFailureKind::Protocol,
                            diagnostic.message(),
                        ));
                    }
                    return Err(ChainSubmissionFailure::with_durable_state(
                        ChainSubmissionFailureKind::Protocol,
                        ChainSubmissionState::Recovering,
                        diagnostic.message(),
                    ));
                }
                PostAttemptOutcome::EndpointUnsupported(diagnostic) => {
                    // The vote-chain router answered in its own error
                    // envelope, so nothing decoded the body and nothing was
                    // dispatched: the fresh reservation is released exactly as
                    // for a definitely-unsent attempt. Another configured node
                    // may still serve the route, so the remaining attempts
                    // rotate on; once every node has refused, or the budget is
                    // spent, the invocation stops instead of backing off
                    // against an answer that cannot change. The failure is a
                    // protocol failure, not a transport one: the network works,
                    // the node speaks an older protocol.
                    refusing_endpoints.insert(endpoint_index);
                    rotate_without_backoff = true;
                    if ambiguity_seen {
                        reserved = self.reconcile_with_durable_state(
                            derived.generation(),
                            SubmissionObservation::DefinitelyUnsent,
                            Some(ChainSubmissionDiagnostic::from_redacted_message(
                                ChainSubmissionDiagnosticKind::ReconciliationPending,
                                diagnostic.message(),
                            )),
                            ChainSubmissionState::Recovering,
                        )?;
                    } else {
                        self.remove_fresh_reservation(derived.generation())?;
                    }
                    let every_node_refused =
                        refusing_endpoints.len() >= self.protocol.endpoint_count();
                    if every_node_refused || attempt_index + 1 == self.policy.maximum_post_attempts
                    {
                        return Err(if ambiguity_seen {
                            ChainSubmissionFailure::with_durable_state(
                                ChainSubmissionFailureKind::Protocol,
                                ChainSubmissionState::Recovering,
                                diagnostic.message(),
                            )
                        } else {
                            ChainSubmissionFailure::without_state(
                                ChainSubmissionFailureKind::Protocol,
                                diagnostic.message(),
                            )
                        });
                    }
                }
                PostAttemptOutcome::DefinitelyUnsent(error) => {
                    if ambiguity_seen {
                        reserved = self.reconcile_with_durable_state(
                            derived.generation(),
                            SubmissionObservation::DefinitelyUnsent,
                            Some(ChainSubmissionDiagnostic::from_redacted_message(
                                ChainSubmissionDiagnosticKind::ReconciliationPending,
                                error.message(),
                            )),
                            ChainSubmissionState::Recovering,
                        )?;
                    } else {
                        self.remove_fresh_reservation(derived.generation())?;
                    }
                    if attempt_index + 1 == self.policy.maximum_post_attempts {
                        return Err(if ambiguity_seen {
                            ChainSubmissionFailure::with_durable_state(
                                ChainSubmissionFailureKind::Transport,
                                ChainSubmissionState::Recovering,
                                error.message(),
                            )
                        } else {
                            ChainSubmissionFailure::without_state(
                                ChainSubmissionFailureKind::Transport,
                                error.message(),
                            )
                        });
                    }
                }
            }

            // Every non-ambiguous arm returned above on a final attempt. An
            // ambiguous final attempt leaves the durable ambiguity for a later
            // invocation; it must not reach the backoff table, which has one
            // entry fewer than the attempt budget.
            if attempt_index + 1 == self.policy.maximum_post_attempts {
                return reserved.public_result();
            }
            // A rotation past an answer that cannot change waits for nothing,
            // whether or not the row already carries dispatch ambiguity.
            let backoff = if std::mem::take(&mut rotate_without_backoff) {
                Duration::ZERO
            } else {
                self.policy.retry_backoffs[attempt_index]
            };
            if ambiguity_seen {
                reserved = match self
                    .reserve_ambiguous_retry_after_backoff(
                        backoff,
                        derived.generation(),
                        reserved,
                        operation,
                        control,
                    )
                    .await?
                {
                    AmbiguousRetryReservation::Reserved(reserved) => reserved,
                    AmbiguousRetryReservation::Interrupted(reserved) => {
                        return reserved.public_result()
                    }
                };
            } else {
                if let Some(reason) = self
                    .wait_backoff_or_interruption(backoff, operation, control)
                    .await
                {
                    return interrupted_without_state(reason);
                }
                match self.store.admit(
                    request,
                    true,
                    u64::try_from(attempt_index + 2).expect("bounded attempt index fits u64"),
                    self.clock.now_seconds()?,
                )? {
                    StoreAdmission::Ready {
                        derived: retry,
                        record,
                        fresh_reservation: true,
                    } if retry.generation() == derived.generation() => {
                        derived = *retry;
                        reserved = record;
                    }
                    StoreAdmission::Ready { record, .. }
                    | StoreAdmission::Authoritative(record) => return record.public_result(),
                    _ => {
                        return Err(ChainSubmissionFailure::without_state(
                            ChainSubmissionFailureKind::InvariantViolation,
                            "definitely-unsent retry did not reserve the same generation",
                        ));
                    }
                }
            }
        }
        unreachable!("validated attempt bound is nonzero")
    }

    /// Waits `backoff`, then durably reserves the next same-generation POST
    /// for a hashless row that already carries dispatch ambiguity.
    ///
    /// The caller chooses the wait so that a rotation past an answer that
    /// cannot change (a router refusal) passes `Duration::ZERO`.
    ///
    /// Interruption during the backoff leaves the row as it is; the durable
    /// ambiguity is returned so the caller can report it.
    async fn reserve_ambiguous_retry_after_backoff(
        &self,
        backoff: Duration,
        generation: &super::ChainSubmissionGeneration,
        reserved: StoredChainSubmission,
        operation: &CapturedSubmissionOperation,
        control: &dyn SubmissionControl,
    ) -> Result<AmbiguousRetryReservation, ChainSubmissionFailure> {
        if self
            .wait_backoff_or_interruption(backoff, operation, control)
            .await
            .is_some()
        {
            return Ok(AmbiguousRetryReservation::Interrupted(reserved));
        }
        let durable_state = reserved.durable_state();
        let attach_state = |error: ChainSubmissionFailure| {
            if error.strongest_state().is_some() {
                error
            } else {
                ChainSubmissionFailure::with_durable_state(
                    error.kind(),
                    durable_state,
                    error.message(),
                )
            }
        };
        let now = self.clock.now_seconds().map_err(attach_state)?;
        self.store
            .reserve_ambiguous_retry(generation, now)
            .map(AmbiguousRetryReservation::Reserved)
            .map_err(attach_state)
    }

    async fn submit_to_endpoint(
        &self,
        endpoint_index: usize,
        derived: &DerivedChainSubmission,
        dispatch: ChainPostDispatch,
        observations: &crate::ObservationScope,
    ) -> PostAttemptOutcome {
        match derived.request() {
            ChainSubmissionRequest::Delegation(submission) => {
                self.protocol
                    .submit_delegation_with_dispatch(
                        endpoint_index,
                        submission,
                        dispatch,
                        observations,
                    )
                    .await
            }
            ChainSubmissionRequest::ImportedDelegation(_) => {
                PostAttemptOutcome::LocalFailure(ChainSubmissionDiagnostic::from_redacted_message(
                    ChainSubmissionDiagnosticKind::InvalidProtocolResponse,
                    "a capability-imported delegation cannot be dispatched by the voter",
                ))
            }
            ChainSubmissionRequest::Vote(submission) => {
                self.protocol
                    .submit_vote_with_dispatch(endpoint_index, submission, dispatch, observations)
                    .await
            }
            ChainSubmissionRequest::DelegateAndVoteBatch(submission) => {
                self.protocol
                    .submit_delegate_and_vote_batch_with_dispatch(
                        endpoint_index,
                        submission,
                        derived
                            .generation()
                            .identity()
                            .target()
                            .batch_digest()
                            .expect("combined identity"),
                        dispatch,
                        observations,
                    )
                    .await
            }
            ChainSubmissionRequest::VoteBatch(submission) => {
                let super::ChainSubmissionTarget::VoteBatch {
                    ordered_batch_digest,
                } = derived.generation().identity().target()
                else {
                    unreachable!("vote-batch request has a vote-batch identity")
                };
                self.protocol
                    .submit_vote_batch_with_dispatch(
                        endpoint_index,
                        submission,
                        ordered_batch_digest,
                        dispatch,
                        observations,
                    )
                    .await
            }
        }
    }

    fn classify_dispatched_post(
        &self,
        generation: &super::ChainSubmissionGeneration,
        observation: SubmissionObservation,
    ) -> Result<StoredChainSubmission, ChainSubmissionFailure> {
        let now = self.clock.now_seconds().map_err(|error| {
            ChainSubmissionFailure::with_known_possible_dispatch(error.kind(), error.message())
        })?;
        self.store
            .classify_post(generation, observation, now)
            .map_err(|error| {
                ChainSubmissionFailure::with_known_possible_dispatch(error.kind(), error.message())
            })
    }

    fn remove_fresh_reservation(
        &self,
        generation: &super::ChainSubmissionGeneration,
    ) -> Result<(), ChainSubmissionFailure> {
        let now = self.clock.now_seconds().map_err(|error| {
            ChainSubmissionFailure::with_durable_state(
                error.kind(),
                ChainSubmissionState::Submitting,
                error.message(),
            )
        })?;
        self.store
            .remove_fresh_reservation(generation, now)
            .map_err(|error| {
                if error.strongest_state().is_some() {
                    error
                } else {
                    ChainSubmissionFailure::with_durable_state(
                        error.kind(),
                        ChainSubmissionState::Submitting,
                        error.message(),
                    )
                }
            })
    }

    fn reconcile_with_durable_state(
        &self,
        generation: &super::ChainSubmissionGeneration,
        observation: SubmissionObservation,
        diagnostic: Option<ChainSubmissionDiagnostic>,
        durable_state: ChainSubmissionState,
    ) -> Result<StoredChainSubmission, ChainSubmissionFailure> {
        let attach_state = |error: ChainSubmissionFailure| {
            if error.strongest_state().is_some() {
                error
            } else {
                ChainSubmissionFailure::with_durable_state(
                    error.kind(),
                    durable_state,
                    error.message(),
                )
            }
        };
        let now = self.clock.now_seconds().map_err(attach_state)?;
        self.store
            .reconcile(generation, observation, diagnostic, now)
            .map_err(attach_state)
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the lifecycle boundary keeps each captured authority explicit"
    )]
    async fn reconcile_existing(
        &self,
        request: &StoreAdvancementRequest,
        operation: &CapturedSubmissionOperation,
        lease: &SubmissionOperationLease,
        derived: DerivedChainSubmission,
        record: StoredChainSubmission,
        recovery: ChainRecoveryMode,
        post_attempts_used: usize,
        control: &dyn SubmissionControl,
    ) -> Result<ChainSubmissionResult, ChainSubmissionFailure> {
        match record.state() {
            SubmissionRecordState::Tracking {
                candidate_transaction_hash,
            } => {
                let candidate = *candidate_transaction_hash;
                if interruption(operation, control).is_some() {
                    return record.public_result();
                }
                match self
                    .lookup_or_interruption(operation, candidate, control)
                    .await
                {
                    LookupProgress::Interrupted => record.public_result(),
                    LookupProgress::Observed(TransactionStatusObservation::Pending) => {
                        let record = self.finish_inconclusive_tracking(&derived, record, None)?;
                        self.recover_if_enabled(
                            request,
                            operation,
                            lease,
                            derived,
                            record,
                            recovery,
                            post_attempts_used,
                            control,
                        )
                        .await
                    }
                    LookupProgress::Failed(failure) => {
                        if self.tracking_expired(&record)? {
                            let record = self.finish_inconclusive_tracking(
                                &derived,
                                record,
                                Some(lookup_diagnostic(&failure)),
                            )?;
                            self.recover_if_enabled(
                                request,
                                operation,
                                lease,
                                derived,
                                record,
                                recovery,
                                post_attempts_used,
                                control,
                            )
                            .await
                        } else {
                            self.reconcile_with_durable_state(
                                derived.generation(),
                                SubmissionObservation::CandidatePending,
                                Some(lookup_diagnostic(&failure)),
                                ChainSubmissionState::Tracking,
                            )?;
                            Err(lookup_failure(failure, ChainSubmissionState::Tracking))
                        }
                    }
                    LookupProgress::Observed(TransactionStatusObservation::CommittedFailure(_)) => {
                        let diagnostic = ChainSubmissionDiagnostic::from_redacted_message(
                            ChainSubmissionDiagnosticKind::ChainRejected,
                            "tracked vote-chain transaction committed unsuccessfully",
                        );
                        // An imported delegation has one immutable candidate,
                        // and a combined batch's candidate is the hash of its
                        // one signed envelope, so a committed failure of that
                        // hash is the generation's final word: no other
                        // dispatch of the same bytes can have landed.
                        let terminal = request.is_imported_delegation()
                            || derived.generation().identity().target().is_combined();
                        let observation = if terminal {
                            SubmissionObservation::TerminalCandidateFailure(diagnostic.clone())
                        } else {
                            SubmissionObservation::CandidateCommittedFailure(diagnostic.clone())
                        };
                        self.reconcile_with_durable_state(
                            derived.generation(),
                            observation,
                            Some(diagnostic),
                            ChainSubmissionState::Tracking,
                        )?
                        .public_result()
                    }
                    LookupProgress::Observed(TransactionStatusObservation::CommittedSuccess(
                        committed,
                    )) => self.confirm(ConfirmationContext {
                        request,
                        operation,
                        derived: &derived,
                        candidate,
                        committed: &committed,
                        durable_state: ChainSubmissionState::Tracking,
                        control,
                    }),
                }
            }
            SubmissionRecordState::Recovering {
                candidate_transaction_hash: None,
                ..
            } => {
                self.recover_if_enabled(
                    request,
                    operation,
                    lease,
                    derived,
                    record,
                    recovery,
                    post_attempts_used,
                    control,
                )
                .await
            }
            SubmissionRecordState::Recovering {
                candidate_transaction_hash: Some(candidate),
                ..
            } => {
                let candidate = *candidate;
                if interruption(operation, control).is_some() {
                    return record.public_result();
                }
                match self
                    .lookup_or_interruption(operation, candidate, control)
                    .await
                {
                    LookupProgress::Interrupted => record.public_result(),
                    LookupProgress::Observed(TransactionStatusObservation::CommittedSuccess(
                        committed,
                    )) => self.confirm(ConfirmationContext {
                        request,
                        operation,
                        derived: &derived,
                        candidate,
                        committed: &committed,
                        durable_state: ChainSubmissionState::Recovering,
                        control,
                    }),
                    LookupProgress::Observed(TransactionStatusObservation::CommittedFailure(_)) => {
                        // Same terminality rule as the `Tracking` arm above: an
                        // imported delegation has one immutable candidate, and a
                        // combined batch's candidate is the hash of its one
                        // signed envelope. A row reaches here rather than
                        // `Tracking` only because its tracking window expired
                        // inconclusively first, which changes nothing about
                        // whose bytes the chain just reported on.
                        let terminal = request.is_imported_delegation()
                            || derived.generation().identity().target().is_combined();
                        if terminal {
                            let diagnostic = ChainSubmissionDiagnostic::from_redacted_message(
                                ChainSubmissionDiagnosticKind::ChainRejected,
                                if request.is_imported_delegation() {
                                    "imported vote-chain transaction committed unsuccessfully"
                                } else {
                                    "combined vote-chain transaction committed unsuccessfully"
                                },
                            );
                            return self
                                .reconcile_with_durable_state(
                                    derived.generation(),
                                    SubmissionObservation::TerminalCandidateFailure(
                                        diagnostic.clone(),
                                    ),
                                    Some(diagnostic),
                                    ChainSubmissionState::Recovering,
                                )?
                                .public_result();
                        }
                        let record = self.reconcile_with_durable_state(
                            derived.generation(),
                            SubmissionObservation::CandidateCommittedFailure(
                                ChainSubmissionDiagnostic::from_redacted_message(
                                    ChainSubmissionDiagnosticKind::ChainRejected,
                                    "recovery candidate committed unsuccessfully",
                                ),
                            ),
                            None,
                            ChainSubmissionState::Recovering,
                        )?;
                        self.recover_if_enabled(
                            request,
                            operation,
                            lease,
                            derived,
                            record,
                            recovery,
                            post_attempts_used,
                            control,
                        )
                        .await
                    }
                    LookupProgress::Observed(TransactionStatusObservation::Pending) => {
                        let record = self.reconcile_with_durable_state(
                            derived.generation(),
                            SubmissionObservation::CandidatePending,
                            None,
                            ChainSubmissionState::Recovering,
                        )?;
                        self.recover_if_enabled(
                            request,
                            operation,
                            lease,
                            derived,
                            record,
                            recovery,
                            post_attempts_used,
                            control,
                        )
                        .await
                    }
                    LookupProgress::Failed(failure) => {
                        let record = self.reconcile_with_durable_state(
                            derived.generation(),
                            SubmissionObservation::ContinueRecovery,
                            Some(lookup_diagnostic(&failure)),
                            ChainSubmissionState::Recovering,
                        )?;
                        self.recover_if_enabled(
                            request,
                            operation,
                            lease,
                            derived,
                            record,
                            recovery,
                            post_attempts_used,
                            control,
                        )
                        .await
                    }
                }
            }
            _ => record.public_result(),
        }
    }

    fn finish_inconclusive_tracking(
        &self,
        derived: &DerivedChainSubmission,
        record: StoredChainSubmission,
        diagnostic: Option<ChainSubmissionDiagnostic>,
    ) -> Result<StoredChainSubmission, ChainSubmissionFailure> {
        let now = self.clock.now_seconds().map_err(|error| {
            ChainSubmissionFailure::with_durable_state(
                error.kind(),
                record.durable_state(),
                error.message(),
            )
        })?;
        let (observation, diagnostic) = if self.tracking_expired_at(&record, now)? {
            let expired = ChainSubmissionDiagnostic::from_redacted_message(
                ChainSubmissionDiagnosticKind::TrackingWindowExpired,
                "finite candidate tracking window expired without a definitive result",
            );
            (
                SubmissionObservation::TrackingWindowExpired(expired.clone()),
                Some(expired),
            )
        } else {
            (SubmissionObservation::CandidatePending, diagnostic)
        };
        self.reconcile_with_durable_state(
            derived.generation(),
            observation,
            diagnostic,
            record.durable_state(),
        )
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "recovery keeps the request, operation, lease, state, and host authority explicit"
    )]
    async fn recover_if_enabled(
        &self,
        request: &StoreAdvancementRequest,
        operation: &CapturedSubmissionOperation,
        lease: &SubmissionOperationLease,
        derived: DerivedChainSubmission,
        record: StoredChainSubmission,
        recovery: ChainRecoveryMode,
        post_attempts_used: usize,
        control: &dyn SubmissionControl,
    ) -> Result<ChainSubmissionResult, ChainSubmissionFailure> {
        // An imported delegation cannot be reconstructed or redispatched by
        // the voter, so it never scans; only its candidate hash is polled.
        if recovery == ChainRecoveryMode::StatusOnly
            || request.is_imported_delegation()
            || !matches!(record.state(), SubmissionRecordState::Recovering { .. })
        {
            return record.public_result();
        }
        if interruption(operation, control).is_some() {
            return record.public_result();
        }
        let candidate = match record.state() {
            SubmissionRecordState::Recovering {
                candidate_transaction_hash,
                ..
            } => *candidate_transaction_hash,
            _ => unreachable!("checked above"),
        };
        match self
            .scan_tree(&derived, candidate, operation, lease, control)
            .await?
        {
            RecoveryScan::Match {
                final_van_position,
                vote_commitment_positions,
            } => self.confirm_tree(
                request,
                operation,
                &derived,
                final_van_position,
                vote_commitment_positions,
                control,
            ),
            RecoveryScan::Interrupted => record.public_result(),
            RecoveryScan::NoMatch(authorization) => {
                if post_attempts_used >= self.policy.maximum_post_attempts {
                    return record.public_result();
                }
                if interruption(operation, control).is_some() {
                    return record.public_result();
                }
                if post_attempts_used > 0
                    && self
                        .wait_backoff_or_interruption(
                            self.policy.retry_backoffs[post_attempts_used - 1],
                            operation,
                            control,
                        )
                        .await
                        .is_some()
                {
                    return record.public_result();
                }
                if interruption(operation, control).is_some() {
                    return record.public_result();
                }
                let now = self.clock.now_seconds().map_err(|error| {
                    ChainSubmissionFailure::with_durable_state(
                        error.kind(),
                        ChainSubmissionState::Recovering,
                        error.message(),
                    )
                })?;
                let reserved = self
                    .store
                    .reserve_recovery_retry(request, authorization, now)
                    .map_err(|error| {
                        if error.strongest_state().is_some() {
                            error
                        } else {
                            ChainSubmissionFailure::with_durable_state(
                                error.kind(),
                                record.durable_state(),
                                error.message(),
                            )
                        }
                    })?;
                self.submit_recovery_retry(
                    request,
                    operation,
                    lease,
                    derived,
                    reserved,
                    recovery,
                    post_attempts_used,
                    control,
                )
                .await
            }
        }
    }

    /// Runs one bounded exact-tree pass for a durable `Recovering` row.
    ///
    /// A scan that cannot complete (malformed or contradictory tree data, a
    /// transport failure) records a bounded diagnostic on the row, leaves it
    /// `Recovering`, and surfaces an error; it never produces authorization or
    /// retires a candidate. Interruption is reported as such with no error.
    async fn scan_tree<'a>(
        &self,
        derived: &DerivedChainSubmission,
        candidate: Option<super::CandidateTransactionHash>,
        operation: &'a CapturedSubmissionOperation,
        lease: &'a SubmissionOperationLease,
        control: &dyn SubmissionControl,
    ) -> Result<RecoveryScan<'a>, ChainSubmissionFailure> {
        let observations = control.observations();

        match scan_exact_layout(
            &self.protocol,
            derived,
            candidate,
            operation,
            lease,
            || interruption(operation, control).is_some(),
            &observations,
        )
        .await
        {
            Ok(RecoveryScanOutcome::Match {
                final_van_position,
                vote_commitment_positions,
            }) => Ok(RecoveryScan::Match {
                final_van_position,
                vote_commitment_positions,
            }),
            Ok(RecoveryScanOutcome::NoMatch(authorization)) => {
                Ok(RecoveryScan::NoMatch(authorization))
            }
            Err(RecoveryScanFailure::Interrupted) => Ok(RecoveryScan::Interrupted),
            Err(RecoveryScanFailure::Invalid(diagnostic)) => {
                self.reconcile_with_durable_state(
                    derived.generation(),
                    SubmissionObservation::ContinueRecovery,
                    Some(diagnostic.clone()),
                    ChainSubmissionState::Recovering,
                )?;
                Err(ChainSubmissionFailure::with_durable_state(
                    ChainSubmissionFailureKind::Protocol,
                    ChainSubmissionState::Recovering,
                    diagnostic.message(),
                ))
            }
            Err(RecoveryScanFailure::Transport(error)) => {
                let diagnostic = ChainSubmissionDiagnostic::from_redacted_message(
                    ChainSubmissionDiagnosticKind::ReconciliationPending,
                    error.message(),
                );
                self.reconcile_with_durable_state(
                    derived.generation(),
                    SubmissionObservation::ContinueRecovery,
                    Some(diagnostic),
                    ChainSubmissionState::Recovering,
                )?;
                Err(ChainSubmissionFailure::with_durable_state(
                    ChainSubmissionFailureKind::Transport,
                    ChainSubmissionState::Recovering,
                    error.message(),
                ))
            }
        }
    }

    /// Resolves chain rejection code 2 observed after unresolved dispatch.
    ///
    /// Code 2 proves this generation's nullifiers are spent, so an earlier
    /// dispatch landed. Under exact-tree advancement one tree pass runs first:
    /// a match confirms the generation with its positions; a completed valid
    /// no-match discards its retry authorization, which must never dispatch
    /// again, and ends the generation as `SubmittedWithoutHash`. A scan that
    /// cannot complete leaves the row `Recovering` with its dispatch evidence
    /// so a later pass converges. Status-only advancement has no scan and ends
    /// the generation at once.
    #[allow(
        clippy::too_many_arguments,
        reason = "the code-2 settlement keeps each captured authority explicit"
    )]
    async fn settle_nullifier_spent_after_dispatch(
        &self,
        request: &StoreAdvancementRequest,
        operation: &CapturedSubmissionOperation,
        lease: &SubmissionOperationLease,
        derived: &DerivedChainSubmission,
        current: &StoredChainSubmission,
        recovery: ChainRecoveryMode,
        control: &dyn SubmissionControl,
    ) -> Result<ChainSubmissionResult, ChainSubmissionFailure> {
        let end_hashless = || {
            let submitted = ChainSubmissionDiagnostic::from_redacted_message(
                ChainSubmissionDiagnosticKind::NullifierAlreadySpent,
                "vote chain reported nullifier already spent after ambiguous dispatch",
            );
            self.classify_dispatched_post(
                derived.generation(),
                SubmissionObservation::SubmittedWithoutHash(submitted),
            )?
            .public_result()
        };
        if recovery == ChainRecoveryMode::StatusOnly {
            return end_hashless();
        }
        match self
            .scan_tree(derived, None, operation, lease, control)
            .await?
        {
            RecoveryScan::Match {
                final_van_position,
                vote_commitment_positions,
            } => self.confirm_tree(
                request,
                operation,
                derived,
                final_van_position,
                vote_commitment_positions,
                control,
            ),
            // The no-match authorization is deliberately discarded: code 2
            // already proved the nullifiers spent, so nothing may dispatch.
            RecoveryScan::NoMatch(_discarded_authorization) => end_hashless(),
            RecoveryScan::Interrupted => current.public_result(),
        }
    }

    /// Dispatches the one POST authorized by a completed valid no-match tree
    /// pass.
    ///
    /// `post_attempts_used` counts this invocation's earlier POSTs; this
    /// attempt is final when it exhausts the configured budget, in which case
    /// an ambiguous outcome is left for a later invocation rather than ended.
    /// Whether a "nullifier already spent" rejection may end the generation as
    /// `SubmittedWithoutHash` is decided by the durable row, not by the fact
    /// that a retry was authorized: only a row still carrying unresolved
    /// dispatch evidence treats that rejection as proof of an earlier
    /// submission, and even then one tree pass runs first.
    #[allow(
        clippy::too_many_arguments,
        reason = "the recovery retry keeps each captured authority explicit"
    )]
    async fn submit_recovery_retry(
        &self,
        request: &StoreAdvancementRequest,
        operation: &CapturedSubmissionOperation,
        lease: &SubmissionOperationLease,
        derived: DerivedChainSubmission,
        reserved: StoredChainSubmission,
        recovery: ChainRecoveryMode,
        post_attempts_used: usize,
        control: &dyn SubmissionControl,
    ) -> Result<ChainSubmissionResult, ChainSubmissionFailure> {
        let observations = control.observations();

        if interruption(operation, control).is_some() {
            return reserved.public_result();
        }
        let is_final_attempt = post_attempts_used + 1 == self.policy.maximum_post_attempts;
        let ambiguity_preceded = reserved.state().has_unresolved_dispatch();
        let in_flight = self
            .store
            .coordination()
            .register_in_flight(derived.generation().identity())
            .map_err(|error| {
                ChainSubmissionFailure::with_durable_state(
                    error.kind(),
                    ChainSubmissionState::Recovering,
                    error.message(),
                )
            })?;
        let ordinal = usize::try_from(reserved.committed_post_reservations()).unwrap_or(usize::MAX);
        let endpoint_index = ordinal.saturating_sub(1) % self.protocol.endpoint_count();
        let outcome = {
            let dispatch = ChainPostDispatch::default();
            let post =
                self.submit_to_endpoint(endpoint_index, &derived, dispatch.clone(), &observations);
            tokio::pin!(post);
            tokio::select! {
                biased;
                reason = wait_for_interruption(operation, control) => {
                    if !dispatch.is_possible() {
                        return reserved.public_result();
                    }
                    PostAttemptOutcome::PossiblyDispatched(
                        ChainSubmissionDiagnostic::from_redacted_message(
                            ChainSubmissionDiagnosticKind::AmbiguousDispatch,
                            reason.after_dispatch_message(),
                        ),
                    )
                }
                outcome = &mut post => outcome,
            }
        };
        // Both ambiguous outcomes, a lost response and a colliding hash, leave
        // the row hashless `Recovering` with this attempt's dispatch
        // diagnostic and share the continuation below.
        let ambiguous = match outcome {
            PostAttemptOutcome::Accepted(candidate) => {
                let record = self.classify_dispatched_post(
                    derived.generation(),
                    SubmissionObservation::UsableCandidateHash(candidate),
                )?;
                if !returned_candidate_is_unusable(&record) {
                    return record.public_result();
                }
                record
            }
            PostAttemptOutcome::PossiblyDispatched(diagnostic) => self.classify_dispatched_post(
                derived.generation(),
                SubmissionObservation::PossiblyDispatched(diagnostic),
            )?,
            PostAttemptOutcome::Rejected {
                kind, diagnostic, ..
            } => {
                if ambiguity_preceded && kind == ChainRejectionKind::NullifierAlreadySpent {
                    return self
                        .settle_nullifier_spent_after_dispatch(
                            request, operation, lease, &derived, &reserved, recovery, control,
                        )
                        .await;
                }
                // Without unresolved dispatch evidence a code-2 rejection is
                // handled like any other definite rejection: the row stays
                // `Recovering`, keeps its stored diagnostic, and the next
                // advance may scan the tree again and confirm if the
                // generation did land.
                let record = self.classify_dispatched_post(
                    derived.generation(),
                    SubmissionObservation::DefiniteRejection(diagnostic.clone()),
                )?;
                return Err(ChainSubmissionFailure::with_durable_state(
                    ChainSubmissionFailureKind::Protocol,
                    record.durable_state(),
                    diagnostic.message(),
                ));
            }
            // The retry never left the node's front door, so the row keeps
            // its earlier dispatch ambiguity untouched; the answer is reported
            // as a protocol failure because no node in rotation serves the
            // route this generation needs.
            PostAttemptOutcome::EndpointUnsupported(diagnostic) => {
                self.reconcile_with_durable_state(
                    derived.generation(),
                    SubmissionObservation::DefinitelyUnsent,
                    Some(ChainSubmissionDiagnostic::from_redacted_message(
                        ChainSubmissionDiagnosticKind::ReconciliationPending,
                        diagnostic.message(),
                    )),
                    ChainSubmissionState::Recovering,
                )?;
                return Err(ChainSubmissionFailure::with_durable_state(
                    ChainSubmissionFailureKind::Protocol,
                    ChainSubmissionState::Recovering,
                    diagnostic.message(),
                ));
            }
            PostAttemptOutcome::DefinitelyUnsent(error) => {
                self.reconcile_with_durable_state(
                    derived.generation(),
                    SubmissionObservation::DefinitelyUnsent,
                    Some(ChainSubmissionDiagnostic::from_redacted_message(
                        ChainSubmissionDiagnosticKind::ReconciliationPending,
                        error.message(),
                    )),
                    ChainSubmissionState::Recovering,
                )?;
                return Err(ChainSubmissionFailure::with_durable_state(
                    ChainSubmissionFailureKind::Transport,
                    ChainSubmissionState::Recovering,
                    error.message(),
                ));
            }
            PostAttemptOutcome::LocalFailure(diagnostic) => {
                self.reconcile_with_durable_state(
                    derived.generation(),
                    SubmissionObservation::ContinueRecovery,
                    Some(diagnostic.clone()),
                    ChainSubmissionState::Recovering,
                )?;
                return Err(ChainSubmissionFailure::with_durable_state(
                    ChainSubmissionFailureKind::Protocol,
                    ChainSubmissionState::Recovering,
                    diagnostic.message(),
                ));
            }
        };
        // A final ambiguous attempt leaves the durable ambiguity for a later
        // invocation's fresh budget; it is not chain evidence.
        if is_final_attempt {
            return ambiguous.public_result();
        }
        // The row now durably carries this attempt's dispatch ambiguity, so
        // the remaining budget continues through the ordinary ambiguous-retry
        // loop rather than ending here.
        drop(in_flight);
        if interruption(operation, control).is_some() {
            return ambiguous.public_result();
        }
        let reserved = match self
            .reserve_ambiguous_retry_after_backoff(
                self.policy.retry_backoffs[post_attempts_used],
                derived.generation(),
                ambiguous,
                operation,
                control,
            )
            .await?
        {
            AmbiguousRetryReservation::Reserved(reserved) => reserved,
            AmbiguousRetryReservation::Interrupted(reserved) => return reserved.public_result(),
        };
        // Boxed: the attempt loop can reach recovery again through
        // reconciliation, so this edge closes an async cycle.
        Box::pin(self.submit_reserved_attempts(
            request,
            operation,
            lease,
            derived,
            reserved,
            recovery,
            post_attempts_used + 1,
            control,
        ))
        .await
    }

    fn confirm_tree(
        &self,
        request: &StoreAdvancementRequest,
        operation: &CapturedSubmissionOperation,
        derived: &DerivedChainSubmission,
        final_van_position: u64,
        vote_commitment_positions: Vec<u64>,
        control: &dyn SubmissionControl,
    ) -> Result<ChainSubmissionResult, ChainSubmissionFailure> {
        let allowed = || interruption(operation, control).is_none();
        let now = self.clock.now_seconds().map_err(|error| {
            ChainSubmissionFailure::with_durable_state(
                error.kind(),
                ChainSubmissionState::Recovering,
                error.message(),
            )
        })?;
        let committed = self
            .store
            .confirm_tree(
                request,
                derived.generation(),
                final_van_position,
                vote_commitment_positions,
                &allowed,
                now,
            )
            .map_err(|error| {
                if error.strongest_state().is_some() {
                    error
                } else {
                    ChainSubmissionFailure::with_durable_state(
                        error.kind(),
                        ChainSubmissionState::Recovering,
                        error.message(),
                    )
                }
            })?;
        match committed {
            ConfirmationCommit::Interrupted(record) | ConfirmationCommit::Confirmed(record) => {
                record.public_result()
            }
        }
    }

    fn tracking_expired(
        &self,
        record: &StoredChainSubmission,
    ) -> Result<bool, ChainSubmissionFailure> {
        let now = self.clock.now_seconds().map_err(|error| {
            ChainSubmissionFailure::with_durable_state(
                error.kind(),
                record.durable_state(),
                error.message(),
            )
        })?;
        self.tracking_expired_at(record, now)
    }

    fn tracking_expired_at(
        &self,
        record: &StoredChainSubmission,
        now: u64,
    ) -> Result<bool, ChainSubmissionFailure> {
        let started = record.tracking_started_at().ok_or_else(|| {
            ChainSubmissionFailure::with_durable_state(
                ChainSubmissionFailureKind::InvariantViolation,
                record.durable_state(),
                "Tracking record has no immutable tracking-start timestamp",
            )
        })?;
        Ok(now < started || now.saturating_sub(started) >= self.policy.tracking_window_seconds)
    }

    fn confirm(
        &self,
        context: ConfirmationContext<'_>,
    ) -> Result<ChainSubmissionResult, ChainSubmissionFailure> {
        let allowed = || interruption(context.operation, context.control).is_none();
        let now = self.clock.now_seconds().map_err(|error| {
            ChainSubmissionFailure::with_durable_state(
                error.kind(),
                context.durable_state,
                error.message(),
            )
        })?;
        let committed = self
            .store
            .confirm_committed(
                context.request,
                context.derived.generation(),
                context.candidate,
                context.committed,
                &allowed,
                now,
            )
            .map_err(|error| {
                if error.strongest_state().is_some() {
                    error
                } else {
                    ChainSubmissionFailure::with_durable_state(
                        error.kind(),
                        context.durable_state,
                        error.message(),
                    )
                }
            })?;
        match committed {
            ConfirmationCommit::Interrupted(record) => record.public_result(),
            ConfirmationCommit::Confirmed(record) => record.public_result(),
        }
    }

    async fn lookup_or_interruption(
        &self,
        operation: &CapturedSubmissionOperation,
        candidate: super::CandidateTransactionHash,
        control: &dyn SubmissionControl,
    ) -> LookupProgress {
        let observations = control.observations();

        let lookup = self.protocol.transaction_status(candidate, &observations);
        tokio::pin!(lookup);
        tokio::select! {
            biased;
            _ = wait_for_interruption(operation, control) => LookupProgress::Interrupted,
            result = &mut lookup => match result {
                Ok(observation) => LookupProgress::Observed(observation),
                Err(failure) => LookupProgress::Failed(failure),
            },
        }
    }

    async fn wait_backoff_or_interruption(
        &self,
        delay: Duration,
        operation: &CapturedSubmissionOperation,
        control: &dyn SubmissionControl,
    ) -> Option<Interruption> {
        tokio::select! {
            biased;
            reason = wait_for_interruption(operation, control) => Some(reason),
            _ = tokio::time::sleep(delay) => None,
        }
    }
}

enum LookupProgress {
    Interrupted,
    Observed(TransactionStatusObservation),
    Failed(LookupFailure),
}

#[derive(Clone, Copy)]
enum Interruption {
    Cancelled,
    StaleEpoch,
}

impl Interruption {
    fn after_dispatch_message(self) -> &'static str {
        match self {
            Self::Cancelled => "submission was cancelled after dispatch could not be excluded",
            Self::StaleEpoch => "host operation epoch changed after dispatch could not be excluded",
        }
    }
}

fn interruption(
    operation: &CapturedSubmissionOperation,
    control: &dyn SubmissionControl,
) -> Option<Interruption> {
    if control.is_cancelled() {
        Some(Interruption::Cancelled)
    } else if control.operation_epoch() != operation.host_operation_epoch() {
        Some(Interruption::StaleEpoch)
    } else {
        None
    }
}

async fn wait_for_interruption(
    operation: &CapturedSubmissionOperation,
    control: &dyn SubmissionControl,
) -> Interruption {
    loop {
        if let Some(reason) = interruption(operation, control) {
            return reason;
        }
        tokio::time::sleep(CONTROL_CHECK_INTERVAL).await;
    }
}

fn interrupted_without_state(
    reason: Interruption,
) -> Result<ChainSubmissionResult, ChainSubmissionFailure> {
    match reason {
        Interruption::Cancelled => Ok(ChainSubmissionResult::Cancelled),
        Interruption::StaleEpoch => Err(ChainSubmissionFailure::without_state(
            ChainSubmissionFailureKind::InvalidInput,
            "host operation epoch changed before request dispatch",
        )),
    }
}

/// Outcome of one completed or interrupted exact-tree pass.
enum RecoveryScan<'a> {
    /// The complete expected layout was found once; confirm with positions.
    Match {
        final_van_position: u64,
        vote_commitment_positions: Vec<u64>,
    },
    /// A valid complete pass found no layout; the private authorization may
    /// reserve exactly one same-generation retry, or be discarded.
    NoMatch(RecoveryRetryAuthorization<'a>),
    /// The host interrupted the pass; the row is unchanged.
    Interrupted,
}

/// Outcome of waiting out a backoff and reserving the next ambiguous retry.
enum AmbiguousRetryReservation {
    /// The next same-generation POST is durably reserved.
    Reserved(StoredChainSubmission),
    /// The host interrupted the backoff; the row is unchanged.
    Interrupted(StoredChainSubmission),
}

fn is_retryable_dispatch_ambiguity(record: &StoredChainSubmission) -> bool {
    record.state().permits_ambiguous_retry()
}

/// True when the store classified a returned hash as a collision: the hash is
/// already bound to another generation, so the response is dispatch ambiguity
/// rather than usable acceptance and consumes the attempt like a timeout.
fn returned_candidate_is_unusable(record: &StoredChainSubmission) -> bool {
    matches!(
        record.state(),
        SubmissionRecordState::Recovering {
            candidate_transaction_hash: None,
            ..
        }
    )
}

fn lookup_diagnostic(failure: &LookupFailure) -> ChainSubmissionDiagnostic {
    match failure {
        LookupFailure::Protocol(diagnostic) => diagnostic.clone(),
        LookupFailure::Transport(error) => ChainSubmissionDiagnostic::from_redacted_message(
            ChainSubmissionDiagnosticKind::ReconciliationPending,
            error.message(),
        ),
    }
}

fn lookup_failure(
    failure: LookupFailure,
    durable_state: ChainSubmissionState,
) -> ChainSubmissionFailure {
    match failure {
        LookupFailure::Protocol(diagnostic) => ChainSubmissionFailure::with_durable_state(
            ChainSubmissionFailureKind::Protocol,
            durable_state,
            diagnostic.message(),
        ),
        LookupFailure::Transport(error) => ChainSubmissionFailure::with_durable_state(
            ChainSubmissionFailureKind::Transport,
            durable_state,
            error.message(),
        ),
    }
}

#[cfg(test)]
mod tests;
