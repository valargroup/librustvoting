//! Public bounded lifecycle entry points.

use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use crate::{delegate::DelegationSigner, storage::VotingDb, HyperTransport, Network};

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

/// Required finite policy and chain context for a submission client.
#[derive(Clone, Debug)]
pub struct ChainSubmissionClientConfig {
    pub network: Network,
    pub vote_chain_id: String,
    pub endpoints: Vec<String>,
    pub tracking_window: Duration,
    pub maximum_post_attempts: usize,
    pub retry_backoffs: Vec<Duration>,
}

/// Host-owned cancellation and epoch token captured by one bounded call.
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
    pub fn new(operation_epoch: u64) -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            operation_epoch: Arc::new(AtomicU64::new(operation_epoch)),
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// Invalidates already-captured work after a host account/session switch.
    pub fn set_operation_epoch(&self, operation_epoch: u64) {
        self.operation_epoch
            .store(operation_epoch, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

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

/// Inputs that identify and sign one prepared delegation generation.
pub struct AdvanceDelegation {
    pub vote_round_id: [u8; 32],
    pub bundle_index: u32,
    pub signer: DelegationSigner,
}

/// Inputs that identify one prepared singleton vote generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdvanceVote {
    pub vote_round_id: [u8; 32],
    pub bundle_index: u32,
    pub proposal_id: u32,
}

/// SDK-owned submission lifecycle using one injected HTTP mechanism.
pub struct ChainSubmissionClient<T> {
    db: Arc<VotingDb>,
    network: Network,
    vote_chain_id: String,
    coordinator:
        ChainSubmissionCoordinator<T, SqliteChainSubmissionStore, SystemChainSubmissionClock>,
}

impl ChainSubmissionClient<HyperTransport> {
    pub fn new(
        db: Arc<VotingDb>,
        config: ChainSubmissionClientConfig,
    ) -> Result<Self, ChainSubmissionFailure> {
        Self::with_transport(db, HyperTransport::new(), config)
    }
}

impl<T: ChainTransport> ChainSubmissionClient<T> {
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
            vote_chain_id: config.vote_chain_id,
            coordinator,
        })
    }

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
                StoreAdvancementRequest::delegation(identity, request.signer),
                control,
            )
            .await
    }

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
                StoreAdvancementRequest::delegation(identity, request.signer),
                recovery,
                control,
            )
            .await
    }

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

    fn identity(
        &self,
        vote_round_id: [u8; 32],
        bundle_index: u32,
        target: ChainSubmissionTarget,
    ) -> Result<ChainSubmissionIdentity, ChainSubmissionFailure> {
        ChainSubmissionIdentity::new(
            self.db.wallet_id(),
            self.network,
            &self.vote_chain_id,
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
