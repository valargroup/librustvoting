//! Public bounded lifecycle entry points.

use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use crate::{storage::VotingDb, HyperTransport, Network};

use super::{
    coordinator::{
        ChainSubmissionCoordinator, CoordinatorPolicy, SubmissionControl,
        SystemChainSubmissionClock,
    },
    protocol::ChainProtocolClient,
    store::{SqliteChainSubmissionStore, StoreAdvancementRequest},
    ChainSubmissionFailure, ChainSubmissionFailureKind, ChainSubmissionIdentity,
    ChainSubmissionResult, ChainSubmissionTarget, ChainTransport,
};

/// Chain identity and finite policy for one submission client.
///
/// Construction validates the complete configuration before performing any
/// network request or changing durable submission state.
#[derive(Clone, Debug)]
pub struct ChainSubmissionClientConfig {
    /// Network included in every durable submission identity.
    ///
    /// Mainnet also requires every configured endpoint to use HTTPS.
    pub network: Network,
    /// Vote-chain deployment identifier validated for configuration compatibility.
    ///
    /// This selects the configured deployment but does not bind a submission
    /// identity or generation. The value must contain 1 to 128 printable ASCII
    /// bytes without whitespace.
    pub vote_chain_id: String,
    /// Ordered, distinct vote-chain base URLs.
    ///
    /// One to eight HTTP(S) URLs are required. URLs must not contain
    /// credentials, a query, or a fragment, and duplicates are rejected after
    /// canonicalization. Fresh POST failover and status lookup follow this
    /// order. Exact tree recovery reads from the first endpoint; later
    /// recovery POSTs rotate by durable reservation ordinal.
    pub endpoints: Vec<String>,
    /// Maximum time to track a usable candidate hash before entering recovery.
    ///
    /// This must be a nonzero whole number of seconds. The window starts when
    /// the durable row first enters `Tracking` and is not reset by polling,
    /// diagnostics, restarts, or later advancement calls.
    pub tracking_window: Duration,
    /// Maximum POST attempts made by one advancement call.
    ///
    /// This must be between one and eight and must not exceed the number of
    /// distinct configured endpoints. Historical durable attempt count does
    /// not reduce a later call's independent bounded budget.
    pub maximum_post_attempts: usize,
    /// Delays between consecutive POST attempts in one advancement call.
    ///
    /// This must contain exactly `maximum_post_attempts - 1` entries. Every
    /// delay must be nonzero and no greater than ten minutes.
    pub retry_backoffs: Vec<Duration>,
}

/// Host-owned cancellation and session-epoch authority for bounded calls.
///
/// Clones share both values. Cancellation and epoch changes are observed at
/// lifecycle boundaries but do not roll back a reservation, dispatch
/// classification, or confirmation that is already durable.
#[derive(Clone, Debug)]
pub struct ChainSubmissionControl {
    cancelled: Arc<AtomicBool>,
    operation_epoch: Arc<AtomicU64>,
}

/// Selects whether a bounded advancement may use exact commitment-tree
/// recovery after candidate-first reconciliation is inconclusive.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ChainRecoveryMode {
    /// Reconcile only through a known transaction hash.
    #[default]
    StatusOnly,
    /// Reconcile through a known hash first, then scan one fixed tree snapshot.
    ExactTree,
}

impl ChainSubmissionControl {
    /// Creates an uncancelled control at the supplied host operation epoch.
    pub fn new(operation_epoch: u64) -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            operation_epoch: Arc::new(AtomicU64::new(operation_epoch)),
        }
    }

    /// Permanently cancels this control and every clone of it.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// Replaces the shared epoch, invalidating calls that captured another one.
    ///
    /// A later call captures the new value. Changing the epoch does not undo
    /// durable effects produced before the mismatch is observed.
    pub fn set_operation_epoch(&self, operation_epoch: u64) {
        self.operation_epoch
            .store(operation_epoch, Ordering::Release);
    }

    /// Returns whether this control or one of its clones was cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Returns the current shared host operation epoch.
    pub fn operation_epoch(&self) -> u64 {
        self.operation_epoch.load(Ordering::Acquire)
    }
}

