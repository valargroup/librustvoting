use std::fmt;

use thiserror::Error;

use super::CandidateTransactionHash;

/// Maximum encoded size of a diagnostic retained by the lifecycle.
pub const MAX_CHAIN_SUBMISSION_DIAGNOSTIC_BYTES: usize = 512;

/// The durable lifecycle states stored for native and migrated submissions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChainSubmissionState {
    Submitting,
    Tracking,
    Recovering,
    Confirmed,
    /// Migration-only confirmation whose generation layout could not be
    /// re-derived from version-17 recovery data.
    LegacyConfirmed,
    Rejected,
}

/// Stable category for a bounded, redacted lifecycle diagnostic.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChainSubmissionDiagnosticKind {
    AmbiguousDispatch,
    TrackingWindowExpired,
    ChainRejected,
    ReconciliationPending,
    InvalidProtocolResponse,
    RecoveryUnavailable,
    StorageFailure,
}

/// A bounded UTF-8 diagnostic that is safe for durable storage.
///
/// The constructor's name makes the trust boundary explicit: callers must
/// redact secrets before supplying the message. Control characters are
/// escaped and retained only when the complete escape sequence fits.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ChainSubmissionDiagnostic {
    kind: ChainSubmissionDiagnosticKind,
    redacted_message: String,
}

impl ChainSubmissionDiagnostic {
    pub fn from_redacted_message(
        kind: ChainSubmissionDiagnosticKind,
        redacted_message: impl AsRef<str>,
    ) -> Self {
        let redacted_message = bounded_redacted_text(redacted_message.as_ref());
        Self {
            kind,
            redacted_message,
        }
    }

    pub fn kind(&self) -> ChainSubmissionDiagnosticKind {
        self.kind
    }

    pub fn message(&self) -> &str {
        &self.redacted_message
    }
}

fn bounded_redacted_text(redacted_message: &str) -> String {
    let mut bounded = String::with_capacity(
        redacted_message
            .len()
            .min(MAX_CHAIN_SUBMISSION_DIAGNOSTIC_BYTES),
    );

    for character in redacted_message.chars() {
        let escaped_length = character
            .escape_default()
            .map(char::len_utf8)
            .sum::<usize>();
        if bounded.len() + escaped_length > MAX_CHAIN_SUBMISSION_DIAGNOSTIC_BYTES {
            break;
        }
        bounded.extend(character.escape_default());
    }

    bounded
}

/// Evidence source used by an atomic confirmation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChainSubmissionConfirmationSource {
    Hash,
    Tree,
    /// Version-17 confirmation whose generation and exact layout were
    /// reconstructed and validated during migration.
    LegacyImport,
    /// Version-17 singleton positions preserved without claiming a validated
    /// generation digest or output layout.
    LegacyProjection,
}

/// Exact terminal data whose identical replay is idempotent.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ChainSubmissionConfirmation {
    source: ChainSubmissionConfirmationSource,
    transaction_hash: Option<CandidateTransactionHash>,
    final_van_position: u64,
    vote_commitment_positions: Vec<u64>,
}

impl ChainSubmissionConfirmation {
    pub fn source(&self) -> ChainSubmissionConfirmationSource {
        self.source
    }

    pub fn transaction_hash(&self) -> Option<CandidateTransactionHash> {
        self.transaction_hash
    }

    pub fn final_van_position(&self) -> u64 {
        self.final_van_position
    }

    pub fn vote_commitment_positions(&self) -> &[u64] {
        &self.vote_commitment_positions
    }
}

/// Generation-validated terminal evidence accepted by the runtime lifecycle.
///
/// The variant determines which combinations of source and transaction hash
/// can be represented. Legacy projections deliberately use a separate type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ValidatedChainSubmissionConfirmation {
    Hash(ChainSubmissionConfirmation),
    Tree(ChainSubmissionConfirmation),
    LegacyImport(ChainSubmissionConfirmation),
}

impl ValidatedChainSubmissionConfirmation {
    #[allow(dead_code, reason = "used by chain confirmation")]
    pub(super) fn from_hash(
        transaction_hash: CandidateTransactionHash,
        final_van_position: u64,
        vote_commitment_positions: Vec<u64>,
    ) -> Result<Self, ChainSubmissionConfirmationError> {
        validate_sqlite_positions(final_van_position, &vote_commitment_positions)?;
        Ok(Self::Hash(ChainSubmissionConfirmation {
            source: ChainSubmissionConfirmationSource::Hash,
            transaction_hash: Some(transaction_hash),
            final_van_position,
            vote_commitment_positions,
        }))
    }

