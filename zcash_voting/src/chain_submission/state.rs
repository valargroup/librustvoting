use thiserror::Error;

use super::result::ValidatedChainSubmissionConfirmation;
use super::{
    CandidateTransactionHash, ChainSubmissionDiagnostic, ChainSubmissionDiagnosticKind,
    ChainSubmissionState,
};

/// Authoritative typed state for one semantic generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum SubmissionRecordState {
    Submitting,
    Tracking {
        candidate_transaction_hash: CandidateTransactionHash,
    },
    Recovering {
        candidate_transaction_hash: Option<CandidateTransactionHash>,
        ambiguity_diagnostic: ChainSubmissionDiagnostic,
    },
    SubmittedWithoutHash(ChainSubmissionDiagnostic),
    Confirmed(ValidatedChainSubmissionConfirmation),
    Rejected(ChainSubmissionDiagnostic),
}

impl SubmissionRecordState {
    /// True for a hashless `Recovering` row created by a possibly-dispatched
    /// POST, which may reserve the next same-generation POST directly.
    ///
    /// `AmbiguousDispatch` (timeout, transport ambiguity, interruption after
    /// dispatch, abandoned reservation), `EndpointUnsupported` and
    /// `RouteAnswerReplaced` (possibly replaced route responses), and
    /// `InvalidProtocolResponse` (an unusable or malformed response after
    /// dispatch, including a hash owned by another generation) are dispatch
    /// ambiguities. A definite rejection lands here with `ChainRejected` and
    /// never reserves this way.
    pub(super) fn permits_ambiguous_retry(&self) -> bool {
        matches!(
            self,
            Self::Recovering {
                candidate_transaction_hash: None,
                ambiguity_diagnostic,
            } if matches!(
                ambiguity_diagnostic.kind(),
                ChainSubmissionDiagnosticKind::AmbiguousDispatch
                    | ChainSubmissionDiagnosticKind::InvalidProtocolResponse
                    | ChainSubmissionDiagnosticKind::EndpointUnsupported
                    | ChainSubmissionDiagnosticKind::RouteAnswerReplaced
            )
        )
    }

    /// True while the durable row still carries evidence that this generation
    /// may already be on chain with an unknown outcome.
    ///
    /// The stored diagnostic is the durable carrier. It is set when the row
    /// enters `Recovering` and replaced only by a later observation that is
    /// itself dispatch evidence, so it survives restarts:
    ///
    /// - `AmbiguousDispatch`, `InvalidProtocolResponse`,
    ///   `EndpointUnsupported`, and `RouteAnswerReplaced` record a POST whose
    ///   delivery or response was lost or possibly replaced;
    /// - `TrackingWindowExpired` records an accepted hash that never resolved.
    ///
    /// `ChainRejected` records a definite outcome: a rejected POST or a
    /// candidate that committed unsuccessfully. Neither spent this
    /// generation's nullifiers, so a later "nullifier already spent" rejection
    /// is not evidence that this generation was submitted and must not become
    /// terminal `SubmittedWithoutHash`.
    pub(super) fn has_unresolved_dispatch(&self) -> bool {
        matches!(
            self,
            Self::Recovering {
                ambiguity_diagnostic,
                ..
            } if matches!(
                ambiguity_diagnostic.kind(),
                ChainSubmissionDiagnosticKind::AmbiguousDispatch
                    | ChainSubmissionDiagnosticKind::InvalidProtocolResponse
                    | ChainSubmissionDiagnosticKind::EndpointUnsupported
                    | ChainSubmissionDiagnosticKind::RouteAnswerReplaced
                    | ChainSubmissionDiagnosticKind::TrackingWindowExpired
            )
        )
    }

    pub(super) fn durable_state(&self) -> ChainSubmissionState {
        match self {
            Self::Submitting => ChainSubmissionState::Submitting,
            Self::Tracking { .. } => ChainSubmissionState::Tracking,
            Self::Recovering { .. } => ChainSubmissionState::Recovering,
            Self::SubmittedWithoutHash(_) => ChainSubmissionState::SubmittedWithoutHash,
            Self::Confirmed(_) => ChainSubmissionState::Confirmed,
            Self::Rejected(_) => ChainSubmissionState::Rejected,
        }
    }
}