impl SubmissionControl for ChainSubmissionControl {
    fn is_cancelled(&self) -> bool {
        self.is_cancelled()
    }

    fn operation_epoch(&self) -> u64 {
        self.operation_epoch()
    }
}

/// Inputs that identify and authorize one prepared delegation generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdvanceDelegation {
    /// Canonical 32-byte round identifier used by the prepared bundle.
    pub vote_round_id: [u8; 32],
    /// Durable bundle containing the prepared delegation inputs.
    pub bundle_index: u32,
    /// SpendAuth signature verified against the locked durable setup.
    ///
    /// The SDK loads the authoritative PCZT sighash and randomized verification
    /// key from its database. Callers must not reconstruct that signing context.
    pub spend_auth_signature: [u8; 64],
}

impl AdvanceDelegation {
    /// Builds a delegation advancement request from external signature bytes.
    ///
    /// # Errors
    ///
    /// Returns [`ChainSubmissionFailureKind::InvalidInput`] unless
    /// `spend_auth_signature` is exactly 64 bytes.
    pub fn from_signature_bytes(
        vote_round_id: [u8; 32],
        bundle_index: u32,
        spend_auth_signature: &[u8],
    ) -> Result<Self, ChainSubmissionFailure> {
        let spend_auth_signature = spend_auth_signature.try_into().map_err(|_| {
            ChainSubmissionFailure::without_state(
                ChainSubmissionFailureKind::InvalidInput,
                format!(
                    "delegation SpendAuth signature must be 64 bytes, got {}",
                    spend_auth_signature.len()
                ),
            )
        })?;
        Ok(Self {
            vote_round_id,
            bundle_index,
            spend_auth_signature,
        })
    }
}

/// Identifies an already-broadcast delegation imported from a capability
/// package.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdvanceImportedDelegation {
    /// Canonical 32-byte round identifier bound by the imported package.
    pub vote_round_id: [u8; 32],
    /// Imported bundle whose stored package hash should be polled.
    pub bundle_index: u32,
}

/// Inputs that identify one prepared singleton vote generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdvanceVote {
    /// Canonical 32-byte round identifier used by the prepared bundle.
    pub vote_round_id: [u8; 32],
    /// Durable bundle containing the prepared vote inputs.
    pub bundle_index: u32,
    /// Proposal whose durable singleton vote is advanced.
    pub proposal_id: u32,
}

/// Inputs that identify one prepared atomic vote-batch generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdvanceVoteBatch {
    /// Canonical 32-byte round identifier used by every prepared batch member.
    pub vote_round_id: [u8; 32],
    /// Durable bundle containing the prepared batch inputs.
    pub bundle_index: u32,
    /// Digest binding the complete ordered batch roster.
    pub ordered_batch_digest: [u8; 32],
    /// Proposal identifiers in signed action order.
    pub ordered_proposal_ids: Vec<u32>,
}

/// SDK-owned durable submission lifecycle using one HTTP mechanism.
///
/// Each advancement serializes work for its identity, reconstructs the
/// semantic generation from `db`, and owns reservation, submission,
/// reconciliation, and atomic confirmation. Callers should schedule another
/// bounded pass from [`ChainSubmissionResult`] rather than mutating submission
/// state directly.
pub struct ChainSubmissionClient<T> {
    db: Arc<VotingDb>,
    network: Network,
    coordinator:
        ChainSubmissionCoordinator<T, SqliteChainSubmissionStore, SystemChainSubmissionClock>,
}