    #[allow(dead_code, reason = "used by tree recovery")]
    pub(super) fn from_tree(
        final_van_position: u64,
        vote_commitment_positions: Vec<u64>,
    ) -> Result<Self, ChainSubmissionConfirmationError> {
        validate_sqlite_positions(final_van_position, &vote_commitment_positions)?;
        Ok(Self::Tree(ChainSubmissionConfirmation {
            source: ChainSubmissionConfirmationSource::Tree,
            transaction_hash: None,
            final_van_position,
            vote_commitment_positions,
        }))
    }

    #[allow(dead_code, reason = "used by the version-18 migration")]
    pub(super) fn from_legacy_import(
        transaction_hash: Option<CandidateTransactionHash>,
        final_van_position: u64,
        vote_commitment_positions: Vec<u64>,
    ) -> Result<Self, ChainSubmissionConfirmationError> {
        validate_sqlite_positions(final_van_position, &vote_commitment_positions)?;
        Ok(Self::LegacyImport(ChainSubmissionConfirmation {
            source: ChainSubmissionConfirmationSource::LegacyImport,
            transaction_hash,
            final_van_position,
            vote_commitment_positions,
        }))
    }

    #[allow(dead_code, reason = "returned by the chain submission coordinator")]
    pub(super) fn into_public(self) -> ChainSubmissionConfirmation {
        match self {
            Self::Hash(confirmation)
            | Self::Tree(confirmation)
            | Self::LegacyImport(confirmation) => confirmation,
        }
    }
}

/// Migration-only positions that lack a validated generation layout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct LegacyProjectionConfirmation(ChainSubmissionConfirmation);

impl LegacyProjectionConfirmation {
    #[allow(dead_code, reason = "used by the version-18 migration")]
    pub(super) fn from_positions(
        final_van_position: u64,
        vote_commitment_position: u64,
    ) -> Result<Self, ChainSubmissionConfirmationError> {
        validate_sqlite_positions(final_van_position, &[vote_commitment_position])?;
        Ok(Self(ChainSubmissionConfirmation {
            source: ChainSubmissionConfirmationSource::LegacyProjection,
            transaction_hash: None,
            final_van_position,
            vote_commitment_positions: vec![vote_commitment_position],
        }))
    }

    #[allow(dead_code, reason = "returned for migration projections")]
    pub(super) fn into_public(self) -> ChainSubmissionConfirmation {
        self.0
    }
}

fn validate_sqlite_positions(
    final_van_position: u64,
    vote_commitment_positions: &[u64],
) -> Result<(), ChainSubmissionConfirmationError> {
    let maximum_position = i64::MAX as u64;
    if final_van_position > maximum_position
        || vote_commitment_positions
            .iter()
            .any(|position| *position > maximum_position)
    {
        return Err(ChainSubmissionConfirmationError::PositionOutOfRange);
    }
    Ok(())
}

/// Invalid terminal data that cannot be represented by the durable schema.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum ChainSubmissionConfirmationError {
    #[error("chain tree positions must fit SQLite's signed integer range")]
    PositionOutOfRange,
}

/// Non-terminal result that tells a host which bounded pass to schedule next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChainSubmissionPending {
    Tracking {
        candidate_transaction_hash: CandidateTransactionHash,
    },
    Recovering {
        candidate_transaction_hash: Option<CandidateTransactionHash>,
        diagnostic: ChainSubmissionDiagnostic,
    },
}

/// Authoritative outcome returned by a lifecycle advancement call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChainSubmissionResult {
    Confirmed(ChainSubmissionConfirmation),
    Pending(ChainSubmissionPending),
    Rejected(ChainSubmissionDiagnostic),
    Cancelled,
}

impl ChainSubmissionResult {
    /// Returns the durable state represented by this result.
    ///
    /// `Cancelled` has no durable state. A migrated legacy projection is
    /// intentionally distinguished from a generation-validated confirmation.
    pub fn durable_state(&self) -> Option<ChainSubmissionState> {
        match self {
            Self::Confirmed(confirmation)
                if confirmation.source() == ChainSubmissionConfirmationSource::LegacyProjection =>
            {
                Some(ChainSubmissionState::LegacyConfirmed)
            }
            Self::Confirmed(_) => Some(ChainSubmissionState::Confirmed),
            Self::Pending(ChainSubmissionPending::Tracking { .. }) => {
                Some(ChainSubmissionState::Tracking)
            }
            Self::Pending(ChainSubmissionPending::Recovering { .. }) => {
                Some(ChainSubmissionState::Recovering)
            }
            Self::Rejected(_) => Some(ChainSubmissionState::Rejected),
            Self::Cancelled => None,
        }
    }
}

/// Operational failure category, distinct from a durable chain outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChainSubmissionFailureKind {
    InvalidInput,
    InvariantViolation,
    Storage,
    Transport,
    Protocol,
}

