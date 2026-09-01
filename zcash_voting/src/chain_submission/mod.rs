//! Durable vote-chain submission and confirmation lifecycle.
//!
//! This module is the public facade. Private children own payload recovery,
//! durable attempt evidence, reconciliation, and process-local coordination.

use crate::{
    chain::{ChainClient, ChainError},
    storage::VotingDb,
    types::VotingError,
    wire::{DelegationConfirmation, VoteBatchConfirmation, VoteConfirmation},
};

mod attempt_journal;
mod coordination;
mod lifecycle;
mod payload;
mod reconciliation;
mod storage_util;

pub(crate) use attempt_journal::{
    attempt_protected_vote_rows, can_still_learn_a_hash, delegation_candidate_hashes,
    delegation_candidates, vote_candidate_hashes, vote_candidates,
};
use attempt_journal::{
    candidate_transaction_hashes, delete_definitely_unsent_attempt, has_ambiguous_attempt,
    journal_accepted_hash, record_attempt_evidence, reserve_dispatch_attempt,
    retire_failed_candidate,
};
pub(crate) use coordination::has_in_flight_at_or_after;
#[cfg(test)]
use coordination::in_flight_count;
#[cfg(test)]
pub(crate) use coordination::interrupted_reservation_grace_secs;
use coordination::{
    identity_operation_lock, in_flight_for_round, refresh_attempt_reservation, InFlightAttempt,
    FUTURE_STAMP_TOLERANCE_SECS, INTERRUPTED_RESERVATION_GRACE_SECS, RESERVATION_HEARTBEAT,
};
use payload::{
    canonical_batch_payload, canonical_singleton_vote_payload, decode_canonical_array,
    delegation_payload_rebuild, stale_generation_error, PayloadRebuild,
};
#[cfg(test)]
use reconciliation::apply_confirmation;
use reconciliation::durable_confirmation_hash;
use storage_util::{internal, now_seconds};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChainSubmissionKind {
    /// A delegation transaction that creates a vote-authority note.
    Delegation,
    /// A singleton cast-vote transaction for one proposal.
    Vote,
    /// One atomic cast-vote transaction containing several proposals.
    VoteBatch,
}

impl ChainSubmissionKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Delegation => "delegation",
            Self::Vote => "vote",
            Self::VoteBatch => "vote_batch",
        }
    }

    fn endpoint(self) -> &'static str {
        match self {
            Self::Delegation => "delegate-vote",
            Self::Vote => "cast-vote",
            Self::VoteBatch => "cast-vote-batch",
        }
    }
}

/// The durable voting identity one chain mutation belongs to.
///
/// Each variant carries exactly the fields its mutation requires, so a caller
/// cannot pair a submission kind with an unrelated proposal or batch digest.
/// The storage `CHECK` on `chain_submission_attempts` enforces the same pairing
/// at rest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChainSubmissionIdentity {
    /// A delegation for one durable round bundle.
    Delegation {
        /// Canonical vote-round identifier.
        round_id: String,
        /// Durable bundle index within the round.
        bundle_index: u32,
    },
    /// A singleton vote for one proposal in one bundle.
    Vote {
        /// Canonical vote-round identifier.
        round_id: String,
        /// Durable bundle index within the round.
        bundle_index: u32,
        /// Proposal whose persisted vote generation is submitted.
        proposal_id: u32,
    },
    /// An atomic vote batch identified by its signed batch digest.
    VoteBatch {
        /// Canonical vote-round identifier.
        round_id: String,
        /// Durable bundle index shared by every batch member.
        bundle_index: u32,
        /// Digest binding the ordered persisted batch members.
        batch_digest: [u8; 32],
    },
}

impl ChainSubmissionIdentity {
    /// Returns the durable round this submission belongs to.
    pub fn round_id(&self) -> &str {
        match self {
            Self::Delegation { round_id, .. }
            | Self::Vote { round_id, .. }
            | Self::VoteBatch { round_id, .. } => round_id,
        }
    }