impl ChainSubmissionClient<HyperTransport> {
    /// Creates a client backed by the SDK's default HTTP transport.
    ///
    /// Construction validates `config` but performs no network request and
    /// does not change durable submission state.
    ///
    /// # Errors
    ///
    /// Returns [`ChainSubmissionFailureKind::InvalidInput`] if the chain
    /// identifier, endpoints, tracking window, attempt count, or backoff
    /// relationship is invalid.
    pub fn new(
        db: Arc<VotingDb>,
        config: ChainSubmissionClientConfig,
    ) -> Result<Self, ChainSubmissionFailure> {
        Self::with_transport(db, HyperTransport::new(), config)
    }
}

impl<T: ChainTransport> ChainSubmissionClient<T> {
    /// Creates a client backed by a caller-supplied transport.
    ///
    /// The injected transport changes only HTTP execution; validation,
    /// persistence, locking, retry bounds, and result postconditions are the
    /// same as for [`ChainSubmissionClient::new`]. Construction performs no
    /// network request and does not change durable submission state.
    ///
    /// # Errors
    ///
    /// Returns [`ChainSubmissionFailureKind::InvalidInput`] if the chain
    /// identifier, endpoints, tracking window, attempt count, or backoff
    /// relationship is invalid.
    pub fn with_transport(
        db: Arc<VotingDb>,
        transport: T,
        config: ChainSubmissionClientConfig,
    ) -> Result<Self, ChainSubmissionFailure> {
        crate::types::validate_vote_chain_id(&config.vote_chain_id).map_err(|error| {
            ChainSubmissionFailure::without_state(
                ChainSubmissionFailureKind::InvalidInput,
                error.to_string(),
            )
        })?;
        let policy = CoordinatorPolicy::new(
            config.tracking_window,
            config.maximum_post_attempts,
            config.retry_backoffs,
        )?;
        let protocol = ChainProtocolClient::new(transport, config.network, &config.endpoints)
            .map_err(|diagnostic| {
                ChainSubmissionFailure::without_state(
                    ChainSubmissionFailureKind::InvalidInput,
                    diagnostic.message(),
                )
            })?;
        let store = Arc::new(SqliteChainSubmissionStore::new(Arc::clone(&db)));
        let coordinator =
            ChainSubmissionCoordinator::new(protocol, store, SystemChainSubmissionClock, policy)?;
        Ok(Self {
            db,
            network: config.network,
            coordinator,
        })
    }

    /// Advances one prepared delegation through one bounded status-only pass.
    ///
    /// The pass may durably reserve before POST, submit, poll a candidate, and
    /// atomically persist confirmation plus the applicable bundle, recovery,
    /// domain, and helper projections. It does not scan the commitment tree.
    /// A non-cancelled result represents the authoritative durable outcome
    /// reported by [`ChainSubmissionResult::durable_state`].
    ///
    /// # Errors
    ///
    /// Returns a failure for invalid identity or prepared state, invariant or
    /// storage failure, transport failure, or invalid protocol data. Once
    /// dispatch may have occurred, cancellation or failure does not erase the
    /// strongest state reported by [`ChainSubmissionFailure::strongest_state`].
    pub async fn advance_delegation(
        &self,
        request: AdvanceDelegation,
        control: &ChainSubmissionControl,
    ) -> Result<ChainSubmissionResult, ChainSubmissionFailure> {
        let identity = self.identity(
            request.vote_round_id,
            request.bundle_index,
            ChainSubmissionTarget::Delegation,
        )?;
        self.coordinator
            .advance(
                StoreAdvancementRequest::delegation(identity, request.spend_auth_signature),
                control,
            )
            .await
    }

