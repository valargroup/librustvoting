//! One bounded, state-driven advancement of chain submission.

use std::{sync::Arc, time::Duration};

use super::{
    coordination::CapturedSubmissionOperation,
    generation::{ChainSubmissionRequest, DerivedChainSubmission},
    protocol::{
        ChainProtocolClient, LookupFailure, PostAttemptOutcome, TransactionStatusObservation,
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

/// Finite policy for one private coordinator instance.
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
pub(super) trait ChainSubmissionControl: Send + Sync {
    fn is_cancelled(&self) -> bool;
    fn operation_epoch(&self) -> u64;
}

/// Private lifecycle engine shared by Phase 5's future public entry points.
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
    control: &'a dyn ChainSubmissionControl,
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
        if policy.maximum_post_attempts > protocol.endpoint_count() {
            return Err(ChainSubmissionFailure::without_state(
                ChainSubmissionFailureKind::InvalidInput,
                "maximum POST attempts exceeds the distinct endpoint count",
            ));
        }
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
        control: &dyn ChainSubmissionControl,
    ) -> Result<ChainSubmissionResult, ChainSubmissionFailure> {
        let operation =
            CapturedSubmissionOperation::new(request.identity().clone(), control.operation_epoch());
        let applicable_identities = request.applicable_identities();
        let _lease = self
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
                record: _,
                fresh_reservation,
            } if fresh_reservation => {
                self.submit_fresh(request, operation, *derived, control)
                    .await
            }
            StoreAdmission::Ready {
                derived, record, ..
            } => {
                self.reconcile_existing(&request, &operation, *derived, record, control)
                    .await
            }
        }
    }

    async fn submit_fresh(
        &self,
        request: StoreAdvancementRequest,
        operation: CapturedSubmissionOperation,
        mut derived: DerivedChainSubmission,
        control: &dyn ChainSubmissionControl,
    ) -> Result<ChainSubmissionResult, ChainSubmissionFailure> {
        for attempt_index in 0..self.policy.maximum_post_attempts {
            if let Some(reason) = interruption(&operation, control) {
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
                    self.remove_fresh_reservation(derived.generation())?;
                    return Err(error);
                }
            };
            let outcome = {
                let dispatch = ChainPostDispatch::default();
                let post = self.submit_to_endpoint(attempt_index, &derived, dispatch.clone());
                tokio::pin!(post);
                tokio::select! {
                    biased;
                    reason = wait_for_interruption(&operation, control) => {
                        if !dispatch.is_possible() {
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
                    return self
                        .reconcile_existing(&request, &operation, derived, record, control)
                        .await;
                }
                PostAttemptOutcome::Rejected { diagnostic, .. } => {
                    let record = self.classify_dispatched_post(
                        derived.generation(),
                        SubmissionObservation::DefiniteRejection(diagnostic),
                    )?;
                    return record.public_result();
                }
                PostAttemptOutcome::PossiblyDispatched(diagnostic) => {
                    let record = self.classify_dispatched_post(
                        derived.generation(),
                        SubmissionObservation::PossiblyDispatched(diagnostic),
                    )?;
                    return record.public_result();
                }
                PostAttemptOutcome::LocalFailure(diagnostic) => {
                    self.remove_fresh_reservation(derived.generation())?;
                    return Err(ChainSubmissionFailure::without_state(
                        ChainSubmissionFailureKind::Protocol,
                        diagnostic.message(),
                    ));
                }
                PostAttemptOutcome::DefinitelyUnsent(error) => {
                    self.remove_fresh_reservation(derived.generation())?;
                    if attempt_index + 1 == self.policy.maximum_post_attempts {
                        return Err(ChainSubmissionFailure::without_state(
                            ChainSubmissionFailureKind::Transport,
                            error.message(),
                        ));
                    }
                    if let Some(reason) = self
                        .wait_backoff_or_interruption(
                            self.policy.retry_backoffs[attempt_index],
                            &operation,
                            control,
                        )
                        .await
                    {
                        return interrupted_without_state(reason);
                    }
                    if let Some(reason) = interruption(&operation, control) {
                        return interrupted_without_state(reason);
                    }
                    match self.store.admit(
                        &request,
                        true,
                        u64::try_from(attempt_index + 2).expect("bounded attempt index fits u64"),
                        self.clock.now_seconds()?,
                    )? {
                        StoreAdmission::Ready {
                            derived: retry,
                            fresh_reservation: true,
                            ..
                        } if retry.generation() == derived.generation() => derived = *retry,
                        StoreAdmission::Ready { record, .. }
                        | StoreAdmission::Authoritative(record) => {
                            return record.public_result();
                        }
                        _ => {
                            return Err(ChainSubmissionFailure::without_state(
                                ChainSubmissionFailureKind::InvariantViolation,
                                "definitely-unsent retry did not reserve the same generation",
                            ));
                        }
                    }
                }
            }
        }
        unreachable!("validated attempt bound is nonzero")
    }

    async fn submit_to_endpoint(
        &self,
        endpoint_index: usize,
        derived: &DerivedChainSubmission,
        dispatch: ChainPostDispatch,
    ) -> PostAttemptOutcome {
        match derived.request() {
            ChainSubmissionRequest::Delegation(submission) => {
                self.protocol
                    .submit_delegation_with_dispatch(endpoint_index, submission, dispatch)
                    .await
            }
            ChainSubmissionRequest::Vote(submission) => {
                self.protocol
                    .submit_vote_with_dispatch(endpoint_index, submission, dispatch)
                    .await
            }
            ChainSubmissionRequest::VoteBatch(_) => {
                PostAttemptOutcome::LocalFailure(ChainSubmissionDiagnostic::from_redacted_message(
                    ChainSubmissionDiagnosticKind::InvalidProtocolResponse,
                    "atomic vote-batch submission is disabled until phase 7",
                ))
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

    async fn reconcile_existing(
        &self,
        request: &StoreAdvancementRequest,
        operation: &CapturedSubmissionOperation,
        derived: DerivedChainSubmission,
        record: StoredChainSubmission,
        control: &dyn ChainSubmissionControl,
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
                        self.finish_inconclusive_tracking(derived, record, None)
                    }
                    LookupProgress::Failed(failure) => {
                        if self.tracking_expired(&record)? {
                            self.finish_inconclusive_tracking(
                                derived,
                                record,
                                Some(lookup_diagnostic(&failure)),
                            )
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
                        self.reconcile_with_durable_state(
                            derived.generation(),
                            SubmissionObservation::CandidateCommittedFailure(diagnostic.clone()),
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
            } => record.public_result(),
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
                        self.reconcile_with_durable_state(
                            derived.generation(),
                            SubmissionObservation::CandidateCommittedFailure(
                                ChainSubmissionDiagnostic::from_redacted_message(
                                    ChainSubmissionDiagnosticKind::ChainRejected,
                                    "recovery candidate committed unsuccessfully",
                                ),
                            ),
                            None,
                            ChainSubmissionState::Recovering,
                        )?
                        .public_result()
                    }
                    LookupProgress::Observed(TransactionStatusObservation::Pending) => self
                        .reconcile_with_durable_state(
                            derived.generation(),
                            SubmissionObservation::CandidatePending,
                            None,
                            ChainSubmissionState::Recovering,
                        )?
                        .public_result(),
                    LookupProgress::Failed(failure) => self
                        .reconcile_with_durable_state(
                            derived.generation(),
                            SubmissionObservation::ContinueRecovery,
                            Some(lookup_diagnostic(&failure)),
                            ChainSubmissionState::Recovering,
                        )?
                        .public_result(),
                }
            }
            _ => record.public_result(),
        }
    }

    fn finish_inconclusive_tracking(
        &self,
        derived: DerivedChainSubmission,
        record: StoredChainSubmission,
        diagnostic: Option<ChainSubmissionDiagnostic>,
    ) -> Result<ChainSubmissionResult, ChainSubmissionFailure> {
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
        )?
        .public_result()
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
        control: &dyn ChainSubmissionControl,
    ) -> LookupProgress {
        let lookup = self.protocol.transaction_status(candidate);
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
        control: &dyn ChainSubmissionControl,
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
    control: &dyn ChainSubmissionControl,
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
    control: &dyn ChainSubmissionControl,
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
