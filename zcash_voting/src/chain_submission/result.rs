use std::fmt;

use thiserror::Error;

use super::CandidateTransactionHash;

/// Maximum encoded size of a diagnostic retained by the lifecycle.
pub const MAX_CHAIN_SUBMISSION_DIAGNOSTIC_BYTES: usize = 512;

/// The durable lifecycle states stored for lifecycle-owned submissions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChainSubmissionState {
    /// A POST reservation exists and dispatch has not yet been classified.
    Submitting,
    /// A returned transaction hash is being polled within the finite
    /// tracking window.
    Tracking,
    /// Dispatch could not be excluded and no usable hash is being tracked, or
    /// tracking ended inconclusively; further bounded retries, polling, or
    /// exact-tree recovery may still resolve the submission.
    Recovering,
    /// Terminal: the vote chain rejected a retry with "nullifier already
    /// spent" after this generation was possibly dispatched, which proves an
    /// earlier dispatch landed, but no transaction hash and no confirmation
    /// positions are available. Under exact-tree advancement one complete
    /// tree pass found no layout before this state was written.
    ///
    /// This is not a confirmation. The lifecycle performs no further retry,
    /// polling, or tree recovery for the generation, and dependent work (such
    /// as the next bundle generation) stays blocked exactly as it would behind
    /// an unresolved submission. Exhausting an invocation's POST budget never
    /// produces this state; that leaves the row `Recovering`. Hosts surface the
    /// stored diagnostic to the user rather than scheduling another pass.
    SubmittedWithoutHash,
    /// Terminal: the transaction committed successfully and its positions are
    /// recorded.
    Confirmed,
    /// Terminal: the chain definitively rejected the submission.
    Rejected,
}

/// Stable category for a bounded, redacted lifecycle diagnostic.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChainSubmissionDiagnosticKind {
    AmbiguousDispatch,
    AmbiguousAttemptsExhausted,
    NullifierAlreadySpent,
    TrackingWindowExpired,
    ChainRejected,
    ReconciliationPending,
    InvalidProtocolResponse,
    StorageFailure,
    /// A mutation response suggests an unsupported route (HTTP 404/405 or an
    /// HTML fallback). A proxy may have replaced a response after forwarding
    /// the POST, so this diagnostic does not establish non-dispatch.
    EndpointUnsupported,
}

impl ChainSubmissionDiagnosticKind {
    /// Returns the stable string discriminator used by durable storage and
    /// FFI views.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AmbiguousDispatch => "ambiguous_dispatch",
            Self::AmbiguousAttemptsExhausted => "ambiguous_attempts_exhausted",
            Self::NullifierAlreadySpent => "nullifier_already_spent",
            Self::TrackingWindowExpired => "tracking_window_expired",
            Self::ChainRejected => "chain_rejected",
            Self::ReconciliationPending => "reconciliation_pending",
            Self::InvalidProtocolResponse => "invalid_protocol_response",
            Self::StorageFailure => "storage_failure",
            Self::EndpointUnsupported => "endpoint_unsupported",
        }
    }

    /// Parses the discriminator written by [`Self::as_str`].
    pub(crate) fn from_stable_name(value: &str) -> Option<Self> {
        match value {
            "ambiguous_dispatch" => Some(Self::AmbiguousDispatch),
            "ambiguous_attempts_exhausted" => Some(Self::AmbiguousAttemptsExhausted),
            "nullifier_already_spent" => Some(Self::NullifierAlreadySpent),
            "tracking_window_expired" => Some(Self::TrackingWindowExpired),
            "chain_rejected" => Some(Self::ChainRejected),
            "reconciliation_pending" => Some(Self::ReconciliationPending),
            "invalid_protocol_response" => Some(Self::InvalidProtocolResponse),
            "storage_failure" => Some(Self::StorageFailure),
            "endpoint_unsupported" => Some(Self::EndpointUnsupported),
            _ => None,
        }
    }
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
/// can be represented.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ValidatedChainSubmissionConfirmation {
    Hash(ChainSubmissionConfirmation),
    Tree(ChainSubmissionConfirmation),
}

impl ValidatedChainSubmissionConfirmation {
    /// Returns the validated confirmation payload independent of evidence kind.
    pub(super) fn confirmation(&self) -> &ChainSubmissionConfirmation {
        match self {
            Self::Hash(confirmation) | Self::Tree(confirmation) => confirmation,
        }
    }

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

    #[allow(dead_code, reason = "returned by the chain submission coordinator")]
    pub(super) fn into_public(self) -> ChainSubmissionConfirmation {
        match self {
            Self::Hash(confirmation) | Self::Tree(confirmation) => confirmation,
        }
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
    /// POST dispatch is durably treated as submitted, but no hash or
    /// confirmation positions are available.
    SubmittedWithoutHash(ChainSubmissionDiagnostic),
    Rejected(ChainSubmissionDiagnostic),
    Cancelled,
}

impl ChainSubmissionResult {
    /// Returns the durable state represented by this result.
    ///
    /// `Cancelled` has no durable state. Every confirmation is `Confirmed`
    /// regardless of provenance; callers that care whether a layout was
    /// validated read [`ChainSubmissionConfirmation::source`].
    pub fn durable_state(&self) -> Option<ChainSubmissionState> {
        match self {
            Self::Confirmed(_) => Some(ChainSubmissionState::Confirmed),
            Self::Pending(ChainSubmissionPending::Tracking { .. }) => {
                Some(ChainSubmissionState::Tracking)
            }
            Self::Pending(ChainSubmissionPending::Recovering { .. }) => {
                Some(ChainSubmissionState::Recovering)
            }
            Self::SubmittedWithoutHash(_) => Some(ChainSubmissionState::SubmittedWithoutHash),
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
    pub(crate) fn without_state(
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
                ChainSubmissionResult::SubmittedWithoutHash(diagnostic.clone()),
                Some(ChainSubmissionState::SubmittedWithoutHash),
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