/// Why the state attached to an operational failure is authoritative.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChainSubmissionStateEvidence {
    /// The state is known to be durable.
    Durable,
    /// Dispatch may have occurred, but persistence of `Recovering` failed.
    /// The caller must preserve the ambiguity and must not treat the operation
    /// as cancelled or definitely unsent.
    KnownPossiblyDispatched,
}

/// Strongest truthful lifecycle state known when an operation failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ChainSubmissionFailureState {
    state: ChainSubmissionState,
    evidence: ChainSubmissionStateEvidence,
}

impl ChainSubmissionFailureState {
    pub fn state(&self) -> ChainSubmissionState {
        self.state
    }

    pub fn evidence(&self) -> ChainSubmissionStateEvidence {
        self.evidence
    }

    fn durable(state: ChainSubmissionState) -> Self {
        Self {
            state,
            evidence: ChainSubmissionStateEvidence::Durable,
        }
    }

    fn known_possibly_dispatched() -> Self {
        Self {
            state: ChainSubmissionState::Recovering,
            evidence: ChainSubmissionStateEvidence::KnownPossiblyDispatched,
        }
    }
}

/// Failure of one bounded advancement pass.
///
/// `strongest_state` distinguishes durable state from ambiguity known to the
/// current call when a recovery transition could not itself be persisted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainSubmissionFailure {
    kind: ChainSubmissionFailureKind,
    strongest_state: Option<ChainSubmissionFailureState>,
    redacted_message: String,
}

#[allow(dead_code, reason = "constructed by protocol and coordinator phases")]
impl ChainSubmissionFailure {
    pub(super) fn without_state(
        kind: ChainSubmissionFailureKind,
        redacted_message: impl AsRef<str>,
    ) -> Self {
        Self {
            kind,
            strongest_state: None,
            redacted_message: bounded_redacted_text(redacted_message.as_ref()),
        }
    }

    pub(super) fn with_durable_state(
        kind: ChainSubmissionFailureKind,
        durable_state: ChainSubmissionState,
        redacted_message: impl AsRef<str>,
    ) -> Self {
        Self {
            kind,
            strongest_state: Some(ChainSubmissionFailureState::durable(durable_state)),
            redacted_message: bounded_redacted_text(redacted_message.as_ref()),
        }
    }

    pub(super) fn with_known_possible_dispatch(
        kind: ChainSubmissionFailureKind,
        redacted_message: impl AsRef<str>,
    ) -> Self {
        Self {
            kind,
            strongest_state: Some(ChainSubmissionFailureState::known_possibly_dispatched()),
            redacted_message: bounded_redacted_text(redacted_message.as_ref()),
        }
    }

    pub fn kind(&self) -> ChainSubmissionFailureKind {
        self.kind
    }

    pub fn strongest_state(&self) -> Option<ChainSubmissionFailureState> {
        self.strongest_state
    }

    pub fn message(&self) -> &str {
        &self.redacted_message
    }
}

impl fmt::Display for ChainSubmissionFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "chain submission failed: {}",
            self.redacted_message
        )
    }
}

impl std::error::Error for ChainSubmissionFailure {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_is_escaped_and_bounded_on_utf8_boundary() {
        let input = format!("line one\n{}", "é".repeat(400));
        let diagnostic = ChainSubmissionDiagnostic::from_redacted_message(
            ChainSubmissionDiagnosticKind::AmbiguousDispatch,
            input,
        );

        assert!(!diagnostic.message().contains('\n'));
        assert!(diagnostic.message().contains("\\n"));
        assert!(diagnostic.message().len() <= MAX_CHAIN_SUBMISSION_DIAGNOSTIC_BYTES);
        assert!(std::str::from_utf8(diagnostic.message().as_bytes()).is_ok());
    }

    #[test]
    fn diagnostic_stops_before_an_incomplete_escape_sequence() {
        let input = format!("{}\nignored", "a".repeat(511));
        let diagnostic = ChainSubmissionDiagnostic::from_redacted_message(
            ChainSubmissionDiagnosticKind::AmbiguousDispatch,
            input,
        );

        assert_eq!(diagnostic.message(), "a".repeat(511));
    }

    #[test]
    fn diagnostic_bounds_large_input_while_escaping() {
        let diagnostic = ChainSubmissionDiagnostic::from_redacted_message(
            ChainSubmissionDiagnosticKind::AmbiguousDispatch,
            "\n".repeat(1_000_000),
        );

        assert_eq!(
            diagnostic.message().len(),
            MAX_CHAIN_SUBMISSION_DIAGNOSTIC_BYTES
        );
        assert!(diagnostic
            .message()
            .bytes()
            .all(|byte| matches!(byte, b'\\' | b'n')));
    }