    /// Returns the mutation kind encoded by this identity variant.
    pub fn kind(&self) -> ChainSubmissionKind {
        match self {
            Self::Delegation { .. } => ChainSubmissionKind::Delegation,
            Self::Vote { .. } => ChainSubmissionKind::Vote,
            Self::VoteBatch { .. } => ChainSubmissionKind::VoteBatch,
        }
    }

    /// Returns the durable round-bundle index.
    pub fn bundle_index(&self) -> u32 {
        match self {
            Self::Delegation { bundle_index, .. }
            | Self::Vote { bundle_index, .. }
            | Self::VoteBatch { bundle_index, .. } => *bundle_index,
        }
    }

    /// The proposal this identity votes on, present exactly for singleton votes.
    pub fn proposal_id(&self) -> Option<u32> {
        match self {
            Self::Vote { proposal_id, .. } => Some(*proposal_id),
            Self::Delegation { .. } | Self::VoteBatch { .. } => None,
        }
    }

    /// The batch sighash digest, present exactly for atomic vote batches.
    pub fn batch_digest(&self) -> Option<[u8; 32]> {
        match self {
            Self::VoteBatch { batch_digest, .. } => Some(*batch_digest),
            Self::Delegation { .. } | Self::Vote { .. } => None,
        }
    }

    fn require_proposal_id(&self) -> Result<u32, VotingError> {
        self.proposal_id().ok_or_else(|| VotingError::Internal {
            message: "singleton vote submission identity has no proposal".to_string(),
        })
    }

    fn require_batch_digest(&self) -> Result<[u8; 32], VotingError> {
        self.batch_digest().ok_or_else(|| VotingError::Internal {
            message: "atomic vote batch submission identity has no batch digest".to_string(),
        })
    }

    /// Constructs a delegation identity.
    pub fn delegation(round_id: impl Into<String>, bundle_index: u32) -> Self {
        Self::Delegation {
            round_id: round_id.into(),
            bundle_index,
        }
    }

    /// Constructs a singleton-vote identity.
    pub fn vote(round_id: impl Into<String>, bundle_index: u32, proposal_id: u32) -> Self {
        Self::Vote {
            round_id: round_id.into(),
            bundle_index,
            proposal_id,
        }
    }

    /// Constructs an atomic vote-batch identity.
    pub fn vote_batch(
        round_id: impl Into<String>,
        bundle_index: u32,
        batch_digest: [u8; 32],
    ) -> Self {
        Self::VoteBatch {
            round_id: round_id.into(),
            bundle_index,
            batch_digest,
        }
    }

    fn proposal_key(&self) -> i64 {
        self.proposal_id().map(i64::from).unwrap_or(-1)
    }

    fn batch_key(&self) -> &[u8] {
        match self {
            Self::VoteBatch { batch_digest, .. } => batch_digest,
            Self::Delegation { .. } | Self::Vote { .. } => &[],
        }
    }

