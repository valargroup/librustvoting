use thiserror::Error;

use super::result::ValidatedChainSubmissionConfirmation;
use super::{
    CandidateTransactionHash, ChainSubmissionDiagnostic, ChainSubmissionDiagnosticKind,
    ChainSubmissionState,
};

/// Migration-only guard data with a canonical unavailable-recovery diagnostic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DigestlessRecoveryGuard {
    diagnostic: ChainSubmissionDiagnostic,
}

impl DigestlessRecoveryGuard {
    pub(super) fn new() -> Self {
        Self {
            diagnostic: ChainSubmissionDiagnostic::from_redacted_message(
                ChainSubmissionDiagnosticKind::RecoveryUnavailable,
                "version-17 chain evidence lacks generation recovery material",
            ),
        }
    }

    pub(super) fn diagnostic(&self) -> &ChainSubmissionDiagnostic {
        &self.diagnostic
    }
}

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
    /// Migration-only guard whose original generation cannot be derived.
    ///
    /// This variant cannot carry a candidate and permanently rejects every
    /// runtime lifecycle observation.
    DigestlessRecoveryGuard(DigestlessRecoveryGuard),
    Confirmed(ValidatedChainSubmissionConfirmation),
    Rejected(ChainSubmissionDiagnostic),
}

impl SubmissionRecordState {
    pub(super) fn durable_state(&self) -> ChainSubmissionState {
        match self {
            Self::Submitting => ChainSubmissionState::Submitting,
            Self::Tracking { .. } => ChainSubmissionState::Tracking,
            Self::Recovering { .. } | Self::DigestlessRecoveryGuard(_) => {
                ChainSubmissionState::Recovering
            }
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
    CandidatePending,
    TrackingWindowExpired(ChainSubmissionDiagnostic),
    CandidateCommittedFailure(ChainSubmissionDiagnostic),
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
            Ok(Some(State::Rejected(diagnostic)))
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
                candidate_transaction_hash,
                ambiguity_diagnostic,
            }),
            Observation::UsableCandidateHash(candidate),
        ) => {
            if candidate_transaction_hash.is_some_and(|existing| existing != candidate) {
                return Err(SubmissionTransitionError::ConflictingCandidateHash);
            }
            Ok(Some(State::Recovering {
                candidate_transaction_hash: Some(candidate),
                ambiguity_diagnostic,
            }))
        }
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
        (
            Some(State::Recovering {
                candidate_transaction_hash: Some(_),
                ambiguity_diagnostic,
            }),
            Observation::CandidateCommittedFailure(_),
        ) => Ok(Some(State::Recovering {
            candidate_transaction_hash: None,
            ambiguity_diagnostic,
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
        migration_only if migration_only.is_migration_only() => {
            Err(SubmissionTransitionError::MigrationOnlyConfirmationOutsideMigration)
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
        ValidatedChainSubmissionConfirmation::LegacyImport(_)
        | ValidatedChainSubmissionConfirmation::LegacyProjection(_) => {
            Err(SubmissionTransitionError::MigrationOnlyConfirmationOutsideMigration)
        }
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
            Self::CandidatePending => "candidate_pending",
            Self::TrackingWindowExpired(_) => "tracking_window_expired",
            Self::CandidateCommittedFailure(_) => "candidate_committed_failure",
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
    #[error("imported legacy confirmation is valid only during database migration")]
    MigrationOnlyConfirmationOutsideMigration,
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
        ChainSubmissionDiagnostic::from_redacted_message(
            ChainSubmissionDiagnosticKind::AmbiguousDispatch,
            message,
        )
    }

    fn hash_confirmation(hash: CandidateTransactionHash) -> ValidatedChainSubmissionConfirmation {
        ValidatedChainSubmissionConfirmation::from_hash(hash, 10, vec![11]).unwrap()
    }

    fn tree_confirmation() -> ValidatedChainSubmissionConfirmation {
        ValidatedChainSubmissionConfirmation::from_tree(10, vec![11]).unwrap()
    }

    fn legacy_projection_confirmation() -> ValidatedChainSubmissionConfirmation {
        ValidatedChainSubmissionConfirmation::from_legacy_projection(10, 11).unwrap()
    }

    fn legacy_import_confirmation() -> ValidatedChainSubmissionConfirmation {
        ValidatedChainSubmissionConfirmation::from_legacy_import(Some(candidate(1)), 10, vec![11])
            .unwrap()
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

        for observation in [
            SubmissionObservation::CandidatePending,
            SubmissionObservation::DefiniteRejection(diagnostic("later rejection")),
            SubmissionObservation::PossiblyDispatched(diagnostic("second ambiguity")),
            SubmissionObservation::DefinitelyUnsent,
            SubmissionObservation::ContinueRecovery,
        ] {
            assert_eq!(apply(with_candidate.clone(), observation), with_candidate);
        }

        assert_eq!(
            with_candidate,
            Some(SubmissionRecordState::Recovering {
                candidate_transaction_hash: Some(candidate(2)),
                ambiguity_diagnostic: first_ambiguity,
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
    fn committed_failure_rejects_tracking_but_not_recovery() {
        let rejected = diagnostic("transaction committed unsuccessfully");
        let tracking = Some(SubmissionRecordState::Tracking {
            candidate_transaction_hash: candidate(4),
        });
        assert_eq!(
            apply(
                tracking,
                SubmissionObservation::CandidateCommittedFailure(rejected.clone())
            ),
            Some(SubmissionRecordState::Rejected(rejected.clone()))
        );

        let recovering = Some(SubmissionRecordState::Recovering {
            candidate_transaction_hash: Some(candidate(4)),
            ambiguity_diagnostic: diagnostic("original ambiguity"),
        });
        let original_ambiguity = diagnostic("original ambiguity");
        assert_eq!(
            apply(
                recovering,
                SubmissionObservation::CandidateCommittedFailure(rejected)
            ),
            Some(SubmissionRecordState::Recovering {
                candidate_transaction_hash: None,
                ambiguity_diagnostic: original_ambiguity,
            })
        );
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
    fn digestless_guard_rejects_every_runtime_observation() {
        let guard_data = DigestlessRecoveryGuard::new();
        assert_eq!(
            guard_data.diagnostic().kind(),
            ChainSubmissionDiagnosticKind::RecoveryUnavailable
        );
        let guard = Some(SubmissionRecordState::DigestlessRecoveryGuard(guard_data));

        for observation_kind in ObservationKind::ALL {
            assert!(
                apply_submission_observation(guard.clone(), observation_kind.observation())
                    .is_err()
            );
        }
    }

    #[test]
    fn legacy_projection_confirmation_is_terminal_and_never_produced_at_runtime() {
        let legacy = Some(SubmissionRecordState::Confirmed(
            legacy_projection_confirmation(),
        ));

        // A migrated projection is terminal: only its own identical replay is a
        // no-op, and no runtime observation can reach or recreate it.
        for observation_kind in ObservationKind::ALL {
            let result =
                apply_submission_observation(legacy.clone(), observation_kind.observation());
            match observation_kind {
                ObservationKind::ConfirmedByLegacyProjection => {
                    assert_eq!(result, Ok(legacy.clone()))
                }
                _ => assert!(result.is_err(), "{observation_kind:?} must not apply"),
            }
        }

        for state in [
            SubmissionRecordState::Tracking {
                candidate_transaction_hash: candidate(1),
            },
            SubmissionRecordState::Recovering {
                candidate_transaction_hash: None,
                ambiguity_diagnostic: diagnostic("ambiguous"),
            },
        ] {
            assert_eq!(
                apply_submission_observation(
                    Some(state),
                    SubmissionObservation::Confirmed(legacy_projection_confirmation()),
                ),
                Err(SubmissionTransitionError::MigrationOnlyConfirmationOutsideMigration)
            );
        }
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
        DigestlessRecoveryGuard,
        ConfirmedByHash,
        ConfirmedByTree,
        ConfirmedByLegacyImport,
        ConfirmedByLegacyProjection,
        Rejected,
    }

    impl StateCase {
        const ALL: [Self; 11] = [
            Self::Absent,
            Self::Submitting,
            Self::Tracking,
            Self::RecoveringWithoutCandidate,
            Self::RecoveringWithCandidate,
            Self::DigestlessRecoveryGuard,
            Self::ConfirmedByHash,
            Self::ConfirmedByTree,
            Self::ConfirmedByLegacyImport,
            Self::ConfirmedByLegacyProjection,
            Self::Rejected,
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
                Self::DigestlessRecoveryGuard => Some(
                    SubmissionRecordState::DigestlessRecoveryGuard(DigestlessRecoveryGuard::new()),
                ),
                Self::ConfirmedByHash => Some(SubmissionRecordState::Confirmed(hash_confirmation(
                    candidate(1),
                ))),
                Self::ConfirmedByTree => {
                    Some(SubmissionRecordState::Confirmed(tree_confirmation()))
                }
                Self::ConfirmedByLegacyImport => Some(SubmissionRecordState::Confirmed(
                    legacy_import_confirmation(),
                )),
                Self::ConfirmedByLegacyProjection => Some(SubmissionRecordState::Confirmed(
                    legacy_projection_confirmation(),
                )),
                Self::Rejected => Some(SubmissionRecordState::Rejected(diagnostic("rejected"))),
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
        CandidatePending,
        TrackingWindowExpired,
        CandidateCommittedFailure,
        ConfirmedByHash,
        ConfirmedByTree,
        ConfirmedByLegacyImport,
        ConfirmedByLegacyProjection,
        ContinueRecovery,
        AbandonedSubmitting,
    }

    impl ObservationKind {
        const ALL: [Self; 14] = [
            Self::ReserveFreshSubmission,
            Self::DefinitelyUnsent,
            Self::UsableCandidateHash,
            Self::PossiblyDispatched,
            Self::DefiniteRejection,
            Self::CandidatePending,
            Self::TrackingWindowExpired,
            Self::CandidateCommittedFailure,
            Self::ConfirmedByHash,
            Self::ConfirmedByTree,
            Self::ConfirmedByLegacyImport,
            Self::ConfirmedByLegacyProjection,
            Self::ContinueRecovery,
            Self::AbandonedSubmitting,
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
                Self::CandidatePending => SubmissionObservation::CandidatePending,
                Self::TrackingWindowExpired => {
                    SubmissionObservation::TrackingWindowExpired(diagnostic("expired"))
                }
                Self::CandidateCommittedFailure => {
                    SubmissionObservation::CandidateCommittedFailure(diagnostic("failed"))
                }
                Self::ConfirmedByHash => {
                    SubmissionObservation::Confirmed(hash_confirmation(candidate(1)))
                }
                Self::ConfirmedByTree => SubmissionObservation::Confirmed(tree_confirmation()),
                Self::ConfirmedByLegacyImport => {
                    SubmissionObservation::Confirmed(legacy_import_confirmation())
                }
                Self::ConfirmedByLegacyProjection => {
                    SubmissionObservation::Confirmed(legacy_projection_confirmation())
                }
                Self::ContinueRecovery => SubmissionObservation::ContinueRecovery,
                Self::AbandonedSubmitting => {
                    SubmissionObservation::AbandonedSubmitting(diagnostic("abandoned"))
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
                    | Observation::AbandonedSubmitting
            ),
            State::Tracking => matches!(
                observation,
                Observation::CandidatePending
                    | Observation::TrackingWindowExpired
                    | Observation::CandidateCommittedFailure
                    | Observation::ConfirmedByHash
            ),
            State::RecoveringWithoutCandidate => matches!(
                observation,
                Observation::DefinitelyUnsent
                    | Observation::UsableCandidateHash
                    | Observation::PossiblyDispatched
                    | Observation::DefiniteRejection
                    | Observation::ConfirmedByTree
                    | Observation::ContinueRecovery
            ),
            State::RecoveringWithCandidate => matches!(
                observation,
                Observation::DefinitelyUnsent
                    | Observation::UsableCandidateHash
                    | Observation::PossiblyDispatched
                    | Observation::DefiniteRejection
                    | Observation::CandidatePending
                    | Observation::CandidateCommittedFailure
                    | Observation::ConfirmedByHash
                    | Observation::ConfirmedByTree
                    | Observation::ContinueRecovery
            ),
            State::DigestlessRecoveryGuard => false,
            State::ConfirmedByHash => matches!(observation, Observation::ConfirmedByHash),
            State::ConfirmedByTree => matches!(observation, Observation::ConfirmedByTree),
            State::ConfirmedByLegacyImport => {
                matches!(observation, Observation::ConfirmedByLegacyImport)
            }
            State::ConfirmedByLegacyProjection => {
                matches!(observation, Observation::ConfirmedByLegacyProjection)
            }
            State::Rejected => false,
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