    #[test]
    fn pending_tracking_always_contains_candidate_hash() {
        let candidate = CandidateTransactionHash::from_bytes([3; 32]);
        assert_eq!(
            ChainSubmissionPending::Tracking {
                candidate_transaction_hash: candidate
            },
            ChainSubmissionPending::Tracking {
                candidate_transaction_hash: candidate
            }
        );
    }

    #[test]
    fn confirmation_accepts_zero_and_rejects_positions_outside_sqlite_range() {
        assert!(ValidatedChainSubmissionConfirmation::from_tree(0, vec![0]).is_ok());
        assert_eq!(
            ValidatedChainSubmissionConfirmation::from_tree(i64::MAX as u64 + 1, vec![]),
            Err(ChainSubmissionConfirmationError::PositionOutOfRange)
        );
        assert_eq!(
            ValidatedChainSubmissionConfirmation::from_tree(0, vec![i64::MAX as u64 + 1]),
            Err(ChainSubmissionConfirmationError::PositionOutOfRange)
        );
    }

    #[test]
    fn legacy_projection_is_distinct_from_validated_legacy_import() {
        let legacy_projection = LegacyProjectionConfirmation::from_positions(4, 5)
            .unwrap()
            .into_public();
        let legacy_import = ValidatedChainSubmissionConfirmation::from_legacy_import(
            Some(CandidateTransactionHash::from_bytes([7; 32])),
            4,
            vec![5],
        )
        .unwrap()
        .into_public();

        assert_eq!(
            legacy_projection.source(),
            ChainSubmissionConfirmationSource::LegacyProjection
        );
        assert_eq!(legacy_projection.transaction_hash(), None);
        assert_eq!(
            ChainSubmissionResult::Confirmed(legacy_projection).durable_state(),
            Some(ChainSubmissionState::LegacyConfirmed)
        );
        assert_eq!(
            legacy_import.source(),
            ChainSubmissionConfirmationSource::LegacyImport
        );
        assert_eq!(
            ChainSubmissionResult::Confirmed(legacy_import).durable_state(),
            Some(ChainSubmissionState::Confirmed)
        );
    }

    #[test]
    fn every_public_result_maps_to_its_durable_state() {
        let candidate_transaction_hash = CandidateTransactionHash::from_bytes([8; 32]);
        let diagnostic = ChainSubmissionDiagnostic::from_redacted_message(
            ChainSubmissionDiagnosticKind::ReconciliationPending,
            "pending",
        );
        let cases = [
            (
                ChainSubmissionResult::Pending(ChainSubmissionPending::Tracking {
                    candidate_transaction_hash,
                }),
                Some(ChainSubmissionState::Tracking),
            ),
            (
                ChainSubmissionResult::Pending(ChainSubmissionPending::Recovering {
                    candidate_transaction_hash: None,
                    diagnostic: diagnostic.clone(),
                }),
                Some(ChainSubmissionState::Recovering),
            ),
            (
                ChainSubmissionResult::Rejected(diagnostic),
                Some(ChainSubmissionState::Rejected),
            ),
            (ChainSubmissionResult::Cancelled, None),
        ];

        for (result, expected_state) in cases {
            assert_eq!(result.durable_state(), expected_state);
        }
    }

    #[test]
    fn operational_failure_distinguishes_durable_from_known_ambiguity() {
        let stateless = ChainSubmissionFailure::without_state(
            ChainSubmissionFailureKind::InvalidInput,
            "invalid request",
        );
        assert_eq!(stateless.kind(), ChainSubmissionFailureKind::InvalidInput);
        assert_eq!(stateless.strongest_state(), None);

        let durable = ChainSubmissionFailure::with_durable_state(
            ChainSubmissionFailureKind::Storage,
            ChainSubmissionState::Submitting,
            "reservation persisted",
        );
        assert_eq!(
            durable.strongest_state(),
            Some(ChainSubmissionFailureState {
                state: ChainSubmissionState::Submitting,
                evidence: ChainSubmissionStateEvidence::Durable,
            })
        );

        let ambiguous = ChainSubmissionFailure::with_known_possible_dispatch(
            ChainSubmissionFailureKind::Storage,
            format!("normalization failed\n{}", "x".repeat(600)),
        );
        assert_eq!(
            ambiguous.strongest_state(),
            Some(ChainSubmissionFailureState {
                state: ChainSubmissionState::Recovering,
                evidence: ChainSubmissionStateEvidence::KnownPossiblyDispatched,
            })
        );
        assert!(!ambiguous.message().contains('\n'));
        assert!(ambiguous.message().len() <= MAX_CHAIN_SUBMISSION_DIAGNOSTIC_BYTES);
    }
}