    fn lock_key(&self, wallet_id: &str) -> String {
        format!(
            "{wallet_id}/{}/{}/{}/{}",
            self.round_id(),
            self.kind().as_str(),
            self.bundle_index(),
            self.proposal_key()
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// Event-derived confirmation data for one supported chain mutation.
pub enum ChainConfirmation {
    /// A confirmed delegation and its resulting VAN position.
    Delegation(DelegationConfirmation),
    /// A confirmed singleton vote and its resulting tree positions.
    Vote(VoteConfirmation),
    /// A confirmed atomic batch and all ordered member positions.
    VoteBatch(VoteBatchConfirmation),
}

impl ChainConfirmation {
    /// The canonical chain transaction hash carried by this confirmation.
    pub fn tx_hash(&self) -> &str {
        match self {
            Self::Delegation(confirmation) => &confirmation.tx_hash,
            Self::Vote(confirmation) => &confirmation.tx_hash,
            Self::VoteBatch(confirmation) => &confirmation.tx_hash,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// Result of one durable submission or reconciliation operation.
pub enum ChainLifecycleOutcome {
    /// CheckTx accepted the transaction; commitment is not yet known.
    Accepted { tx_hash: String },
    /// This operation parsed and durably applied a committed success.
    Confirmed { confirmation: ChainConfirmation },
    /// This submission was confirmed by an earlier call, and its event-derived
    /// positions are already recorded in the voting database.
    ///
    /// Distinct from [`ChainLifecycleOutcome::Confirmed`], which carries the
    /// confirmation this call just parsed. The exact per-transaction VAN
    /// position is not recoverable afterwards — `bundles.van_leaf_position` is a
    /// single pointer that later confirmations on the same bundle advance — so
    /// this variant reports the settled fact without inventing event data.
    AlreadyConfirmed { tx_hash: String },
    /// Known candidates are valid but not yet committed.
    Pending { known_tx_hashes: Vec<String> },
    /// This exact attempt or candidate failed definitively.
    Rejected { code: u32, log: String },
    /// A spent nullifier was reported, but no known candidate proved success.
    AlreadySpentUnresolved {
        known_tx_hashes: Vec<String>,
        log: String,
    },
    /// A request or lookup may still settle and cannot be classified further.
    OutcomeUnknown {
        known_tx_hashes: Vec<String>,
        message: String,
    },
    /// Cancellation was observed before this call acquired ambiguous evidence.
    Cancelled,
}

#[derive(Debug)]
/// Failure to perform a durable chain lifecycle operation.
pub enum ChainLifecycleError {
    /// Durable voting-state validation or persistence failed.
    Voting(VotingError),
    /// Chain transport or protocol handling failed.
    Chain(ChainError),
    /// CheckTx accepted the transaction, but its hash could not be journaled.
    ///
    /// The transaction is in the mempool and may commit, and this hash is the
    /// only way anything can ever locate it: the SDK does not predict chain
    /// hashes and cannot find a transaction from its commitment. Returning the
    /// persistence error alone would discard it, leaving a hashless reservation
    /// that no reconciliation can resolve.
    ///
    /// The host SHOULD retain `tx_hash` and record it once storage recovers, for
    /// example with `mark_delegation_submitted` or `mark_vote_submitted`, so a
    /// later reconciliation can confirm it.
    AcceptedButUnjournaled {
        tx_hash: String,
        source: VotingError,
    },
}

impl std::fmt::Display for ChainLifecycleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Voting(error) => write!(f, "{error}"),
            Self::Chain(error) => write!(f, "{error}"),
            Self::AcceptedButUnjournaled { tx_hash, source } => write!(
                f,
                "vote chain accepted transaction {tx_hash} but it could not be journaled \
                 ({source}); record this hash once storage recovers"
            ),
        }
    }
}

impl std::error::Error for ChainLifecycleError {}

impl From<VotingError> for ChainLifecycleError {
    fn from(value: VotingError) -> Self {
        Self::Voting(value)
    }
}

impl From<ChainError> for ChainLifecycleError {
    fn from(value: ChainError) -> Self {
        Self::Chain(value)
    }
}

/// High-level durable submission and reconciliation facade.
pub struct ChainSubmissionLifecycle<'a> {
    db: &'a VotingDb,
    client: &'a ChainClient,
}

/// Whether an outcome means this call must not broadcast again.
///
/// A settled result speaks for itself, and a known candidate that is pending or
/// unresolved may still commit. Used by both the preflight and the between-retry
/// reconciliation so the two cannot drift apart.
fn outcome_blocks_dispatch(outcome: &ChainLifecycleOutcome) -> bool {
    match outcome {
        ChainLifecycleOutcome::Confirmed { .. }
        | ChainLifecycleOutcome::AlreadyConfirmed { .. }
        | ChainLifecycleOutcome::Rejected { .. }
        | ChainLifecycleOutcome::AlreadySpentUnresolved { .. }
        | ChainLifecycleOutcome::Cancelled => true,
        ChainLifecycleOutcome::Pending { known_tx_hashes }
        | ChainLifecycleOutcome::OutcomeUnknown {
            known_tx_hashes, ..
        } => !known_tx_hashes.is_empty(),
        ChainLifecycleOutcome::Accepted { .. } => true,
    }
}

#[cfg(test)]
mod tests;