/// One classified observation applied by the lifecycle coordinator.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum SubmissionObservation {
    ReserveFreshSubmission,
    DefinitelyUnsent,
    UsableCandidateHash(CandidateTransactionHash),
    PossiblyDispatched(ChainSubmissionDiagnostic),
    DefiniteRejection(ChainSubmissionDiagnostic),
    /// The first POST of a combined delegation-and-cast batch was definitely
    /// rejected before any attempt was possibly dispatched. Unlike
    /// `DefiniteRejection`, which keeps a row recoverable because an earlier
    /// attempt may have landed, this is chain evidence that nothing landed and
    /// makes the generation terminal so its members can be retired.
    TerminalRejection(ChainSubmissionDiagnostic),
    SubmittedWithoutHash(ChainSubmissionDiagnostic),
    CandidatePending,
    TrackingWindowExpired(ChainSubmissionDiagnostic),
    CandidateCommittedFailure(ChainSubmissionDiagnostic),
    TerminalCandidateFailure(ChainSubmissionDiagnostic),
    Confirmed(ValidatedChainSubmissionConfirmation),
    ContinueRecovery,
    AbandonedSubmitting(ChainSubmissionDiagnostic),
}

/// Applies one observation without performing I/O.
///
/// `None` represents an absent row. A definitely-unsent first attempt removes
/// its fresh reservation and is the only transition back to `None`.
pub(super) fn apply_submission_observation(
    current: Option<SubmissionRecordState>,
    observation: SubmissionObservation,
) -> Result<Option<SubmissionRecordState>, SubmissionTransitionError> {
    use SubmissionObservation as Observation;
    use SubmissionRecordState as State;

    match (current, observation) {
        (None, Observation::ReserveFreshSubmission) => Ok(Some(State::Submitting)),

        (Some(State::Submitting), Observation::DefinitelyUnsent) => Ok(None),
        (Some(State::Submitting), Observation::UsableCandidateHash(candidate)) => {
            Ok(Some(State::Tracking {
                candidate_transaction_hash: candidate,
            }))
        }
        (Some(State::Submitting), Observation::PossiblyDispatched(diagnostic))
        | (Some(State::Submitting), Observation::AbandonedSubmitting(diagnostic)) => {
            Ok(Some(State::Recovering {
                candidate_transaction_hash: None,
                ambiguity_diagnostic: diagnostic,
            }))
        }
        (Some(State::Submitting), Observation::DefiniteRejection(diagnostic)) => {
            Ok(Some(State::Recovering {
                candidate_transaction_hash: None,
                ambiguity_diagnostic: diagnostic,
            }))
        }
        // Only `Submitting` admits this: the state exists solely before any
        // possibly-dispatched attempt, so a terminal rejection can never
        // erase dispatch ambiguity recorded by an earlier POST.
        (Some(State::Submitting), Observation::TerminalRejection(diagnostic)) => {
            Ok(Some(State::Rejected(diagnostic)))
        }
        (Some(State::Submitting), Observation::SubmittedWithoutHash(diagnostic))
        | (Some(State::Recovering { .. }), Observation::SubmittedWithoutHash(diagnostic)) => {
            Ok(Some(State::SubmittedWithoutHash(diagnostic)))
        }

        (Some(state @ State::Tracking { .. }), Observation::CandidatePending) => Ok(Some(state)),
        (
            Some(State::Tracking {
                candidate_transaction_hash,
            }),
            Observation::TrackingWindowExpired(diagnostic),
        ) => Ok(Some(State::Recovering {
            candidate_transaction_hash: Some(candidate_transaction_hash),
            ambiguity_diagnostic: diagnostic,
        })),
        (
            Some(State::Tracking {
                candidate_transaction_hash: _,
            }),
            Observation::CandidateCommittedFailure(diagnostic),
        ) => Ok(Some(State::Recovering {
            candidate_transaction_hash: None,
            ambiguity_diagnostic: diagnostic,
        })),
        (
            Some(State::Tracking { .. } | State::Recovering { .. }),
            Observation::TerminalCandidateFailure(diagnostic),
        ) => Ok(Some(State::Rejected(diagnostic))),
        (
            Some(State::Tracking {
                candidate_transaction_hash,
            }),
            Observation::Confirmed(confirmation),
        ) => {
            require_hash_confirmation_for_candidate(&confirmation, candidate_transaction_hash)?;
            Ok(Some(State::Confirmed(confirmation)))
        }

        (
            Some(State::Recovering {
                candidate_transaction_hash: None,
                ..
            }),
            Observation::UsableCandidateHash(candidate),
        ) => Ok(Some(State::Tracking {
            candidate_transaction_hash: candidate,
        })),
        (
            Some(State::Recovering {
                candidate_transaction_hash: Some(existing),
                ambiguity_diagnostic,
            }),
            Observation::UsableCandidateHash(candidate),
        ) => {
            if existing != candidate {
                return Err(SubmissionTransitionError::ConflictingCandidateHash);
            }
            Ok(Some(State::Recovering {
                candidate_transaction_hash: Some(existing),
                ambiguity_diagnostic,
            }))
        }
        // A retry that is itself possibly dispatched replaces the stored
        // diagnostic: the row now carries fresh dispatch ambiguity, which
        // qualifies the next same-generation POST for a direct ambiguous
        // retry under status-only advancement. Exact-tree advancement still
        // scans first.
        (
            Some(State::Recovering {
                candidate_transaction_hash: None,
                ..
            }),
            Observation::PossiblyDispatched(diagnostic),
        ) => Ok(Some(State::Recovering {
            candidate_transaction_hash: None,
            ambiguity_diagnostic: diagnostic,
        })),
        // A definite rejection after earlier ambiguity preserves that
        // ambiguity; the rejection is surfaced to the caller, not stored.
        (
            Some(state @ State::Recovering { .. }),
            Observation::ContinueRecovery
            | Observation::PossiblyDispatched(_)
            | Observation::DefiniteRejection(_)
            | Observation::DefinitelyUnsent,
        ) => Ok(Some(state)),
        (
            Some(
                state @ State::Recovering {
                    candidate_transaction_hash: Some(_),
                    ..
                },
            ),
            Observation::CandidatePending,
        ) => Ok(Some(state)),
        // The candidate resolved definitively, so the row no longer carries
        // unresolved dispatch evidence; the committed-failure diagnostic
        // replaces the earlier ambiguity exactly as it does from `Tracking`.
        (
            Some(State::Recovering {
                candidate_transaction_hash: Some(_),
                ..
            }),
            Observation::CandidateCommittedFailure(diagnostic),
        ) => Ok(Some(State::Recovering {
            candidate_transaction_hash: None,
            ambiguity_diagnostic: diagnostic,
        })),
        (
            Some(State::Recovering {
                candidate_transaction_hash,
                ..
            }),
            Observation::Confirmed(confirmation),
        ) => {
            require_valid_recovery_confirmation(&confirmation, candidate_transaction_hash)?;
            Ok(Some(State::Confirmed(confirmation)))
        }

        (Some(State::Confirmed(existing)), Observation::Confirmed(replayed)) => {
            if existing == replayed {
                Ok(Some(State::Confirmed(existing)))
            } else {
                Err(SubmissionTransitionError::ConflictingConfirmation)
            }
        }
        (current, observation) => Err(SubmissionTransitionError::IllegalTransition {
            from: current.as_ref().map(SubmissionRecordState::durable_state),
            observation: observation.name(),
        }),
    }
}