    /// Adopts and advances one already-broadcast capability delegation.
    ///
    /// The first active pass validates the structurally imported bundle and
    /// atomically adopts its stored package hash as a poll-only lifecycle
    /// generation. The voter never supplies a signer, transaction hash, request
    /// body, or chain events, and this path never dispatches or retries a POST.
    /// Re-invoke while the result is pending; confirmation atomically records
    /// the imported bundle's VAN position.
    ///
    /// # Errors
    ///
    /// Returns a failure when the identity does not name an imported capability
    /// bundle, its stored hash or generation conflicts, status transport or
    /// protocol validation fails, or durable adoption/confirmation cannot
    /// commit. Any adopted state remains available through
    /// [`ChainSubmissionFailure::strongest_state`].
    pub async fn advance_imported_delegation(
        &self,
        request: AdvanceImportedDelegation,
        control: &ChainSubmissionControl,
    ) -> Result<ChainSubmissionResult, ChainSubmissionFailure> {
        let identity = self.identity(
            request.vote_round_id,
            request.bundle_index,
            ChainSubmissionTarget::Delegation,
        )?;
        self.coordinator
            .advance(
                StoreAdvancementRequest::imported_delegation(identity),
                control,
            )
            .await
    }

    /// Advances one prepared delegation through one bounded pass.
    ///
    /// This has the same durable side effects and result postconditions as
    /// [`Self::advance_delegation`]. [`ChainRecoveryMode::ExactTree`] may,
    /// after candidate-first reconciliation is inconclusive, scan one fixed
    /// complete tree snapshot and atomically confirm an exact unique layout or
    /// authorize one same-generation retry within this call's attempt budget.
    ///
    /// # Errors
    ///
    /// Returns a failure for invalid identity or prepared state, invariant or
    /// storage failure, transport failure, or invalid protocol or recovery
    /// data. Durable or possibly-dispatched state remains available through
    /// [`ChainSubmissionFailure::strongest_state`].
    pub async fn advance_delegation_with_recovery(
        &self,
        request: AdvanceDelegation,
        recovery: ChainRecoveryMode,
        control: &ChainSubmissionControl,
    ) -> Result<ChainSubmissionResult, ChainSubmissionFailure> {
        let identity = self.identity(
            request.vote_round_id,
            request.bundle_index,
            ChainSubmissionTarget::Delegation,
        )?;
        self.coordinator
            .advance_with_recovery(
                StoreAdvancementRequest::delegation(identity, request.spend_auth_signature),
                recovery,
                control,
            )
            .await
    }

    /// Advances one prepared singleton vote through one bounded status-only pass.
    ///
    /// The pass may durably reserve before POST, submit, poll a candidate, and
    /// atomically persist confirmation plus the applicable bundle, vote,
    /// recovery, domain, and helper projections. It does not scan the
    /// commitment tree. A non-cancelled result represents the authoritative
    /// durable outcome reported by [`ChainSubmissionResult::durable_state`].
    ///
    /// # Errors
    ///
    /// Returns a failure for invalid identity or prepared state, invariant or
    /// storage failure, transport failure, or invalid protocol data. Once
    /// dispatch may have occurred, cancellation or failure does not erase the
    /// strongest state reported by [`ChainSubmissionFailure::strongest_state`].
    pub async fn advance_vote(
        &self,
        request: AdvanceVote,
        control: &ChainSubmissionControl,
    ) -> Result<ChainSubmissionResult, ChainSubmissionFailure> {
        let identity = self.identity(
            request.vote_round_id,
            request.bundle_index,
            ChainSubmissionTarget::Vote {
                proposal_id: request.proposal_id,
            },
        )?;
        self.coordinator
            .advance(StoreAdvancementRequest::vote(identity), control)
            .await
    }

    /// Advances one prepared singleton vote through one bounded pass.
    ///
    /// This has the same durable side effects and result postconditions as
    /// [`Self::advance_vote`]. [`ChainRecoveryMode::ExactTree`] may, after
    /// candidate-first reconciliation is inconclusive, scan one fixed complete
    /// tree snapshot and atomically confirm an exact unique layout or authorize
    /// one same-generation retry within this call's attempt budget.
    ///
    /// # Errors
    ///
    /// Returns a failure for invalid identity or prepared state, invariant or
    /// storage failure, transport failure, or invalid protocol or recovery
    /// data. Durable or possibly-dispatched state remains available through
    /// [`ChainSubmissionFailure::strongest_state`].
    pub async fn advance_vote_with_recovery(
        &self,
        request: AdvanceVote,
        recovery: ChainRecoveryMode,
        control: &ChainSubmissionControl,
    ) -> Result<ChainSubmissionResult, ChainSubmissionFailure> {
        let identity = self.identity(
            request.vote_round_id,
            request.bundle_index,
            ChainSubmissionTarget::Vote {
                proposal_id: request.proposal_id,
            },
        )?;
        self.coordinator
            .advance_with_recovery(StoreAdvancementRequest::vote(identity), recovery, control)
            .await
    }