fn require_hash_confirmation_for_candidate(
    confirmation: &ValidatedChainSubmissionConfirmation,
    candidate: CandidateTransactionHash,
) -> Result<(), SubmissionTransitionError> {
    match confirmation {
        ValidatedChainSubmissionConfirmation::Hash(confirmation)
            if confirmation.transaction_hash() == Some(candidate) =>
        {
            Ok(())
        }
        _ => Err(SubmissionTransitionError::ConfirmationDoesNotMatchCandidate),
    }
}

fn require_valid_recovery_confirmation(
    confirmation: &ValidatedChainSubmissionConfirmation,
    candidate: Option<CandidateTransactionHash>,
) -> Result<(), SubmissionTransitionError> {
    match confirmation {
        ValidatedChainSubmissionConfirmation::Hash(_) => {
            let Some(candidate) = candidate else {
                return Err(SubmissionTransitionError::ConfirmationDoesNotMatchCandidate);
            };
            require_hash_confirmation_for_candidate(confirmation, candidate)
        }
        ValidatedChainSubmissionConfirmation::Tree(_) => Ok(()),
    }
}

impl SubmissionObservation {
    fn name(&self) -> &'static str {
        match self {
            Self::ReserveFreshSubmission => "reserve_fresh_submission",
            Self::DefinitelyUnsent => "definitely_unsent",
            Self::UsableCandidateHash(_) => "usable_candidate_hash",
            Self::PossiblyDispatched(_) => "possibly_dispatched",
            Self::DefiniteRejection(_) => "definite_rejection",
            Self::TerminalRejection(_) => "terminal_rejection",
            Self::SubmittedWithoutHash(_) => "submitted_without_hash",
            Self::CandidatePending => "candidate_pending",
            Self::TrackingWindowExpired(_) => "tracking_window_expired",
            Self::CandidateCommittedFailure(_) => "candidate_committed_failure",
            Self::TerminalCandidateFailure(_) => "terminal_candidate_failure",
            Self::Confirmed(_) => "confirmed",
            Self::ContinueRecovery => "continue_recovery",
            Self::AbandonedSubmitting(_) => "abandoned_submitting",
        }
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub(super) enum SubmissionTransitionError {
    #[error("illegal chain submission transition from {from:?} after {observation}")]
    IllegalTransition {
        from: Option<ChainSubmissionState>,
        observation: &'static str,
    },
    #[error("candidate transaction hash conflicts with the durable candidate")]
    ConflictingCandidateHash,
    #[error("hash confirmation does not match the durable candidate transaction")]
    ConfirmationDoesNotMatchCandidate,
    #[error("terminal confirmation conflicts with the durable confirmation")]
    ConflictingConfirmation,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(byte: u8) -> CandidateTransactionHash {
        CandidateTransactionHash::from_bytes([byte; 32])
    }

    fn diagnostic(message: &str) -> ChainSubmissionDiagnostic {
        diagnostic_of(ChainSubmissionDiagnosticKind::AmbiguousDispatch, message)
    }

    fn diagnostic_of(
        kind: ChainSubmissionDiagnosticKind,
        message: &str,
    ) -> ChainSubmissionDiagnostic {
        ChainSubmissionDiagnostic::from_redacted_message(kind, message)
    }

    fn hash_confirmation(hash: CandidateTransactionHash) -> ValidatedChainSubmissionConfirmation {
        ValidatedChainSubmissionConfirmation::from_hash(hash, 10, vec![11]).unwrap()
    }

    fn tree_confirmation() -> ValidatedChainSubmissionConfirmation {
        ValidatedChainSubmissionConfirmation::from_tree(10, vec![11]).unwrap()
    }

    fn apply(
        state: Option<SubmissionRecordState>,
        observation: SubmissionObservation,
    ) -> Option<SubmissionRecordState> {
        apply_submission_observation(state, observation).unwrap()
    }

    #[test]
    fn normal_tracking_path_reaches_confirmation() {
        let hash = candidate(1);
        let submitting = apply(None, SubmissionObservation::ReserveFreshSubmission);
        let tracking = apply(submitting, SubmissionObservation::UsableCandidateHash(hash));
        assert_eq!(
            tracking,
            Some(SubmissionRecordState::Tracking {
                candidate_transaction_hash: hash
            })
        );

        let still_tracking = apply(tracking, SubmissionObservation::CandidatePending);
        assert_eq!(
            apply(
                still_tracking,
                SubmissionObservation::Confirmed(hash_confirmation(hash)),
            ),
            Some(SubmissionRecordState::Confirmed(hash_confirmation(hash)))
        );
    }

    #[test]
    fn definitely_unsent_first_attempt_removes_reservation() {
        let submitting = apply(None, SubmissionObservation::ReserveFreshSubmission);
        assert_eq!(
            apply(submitting, SubmissionObservation::DefinitelyUnsent),
            None
        );
    }

    #[test]
    fn possible_dispatch_enters_sticky_recovery() {
        let first_ambiguity = diagnostic("response lost after dispatch");
        let submitting = apply(None, SubmissionObservation::ReserveFreshSubmission);
        let recovering = apply(
            submitting,
            SubmissionObservation::PossiblyDispatched(first_ambiguity.clone()),
        );
        let with_candidate = apply(
            recovering,
            SubmissionObservation::UsableCandidateHash(candidate(2)),
        );

        assert_eq!(
            with_candidate,
            Some(SubmissionRecordState::Tracking {
                candidate_transaction_hash: candidate(2),
            })
        );
    }

    #[test]
    fn abandoned_submitting_is_conservatively_recovered() {
        let submitting = apply(None, SubmissionObservation::ReserveFreshSubmission);
        let abandoned = diagnostic("process restarted with reserved submission");
        assert_eq!(
            apply(
                submitting,
                SubmissionObservation::AbandonedSubmitting(abandoned.clone())
            ),
            Some(SubmissionRecordState::Recovering {
                candidate_transaction_hash: None,
                ambiguity_diagnostic: abandoned,
            })
        );
    }

    #[test]
    fn tracking_expiry_retains_candidate_in_recovery() {
        let hash = candidate(3);
        let expired = diagnostic("tracking window expired");
        assert_eq!(
            apply(
                Some(SubmissionRecordState::Tracking {
                    candidate_transaction_hash: hash,
                }),
                SubmissionObservation::TrackingWindowExpired(expired.clone())
            ),
            Some(SubmissionRecordState::Recovering {
                candidate_transaction_hash: Some(hash),
                ambiguity_diagnostic: expired,
            })
        );
    }

    #[test]
    fn committed_failure_moves_tracking_to_recovery_and_clears_recovery_candidates() {
        let committed_failure = diagnostic_of(
            ChainSubmissionDiagnosticKind::ChainRejected,
            "transaction committed unsuccessfully",
        );
        let tracking = Some(SubmissionRecordState::Tracking {
            candidate_transaction_hash: candidate(4),
        });
        assert_eq!(
            apply(
                tracking,
                SubmissionObservation::CandidateCommittedFailure(committed_failure.clone())
            ),
            Some(SubmissionRecordState::Recovering {
                candidate_transaction_hash: None,
                ambiguity_diagnostic: committed_failure.clone(),
            })
        );

        let recovering = Some(SubmissionRecordState::Recovering {
            candidate_transaction_hash: Some(candidate(4)),
            ambiguity_diagnostic: diagnostic("original ambiguity"),
        });
        let cleared = apply(
            recovering,
            SubmissionObservation::CandidateCommittedFailure(committed_failure.clone()),
        );
        assert_eq!(
            cleared,
            Some(SubmissionRecordState::Recovering {
                candidate_transaction_hash: None,
                ambiguity_diagnostic: committed_failure,
            })
        );
        assert!(!cleared.unwrap().has_unresolved_dispatch());
    }

    #[test]
    fn possible_dispatch_on_hashless_recovery_carries_the_new_ambiguity() {
        let expired = diagnostic_of(
            ChainSubmissionDiagnosticKind::TrackingWindowExpired,
            "tracking window expired",
        );
        let retried = diagnostic("retry response was lost");
        let hashless = Some(SubmissionRecordState::Recovering {
            candidate_transaction_hash: None,
            ambiguity_diagnostic: expired.clone(),
        });
        assert!(!hashless.as_ref().unwrap().permits_ambiguous_retry());

        let after_retry = apply(
            hashless,
            SubmissionObservation::PossiblyDispatched(retried.clone()),
        );
        assert_eq!(
            after_retry,
            Some(SubmissionRecordState::Recovering {
                candidate_transaction_hash: None,
                ambiguity_diagnostic: retried.clone(),
            })
        );
        assert!(after_retry.unwrap().permits_ambiguous_retry());

        let with_candidate = Some(SubmissionRecordState::Recovering {
            candidate_transaction_hash: Some(candidate(5)),
            ambiguity_diagnostic: expired.clone(),
        });
        assert_eq!(
            apply(
                with_candidate.clone(),
                SubmissionObservation::PossiblyDispatched(retried)
            ),
            with_candidate
        );
    }

    #[test]
    fn unresolved_dispatch_follows_the_stored_diagnostic_kind() {
        let recovering = |kind| SubmissionRecordState::Recovering {
            candidate_transaction_hash: None,
            ambiguity_diagnostic: diagnostic_of(kind, "stored"),
        };
        for kind in [
            ChainSubmissionDiagnosticKind::AmbiguousDispatch,
            ChainSubmissionDiagnosticKind::InvalidProtocolResponse,
            ChainSubmissionDiagnosticKind::EndpointUnsupported,
            ChainSubmissionDiagnosticKind::RouteAnswerReplaced,
            ChainSubmissionDiagnosticKind::TrackingWindowExpired,
        ] {
            assert!(recovering(kind).has_unresolved_dispatch(), "{kind:?}");
        }
        for kind in [
            ChainSubmissionDiagnosticKind::ChainRejected,
            ChainSubmissionDiagnosticKind::ReconciliationPending,
            ChainSubmissionDiagnosticKind::StorageFailure,
        ] {
            assert!(!recovering(kind).has_unresolved_dispatch(), "{kind:?}");
        }
        assert!(!SubmissionRecordState::Submitting.has_unresolved_dispatch());
        assert!(!SubmissionRecordState::Tracking {
            candidate_transaction_hash: candidate(1),
        }
        .has_unresolved_dispatch());
    }

    #[test]
    fn terminal_candidate_failure_rejects_poll_only_tracking_and_recovery() {
        let failure = diagnostic("imported transaction committed unsuccessfully");
        for state in [
            SubmissionRecordState::Tracking {
                candidate_transaction_hash: candidate(4),
            },
            SubmissionRecordState::Recovering {
                candidate_transaction_hash: Some(candidate(4)),
                ambiguity_diagnostic: diagnostic("tracking expired"),
            },
        ] {
            assert_eq!(
                apply(
                    Some(state),
                    SubmissionObservation::TerminalCandidateFailure(failure.clone())
                ),
                Some(SubmissionRecordState::Rejected(failure.clone()))
            );
        }
    }

    #[test]
    fn tracking_confirmation_requires_durable_hash() {
        assert_eq!(
            apply_submission_observation(
                Some(SubmissionRecordState::Tracking {
                    candidate_transaction_hash: candidate(5),
                }),
                SubmissionObservation::Confirmed(hash_confirmation(candidate(6)))
            ),
            Err(SubmissionTransitionError::ConfirmationDoesNotMatchCandidate)
        );
    }

    #[test]
    fn recovery_accepts_matching_hash_or_tree_confirmation() {
        let recovering = Some(SubmissionRecordState::Recovering {
            candidate_transaction_hash: Some(candidate(7)),
            ambiguity_diagnostic: diagnostic("ambiguous"),
        });
        assert_eq!(
            apply(
                recovering.clone(),
                SubmissionObservation::Confirmed(hash_confirmation(candidate(7)))
            ),
            Some(SubmissionRecordState::Confirmed(hash_confirmation(
                candidate(7)
            )))
        );

        let tree = ValidatedChainSubmissionConfirmation::from_tree(20, vec![21]).unwrap();
        assert_eq!(
            apply(recovering, SubmissionObservation::Confirmed(tree.clone())),
            Some(SubmissionRecordState::Confirmed(tree))
        );
    }

    #[test]
    fn terminal_confirmation_replay_is_idempotent_and_conflicts_fail() {
        let confirmation = hash_confirmation(candidate(8));
        let confirmed = Some(SubmissionRecordState::Confirmed(confirmation.clone()));
        assert_eq!(
            apply(
                confirmed.clone(),
                SubmissionObservation::Confirmed(confirmation)
            ),
            confirmed
        );

        assert_eq!(
            apply_submission_observation(
                confirmed,
                SubmissionObservation::Confirmed(hash_confirmation(candidate(9)))
            ),
            Err(SubmissionTransitionError::ConflictingConfirmation)
        );
    }

    #[test]
    fn conflicting_recovery_candidate_is_an_invariant_error() {
        let recovering = Some(SubmissionRecordState::Recovering {
            candidate_transaction_hash: Some(candidate(1)),
            ambiguity_diagnostic: diagnostic("ambiguous"),
        });

        assert_eq!(
            apply_submission_observation(
                recovering,
                SubmissionObservation::UsableCandidateHash(candidate(2))
            ),
            Err(SubmissionTransitionError::ConflictingCandidateHash)
        );
    }

    #[derive(Clone, Copy, Debug)]
    enum StateCase {
        Absent,
        Submitting,
        Tracking,
        RecoveringWithoutCandidate,
        RecoveringWithCandidate,
        ConfirmedByHash,
        ConfirmedByTree,
        Rejected,
        SubmittedWithoutHash,
    }

    impl StateCase {
        const ALL: [Self; 9] = [
            Self::Absent,
            Self::Submitting,
            Self::Tracking,
            Self::RecoveringWithoutCandidate,
            Self::RecoveringWithCandidate,
            Self::ConfirmedByHash,
            Self::ConfirmedByTree,
            Self::Rejected,
            Self::SubmittedWithoutHash,
        ];

        fn state(self) -> Option<SubmissionRecordState> {
            match self {
                Self::Absent => None,
                Self::Submitting => Some(SubmissionRecordState::Submitting),
                Self::Tracking => Some(SubmissionRecordState::Tracking {
                    candidate_transaction_hash: candidate(1),
                }),
                Self::RecoveringWithoutCandidate => Some(SubmissionRecordState::Recovering {
                    candidate_transaction_hash: None,
                    ambiguity_diagnostic: diagnostic("ambiguous"),
                }),
                Self::RecoveringWithCandidate => Some(SubmissionRecordState::Recovering {
                    candidate_transaction_hash: Some(candidate(1)),
                    ambiguity_diagnostic: diagnostic("ambiguous"),
                }),
                Self::ConfirmedByHash => Some(SubmissionRecordState::Confirmed(hash_confirmation(
                    candidate(1),
                ))),
                Self::ConfirmedByTree => {
                    Some(SubmissionRecordState::Confirmed(tree_confirmation()))
                }
                Self::Rejected => Some(SubmissionRecordState::Rejected(diagnostic("rejected"))),
                Self::SubmittedWithoutHash => Some(SubmissionRecordState::SubmittedWithoutHash(
                    diagnostic("submitted"),
                )),
            }
        }
    }

    #[derive(Clone, Copy, Debug)]
    enum ObservationKind {
        ReserveFreshSubmission,
        DefinitelyUnsent,
        UsableCandidateHash,
        PossiblyDispatched,
        DefiniteRejection,
        TerminalRejection,
        CandidatePending,
        TrackingWindowExpired,
        CandidateCommittedFailure,
        TerminalCandidateFailure,
        ConfirmedByHash,
        ConfirmedByTree,
        ContinueRecovery,
        AbandonedSubmitting,
        SubmittedWithoutHash,
    }

    impl ObservationKind {
        const ALL: [Self; 15] = [
            Self::ReserveFreshSubmission,
            Self::DefinitelyUnsent,
            Self::UsableCandidateHash,
            Self::PossiblyDispatched,
            Self::DefiniteRejection,
            Self::TerminalRejection,
            Self::CandidatePending,
            Self::TrackingWindowExpired,
            Self::CandidateCommittedFailure,
            Self::TerminalCandidateFailure,
            Self::ConfirmedByHash,
            Self::ConfirmedByTree,
            Self::ContinueRecovery,
            Self::AbandonedSubmitting,
            Self::SubmittedWithoutHash,
        ];

        fn observation(self) -> SubmissionObservation {
            match self {
                Self::ReserveFreshSubmission => SubmissionObservation::ReserveFreshSubmission,
                Self::DefinitelyUnsent => SubmissionObservation::DefinitelyUnsent,
                Self::UsableCandidateHash => {
                    SubmissionObservation::UsableCandidateHash(candidate(1))
                }
                Self::PossiblyDispatched => {
                    SubmissionObservation::PossiblyDispatched(diagnostic("ambiguous"))
                }
                Self::DefiniteRejection => {
                    SubmissionObservation::DefiniteRejection(diagnostic("rejected"))
                }
                Self::TerminalRejection => {
                    SubmissionObservation::TerminalRejection(diagnostic("terminal rejection"))
                }
                Self::CandidatePending => SubmissionObservation::CandidatePending,
                Self::TrackingWindowExpired => {
                    SubmissionObservation::TrackingWindowExpired(diagnostic("expired"))
                }
                Self::CandidateCommittedFailure => {
                    SubmissionObservation::CandidateCommittedFailure(diagnostic("failed"))
                }
                Self::TerminalCandidateFailure => {
                    SubmissionObservation::TerminalCandidateFailure(diagnostic("terminal"))
                }
                Self::ConfirmedByHash => {
                    SubmissionObservation::Confirmed(hash_confirmation(candidate(1)))
                }
                Self::ConfirmedByTree => SubmissionObservation::Confirmed(tree_confirmation()),
                Self::ContinueRecovery => SubmissionObservation::ContinueRecovery,
                Self::AbandonedSubmitting => {
                    SubmissionObservation::AbandonedSubmitting(diagnostic("abandoned"))
                }
                Self::SubmittedWithoutHash => {
                    SubmissionObservation::SubmittedWithoutHash(diagnostic("submitted"))
                }
            }
        }
    }

    fn is_legal_edge(state: StateCase, observation: ObservationKind) -> bool {
        use ObservationKind as Observation;
        use StateCase as State;

        match state {
            State::Absent => matches!(observation, Observation::ReserveFreshSubmission),
            State::Submitting => matches!(
                observation,
                Observation::DefinitelyUnsent
                    | Observation::UsableCandidateHash
                    | Observation::PossiblyDispatched
                    | Observation::DefiniteRejection
                    | Observation::TerminalRejection
                    | Observation::SubmittedWithoutHash
                    | Observation::AbandonedSubmitting
            ),
            State::Tracking => matches!(
                observation,
                Observation::CandidatePending
                    | Observation::TrackingWindowExpired
                    | Observation::CandidateCommittedFailure
                    | Observation::TerminalCandidateFailure
                    | Observation::ConfirmedByHash
            ),
            State::RecoveringWithoutCandidate => matches!(
                observation,
                Observation::DefinitelyUnsent
                    | Observation::UsableCandidateHash
                    | Observation::PossiblyDispatched
                    | Observation::DefiniteRejection
                    | Observation::TerminalCandidateFailure
                    | Observation::ConfirmedByTree
                    | Observation::ContinueRecovery
                    | Observation::SubmittedWithoutHash
            ),
            State::RecoveringWithCandidate => matches!(
                observation,
                Observation::DefinitelyUnsent
                    | Observation::UsableCandidateHash
                    | Observation::PossiblyDispatched
                    | Observation::DefiniteRejection
                    | Observation::CandidatePending
                    | Observation::CandidateCommittedFailure
                    | Observation::TerminalCandidateFailure
                    | Observation::ConfirmedByHash
                    | Observation::ConfirmedByTree
                    | Observation::ContinueRecovery
                    | Observation::SubmittedWithoutHash
            ),
            State::ConfirmedByHash => matches!(observation, Observation::ConfirmedByHash),
            State::ConfirmedByTree => matches!(observation, Observation::ConfirmedByTree),
            State::Rejected => false,
            State::SubmittedWithoutHash => false,
        }
    }

    #[test]
    fn a_terminal_rejection_from_submitting_is_rejected_and_refused_after_ambiguity() {
        let rejected = apply_submission_observation(
            Some(SubmissionRecordState::Submitting),
            SubmissionObservation::TerminalRejection(diagnostic("round closed")),
        )
        .unwrap();
        assert!(matches!(
            rejected,
            Some(SubmissionRecordState::Rejected(ref stored)) if stored.message() == "round closed"
        ));
        // Once any attempt may have been dispatched the generation may be on
        // chain, so a later rejection can only be surfaced, never terminal.
        for ambiguous in [
            StateCase::RecoveringWithoutCandidate,
            StateCase::RecoveringWithCandidate,
            StateCase::Tracking,
        ] {
            assert!(apply_submission_observation(
                ambiguous.state(),
                SubmissionObservation::TerminalRejection(diagnostic("late")),
            )
            .is_err());
        }
    }

    #[test]
    fn transition_matrix_covers_every_state_and_observation_edge() {
        for state_case in StateCase::ALL {
            for observation_kind in ObservationKind::ALL {
                let result = apply_submission_observation(
                    state_case.state(),
                    observation_kind.observation(),
                );
                assert_eq!(
                    result.is_ok(),
                    is_legal_edge(state_case, observation_kind),
                    "unexpected edge result for {state_case:?} + {observation_kind:?}: {result:?}"
                );
            }
        }
    }
}