    /// Advances one prepared atomic vote batch through one bounded status-only pass.
    ///
    /// The pass validates a non-empty, protocol-bounded, duplicate-free proposal
    /// roster, then rederives its locked durable roster and ordered digest. It
    /// may durably reserve before POST, submit the complete batch, poll a
    /// candidate, and atomically persist confirmation for every batch member.
    /// It does not scan the commitment tree. A non-cancelled result represents
    /// the authoritative durable outcome reported by
    /// [`ChainSubmissionResult::durable_state`].
    ///
    /// # Errors
    ///
    /// Returns a failure for invalid identity, roster, digest, or prepared
    /// state; invariant or storage failure; transport failure; or invalid
    /// protocol data. Once dispatch may have occurred, cancellation or failure
    /// does not erase the strongest state reported by
    /// [`ChainSubmissionFailure::strongest_state`].
    pub async fn advance_vote_batch(
        &self,
        request: AdvanceVoteBatch,
        control: &ChainSubmissionControl,
    ) -> Result<ChainSubmissionResult, ChainSubmissionFailure> {
        self.advance_vote_batch_with_recovery(request, ChainRecoveryMode::StatusOnly, control)
            .await
    }

    /// Advances one prepared atomic vote batch through one bounded pass.
    ///
    /// This has the same validation, durable and network side effects, and
    /// result postconditions as [`Self::advance_vote_batch`].
    /// [`ChainRecoveryMode::StatusOnly`] reconciles only through a known
    /// transaction hash. [`ChainRecoveryMode::ExactTree`] may, after
    /// candidate-first reconciliation is inconclusive, scan one fixed complete
    /// tree snapshot and atomically confirm only the unique exact ordered batch
    /// layout or authorize one same-generation retry within this call's attempt
    /// budget.
    ///
    /// # Errors
    ///
    /// Returns a failure for invalid identity, roster, digest, or prepared
    /// state; invariant or storage failure; transport failure; or invalid
    /// protocol or recovery data. Durable or possibly-dispatched state remains
    /// available through [`ChainSubmissionFailure::strongest_state`].
    pub async fn advance_vote_batch_with_recovery(
        &self,
        request: AdvanceVoteBatch,
        recovery: ChainRecoveryMode,
        control: &ChainSubmissionControl,
    ) -> Result<ChainSubmissionResult, ChainSubmissionFailure> {
        let identity = self.identity(
            request.vote_round_id,
            request.bundle_index,
            ChainSubmissionTarget::VoteBatch {
                ordered_batch_digest: request.ordered_batch_digest,
            },
        )?;
        let advancement =
            StoreAdvancementRequest::vote_batch(identity, request.ordered_proposal_ids)?;
        self.coordinator
            .advance_with_recovery(advancement, recovery, control)
            .await
    }

    fn identity(
        &self,
        vote_round_id: [u8; 32],
        bundle_index: u32,
        target: ChainSubmissionTarget,
    ) -> Result<ChainSubmissionIdentity, ChainSubmissionFailure> {
        ChainSubmissionIdentity::new(
            self.db.wallet_id(),
            self.network,
            vote_round_id,
            bundle_index,
            target,
        )
        .map_err(|error| {
            ChainSubmissionFailure::without_state(
                ChainSubmissionFailureKind::InvalidInput,
                error.to_string(),
            )
        })
    }
}
