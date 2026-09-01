//! Durable vote-chain submission and confirmation lifecycle.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::{Arc, Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use rusqlite::{named_params, TransactionBehavior};
use sha2::{Digest, Sha256};

use crate::{
    chain::{
        is_spent_nullifier_log, ChainBroadcastOutcome, ChainClient, ChainError,
        ChainTxConfirmation, ChainTxStatus,
    },
    confirmation::{
        confirm_delegation_submission, confirm_vote_batch_submission, confirm_vote_submission,
    },
    delegate::{self, DelegationSigner, SignedDelegationBundle},
    storage::{queries, VotingDb},
    types::VotingError,
    vote,
    wire::{
        DelegationConfirmation, DelegationSubmissionWire, VoteBatchConfirmation,
        VoteCommitmentBatchWire, VoteCommitmentWire, VoteConfirmation,
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChainSubmissionKind {
    Delegation,
    Vote,
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
/// The fields are private and the constructors below are the only way to build
/// one, so a vote identity always carries a proposal and a batch identity always
/// carries a digest. The storage `CHECK` on `chain_submission_attempts` enforces
/// the same pairing at rest; keeping the type unable to express the invalid
/// combinations means a public caller cannot drive the lifecycle into a state it
/// would have to panic on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainSubmissionIdentity {
    round_id: String,
    kind: ChainSubmissionKind,
    bundle_index: u32,
    proposal_id: Option<u32>,
    batch_digest: Option<[u8; 32]>,
}

impl ChainSubmissionIdentity {
    pub fn round_id(&self) -> &str {
        &self.round_id
    }

    pub fn kind(&self) -> ChainSubmissionKind {
        self.kind
    }

    pub fn bundle_index(&self) -> u32 {
        self.bundle_index
    }

    /// The proposal this identity votes on, present exactly for singleton votes.
    pub fn proposal_id(&self) -> Option<u32> {
        self.proposal_id
    }

    /// The batch sighash digest, present exactly for atomic vote batches.
    pub fn batch_digest(&self) -> Option<[u8; 32]> {
        self.batch_digest
    }

    fn require_proposal_id(&self) -> Result<u32, VotingError> {
        self.proposal_id.ok_or_else(|| VotingError::Internal {
            message: "singleton vote submission identity has no proposal".to_string(),
        })
    }

    fn require_batch_digest(&self) -> Result<[u8; 32], VotingError> {
        self.batch_digest.ok_or_else(|| VotingError::Internal {
            message: "atomic vote batch submission identity has no batch digest".to_string(),
        })
    }

    pub fn delegation(round_id: impl Into<String>, bundle_index: u32) -> Self {
        Self {
            round_id: round_id.into(),
            kind: ChainSubmissionKind::Delegation,
            bundle_index,
            proposal_id: None,
            batch_digest: None,
        }
    }

    pub fn vote(round_id: impl Into<String>, bundle_index: u32, proposal_id: u32) -> Self {
        Self {
            round_id: round_id.into(),
            kind: ChainSubmissionKind::Vote,
            bundle_index,
            proposal_id: Some(proposal_id),
            batch_digest: None,
        }
    }

    pub fn vote_batch(
        round_id: impl Into<String>,
        bundle_index: u32,
        batch_digest: [u8; 32],
    ) -> Self {
        Self {
            round_id: round_id.into(),
            kind: ChainSubmissionKind::VoteBatch,
            bundle_index,
            proposal_id: None,
            batch_digest: Some(batch_digest),
        }
    }

    fn proposal_key(&self) -> i64 {
        self.proposal_id.map(i64::from).unwrap_or(-1)
    }

    fn batch_key(&self) -> &[u8] {
        self.batch_digest
            .as_ref()
            .map(<[u8; 32]>::as_slice)
            .unwrap_or(&[])
    }

    fn lock_key(&self, wallet_id: &str) -> String {
        format!(
            "{wallet_id}/{}/{}/{}/{}",
            self.round_id,
            self.kind.as_str(),
            self.bundle_index,
            self.proposal_key()
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChainConfirmation {
    Delegation(DelegationConfirmation),
    Vote(VoteConfirmation),
    VoteBatch(VoteBatchConfirmation),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChainLifecycleOutcome {
    Accepted {
        tx_hash: String,
    },
    Confirmed {
        tx_hash: String,
        confirmation: ChainConfirmation,
        reconciled: bool,
    },
    Pending {
        known_tx_hashes: Vec<String>,
    },
    Rejected {
        code: u32,
        log: String,
    },
    AlreadySpentUnresolved {
        known_tx_hashes: Vec<String>,
        log: String,
    },
    OutcomeUnknown {
        known_tx_hashes: Vec<String>,
        message: String,
    },
    Cancelled,
}

#[derive(Debug)]
pub enum ChainLifecycleError {
    Voting(VotingError),
    Chain(ChainError),
}

impl std::fmt::Display for ChainLifecycleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Voting(error) => write!(f, "{error}"),
            Self::Chain(error) => write!(f, "{error}"),
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

pub struct ChainSubmissionLifecycle<'a> {
    db: &'a VotingDb,
    client: &'a ChainClient,
}

impl<'a> ChainSubmissionLifecycle<'a> {
    pub fn new(db: &'a VotingDb, client: &'a ChainClient) -> Self {
        Self { db, client }
    }

    pub async fn submit_delegation(
        &self,
        bundle: &SignedDelegationBundle,
        cancel: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<ChainLifecycleOutcome, ChainLifecycleError> {
        let round_id = bundle.submission.vote_round_id.clone();
        let identity = ChainSubmissionIdentity::delegation(&round_id, bundle.bundle_index);
        let wallet_id = self.db.wallet_id();
        let lock = operation_lock(&identity.lock_key(&wallet_id))?;
        let _guard = lock.lock().await;
        let recovered = {
            let conn = self.db.conn();
            delegate::submission_with_conn(
                &conn,
                &wallet_id,
                &round_id,
                bundle.bundle_index,
                DelegationSigner::signature(
                    bundle.submission.spend_auth_sig,
                    bundle.submission.sighash,
                ),
            )?
        };
        if recovered != bundle.submission {
            return Err(VotingError::InvalidInput {
                message: "signed delegation does not match durable bundle generation".to_string(),
            }
            .into());
        }
        let wire = DelegationSubmissionWire::try_from(&bundle.submission)?;
        let body = wire.to_json()?.into_bytes();
        let rebuild = delegation_rebuild(
            wallet_id.clone(),
            round_id.clone(),
            bundle.bundle_index,
            bundle.submission.spend_auth_sig,
            bundle.submission.sighash,
        );
        self.submit_body_locked(&wallet_id, identity, body, &rebuild, cancel)
            .await
    }

    /// Submits an FFI-safe delegation wire value after reconstructing and
    /// comparing it with the exact persisted delegation generation.
    pub async fn submit_delegation_wire(
        &self,
        round_id: &str,
        bundle_index: u32,
        wire: &DelegationSubmissionWire,
        cancel: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<ChainLifecycleOutcome, ChainLifecycleError> {
        let identity = ChainSubmissionIdentity::delegation(round_id, bundle_index);
        let wallet_id = self.db.wallet_id();
        let lock = operation_lock(&identity.lock_key(&wallet_id))?;
        let _guard = lock.lock().await;
        let signature = decode_canonical_array::<64>(&wire.spend_auth_sig, "spend_auth_sig")?;
        let sighash = {
            let conn = self.db.conn();
            let bytes = queries::load_pczt_sighash(&conn, round_id, &wallet_id, bundle_index)?;
            bytes
                .try_into()
                .map_err(|bytes: Vec<u8>| VotingError::Internal {
                    message: format!(
                        "persisted delegation sighash must be 32 bytes, got {}",
                        bytes.len()
                    ),
                })?
        };
        let recovered = delegate::submission(
            self.db,
            round_id,
            bundle_index,
            DelegationSigner::signature(signature, sighash),
        )?;
        let expected = DelegationSubmissionWire::try_from(&recovered)?;
        if expected != *wire {
            return Err(VotingError::InvalidInput {
                message: "delegation wire does not match durable bundle generation".to_string(),
            }
            .into());
        }
        let body = expected.to_json()?.into_bytes();
        let rebuild = delegation_rebuild(
            wallet_id.clone(),
            round_id.to_string(),
            bundle_index,
            signature,
            sighash,
        );
        self.submit_body_locked(&wallet_id, identity, body, &rebuild, cancel)
            .await
    }

    pub async fn submit_vote(
        &self,
        round_id: &str,
        bundle_index: u32,
        proposal_id: u32,
        cancel: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<ChainLifecycleOutcome, ChainLifecycleError> {
        let identity = ChainSubmissionIdentity::vote(round_id, bundle_index, proposal_id);
        let wallet_id = self.db.wallet_id();
        let lock = operation_lock(&identity.lock_key(&wallet_id))?;
        let _guard = lock.lock().await;
        // `singleton_vote_body` loads and validates the exact recovery
        // generation and refuses a member of a persisted atomic batch, so the
        // outer body and the reservation rebuild share one implementation.
        let body = {
            let conn = self.db.conn();
            singleton_vote_body(&conn, &wallet_id, round_id, bundle_index, proposal_id)?
        };
        let rebuild_wallet_id = wallet_id.clone();
        let owned_round_id = round_id.to_string();
        let rebuild = move |conn: &rusqlite::Connection| -> Result<Vec<u8>, VotingError> {
            singleton_vote_body(
                conn,
                &rebuild_wallet_id,
                &owned_round_id,
                bundle_index,
                proposal_id,
            )
        };
        self.submit_body_locked(&wallet_id, identity, body, &rebuild, cancel)
            .await
    }

    pub async fn submit_vote_batch(
        &self,
        round_id: &str,
        bundle_index: u32,
        proposal_id: u32,
        cancel: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<ChainLifecycleOutcome, ChainLifecycleError> {
        let lock_identity = ChainSubmissionIdentity::vote_batch(round_id, bundle_index, [0; 32]);
        let wallet_id = self.db.wallet_id();
        let lock = operation_lock(&lock_identity.lock_key(&wallet_id))?;
        let _guard = lock.lock().await;
        let batch = {
            let conn = self.db.conn();
            vote::recover_atomic_vote_batch_with_conn(
                &conn,
                &wallet_id,
                round_id,
                bundle_index,
                proposal_id,
            )?
        };
        let body = batch_body_from_json(&batch.batch_json)?;
        let identity =
            ChainSubmissionIdentity::vote_batch(round_id, bundle_index, batch.batch_digest);
        let rebuild_wallet_id = wallet_id.clone();
        let owned_round_id = round_id.to_string();
        let rebuild = move |conn: &rusqlite::Connection| -> Result<Vec<u8>, VotingError> {
            let batch = vote::recover_atomic_vote_batch_with_conn(
                conn,
                &rebuild_wallet_id,
                &owned_round_id,
                bundle_index,
                proposal_id,
            )?;
            batch_body_from_json(&batch.batch_json)
        };
        self.submit_body_locked(&wallet_id, identity, body, &rebuild, cancel)
            .await
    }

    pub async fn reconcile(
        &self,
        identity: &ChainSubmissionIdentity,
        cancel: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<ChainLifecycleOutcome, ChainLifecycleError> {
        let wallet_id = self.db.wallet_id();
        let lock = operation_lock(&identity.lock_key(&wallet_id))?;
        let _guard = lock.lock().await;
        self.reconcile_locked(&wallet_id, identity, cancel).await
    }

    async fn submit_body_locked(
        &self,
        wallet_id: &str,
        identity: ChainSubmissionIdentity,
        body: Vec<u8>,
        rebuild: PayloadRebuild<'_>,
        cancel: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<ChainLifecycleOutcome, ChainLifecycleError> {
        let existing = self.reconcile_locked(wallet_id, &identity, cancel).await?;
        match &existing {
            ChainLifecycleOutcome::Confirmed { .. }
            | ChainLifecycleOutcome::Rejected { .. }
            | ChainLifecycleOutcome::Cancelled => return Ok(existing),
            // A known candidate that is pending, or whose status could not be
            // read, must not be rebroadcast: it may still commit.
            ChainLifecycleOutcome::Pending { known_tx_hashes }
            | ChainLifecycleOutcome::OutcomeUnknown {
                known_tx_hashes, ..
            } if !known_tx_hashes.is_empty() => return Ok(existing),
            _ => {}
        }

        let digest: [u8; 32] = Sha256::digest(&body).into();
        let attempts = self.client.retry_delays().len() + 1;
        let mut last_unknown = None;
        for attempt_index in 0..attempts {
            if cancel() {
                return Ok(ChainLifecycleOutcome::Cancelled);
            }
            if attempt_index > 0 {
                let reconciled = self.reconcile_locked(wallet_id, &identity, cancel).await?;
                if matches!(reconciled, ChainLifecycleOutcome::Confirmed { .. }) {
                    return Ok(reconciled);
                }
            }

            // Each attempt re-derives the payload inside its own reservation
            // transaction. That repeats the delegation signature verification
            // and recovery reads up to `attempts` times per call, which is the
            // intended cost of proving the generation is still current.
            let attempt_id = reserve_attempt(self.db, wallet_id, &identity, &digest, rebuild)?;
            if cancel() {
                delete_attempt(self.db, wallet_id, attempt_id)?;
                return Ok(ChainLifecycleOutcome::Cancelled);
            }

            let result = self
                .client
                .post_once(attempt_index, identity.kind.endpoint(), body.clone())
                .await;
            match result {
                Ok(ChainBroadcastOutcome::Accepted(result)) => {
                    mark_attempt(
                        self.db,
                        wallet_id,
                        attempt_id,
                        "accepted",
                        Some(&result.tx_hash),
                    )?;
                    return Ok(ChainLifecycleOutcome::Accepted {
                        tx_hash: result.tx_hash,
                    });
                }
                Ok(ChainBroadcastOutcome::Rejected(result)) => {
                    let tx_hash = (!result.tx_hash.is_empty()).then_some(result.tx_hash.as_str());
                    mark_attempt(self.db, wallet_id, attempt_id, "rejected", tx_hash)?;
                    if is_spent_nullifier_log(&result.log) {
                        let reconciled =
                            self.reconcile_locked(wallet_id, &identity, cancel).await?;
                        if matches!(reconciled, ChainLifecycleOutcome::Confirmed { .. }) {
                            return Ok(reconciled);
                        }
                        return Ok(ChainLifecycleOutcome::AlreadySpentUnresolved {
                            known_tx_hashes: known_hashes(self.db, wallet_id, &identity)?,
                            log: result.log,
                        });
                    }
                    return Ok(ChainLifecycleOutcome::Rejected {
                        code: result.code,
                        log: result.log,
                    });
                }
                Ok(ChainBroadcastOutcome::OutcomeUnknown { message }) => {
                    mark_attempt(self.db, wallet_id, attempt_id, "outcome_unknown", None)?;
                    last_unknown = Some(message);
                }
                Ok(ChainBroadcastOutcome::Cancelled) => {
                    mark_attempt(self.db, wallet_id, attempt_id, "outcome_unknown", None)?;
                    return Ok(ChainLifecycleOutcome::Cancelled);
                }
                Err(error) => {
                    let definitely_unsent = matches!(
                        &error,
                        ChainError::Transport(
                            crate::helper::transport::HelperTransportError::Transport(_)
                        )
                    );
                    if definitely_unsent {
                        delete_attempt(self.db, wallet_id, attempt_id)?;
                    } else if error.is_ambiguous() {
                        mark_attempt(self.db, wallet_id, attempt_id, "outcome_unknown", None)?;
                    } else {
                        mark_attempt(self.db, wallet_id, attempt_id, "rejected", None)?;
                    }
                    if !error.is_retryable() || attempt_index + 1 == attempts {
                        if error.is_ambiguous() {
                            return Ok(ChainLifecycleOutcome::OutcomeUnknown {
                                known_tx_hashes: known_hashes(self.db, wallet_id, &identity)?,
                                message: error.to_string(),
                            });
                        }
                        return Err(error.into());
                    }
                    last_unknown = Some(error.to_string());
                }
            }

            if attempt_index + 1 < attempts {
                if cancel() {
                    return Ok(ChainLifecycleOutcome::Cancelled);
                }
                tokio::time::sleep(self.client.retry_delays()[attempt_index]).await;
            }
        }

        Ok(ChainLifecycleOutcome::OutcomeUnknown {
            known_tx_hashes: known_hashes(self.db, wallet_id, &identity)?,
            message: last_unknown.unwrap_or_else(|| "submission outcome is unknown".to_string()),
        })
    }

    async fn reconcile_locked(
        &self,
        wallet_id: &str,
        identity: &ChainSubmissionIdentity,
        cancel: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<ChainLifecycleOutcome, ChainLifecycleError> {
        let hashes = known_hashes(self.db, wallet_id, identity)?;
        if hashes.is_empty() {
            return Ok(ChainLifecycleOutcome::Pending {
                known_tx_hashes: hashes,
            });
        }
        let mut successful = Vec::new();
        let mut any_pending = false;
        let mut unresolved = None;
        let mut committed_failures = Vec::new();
        for hash in &hashes {
            if cancel() {
                return Ok(ChainLifecycleOutcome::Cancelled);
            }
            match self.client.transaction_status(hash, cancel).await {
                Ok(ChainTxStatus::Pending) => any_pending = true,
                Ok(ChainTxStatus::Committed(confirmation)) if confirmation.code == 0 => {
                    successful.push((hash.clone(), confirmation));
                }
                Ok(ChainTxStatus::Committed(confirmation)) => {
                    committed_failures.push((hash.clone(), confirmation))
                }
                Err(ChainError::Cancelled) => return Ok(ChainLifecycleOutcome::Cancelled),
                // A lookup we could not complete or could not read is not
                // evidence that the transaction is absent. Keep it distinct from
                // a genuine 404 so a broken or incompatible endpoint is not
                // reported as "not yet committed".
                Err(error) if error.is_retryable() || error.is_ambiguous() => {
                    unresolved.get_or_insert(error.to_string());
                }
                Err(error) => return Err(error.into()),
            }
        }
        if successful.len() > 1 {
            return Err(VotingError::Internal {
                message: "multiple chain candidates committed successfully for one submission"
                    .to_string(),
            }
            .into());
        }
        if let Some((hash, response)) = successful.pop() {
            // The status request may have taken arbitrarily long. An account or
            // session invalidated while it was in flight must not have voting
            // state mutated underneath it, so re-check cancellation immediately
            // before the confirmation transaction rather than only before the
            // request.
            if cancel() {
                return Ok(ChainLifecycleOutcome::Cancelled);
            }
            let confirmation = apply_confirmation(self.db, wallet_id, identity, &hash, &response)?;
            return Ok(ChainLifecycleOutcome::Confirmed {
                tx_hash: hash,
                confirmation,
                reconciled: true,
            });
        }
        if let Some(message) = unresolved {
            return Ok(ChainLifecycleOutcome::OutcomeUnknown {
                known_tx_hashes: hashes,
                message,
            });
        }
        if any_pending {
            return Ok(ChainLifecycleOutcome::Pending {
                known_tx_hashes: hashes,
            });
        }
        if let Some((hash, failure)) = committed_failures.into_iter().next() {
            // The candidate's transaction definitively failed at commit, so its
            // journal row must stop being live evidence. Leaving it `accepted`
            // would make every later submission rediscover it and exit before
            // dispatch, and would keep ballot-intent changes and recovery
            // cleanup pinned to a generation that can never confirm.
            retire_attempts_for_hash(self.db, wallet_id, identity, &hash)?;
            return Ok(ChainLifecycleOutcome::Rejected {
                code: failure.code,
                log: failure.log,
            });
        }
        Ok(ChainLifecycleOutcome::Pending {
            known_tx_hashes: hashes,
        })
    }
}

fn apply_confirmation(
    db: &VotingDb,
    wallet_id: &str,
    identity: &ChainSubmissionIdentity,
    tx_hash: &str,
    response: &ChainTxConfirmation,
) -> Result<ChainConfirmation, VotingError> {
    // Confirmation writes go through the existing wallet-scoped `confirm_*`
    // entry points. Refuse rather than record this submission against a wallet
    // the host switched to while the status request was in flight.
    ensure_operation_wallet(db, wallet_id)?;
    match identity.kind {
        ChainSubmissionKind::Delegation => confirm_delegation_submission(
            db,
            &identity.round_id,
            identity.bundle_index,
            tx_hash,
            &response.events,
        )
        .map(ChainConfirmation::Delegation),
        ChainSubmissionKind::Vote => confirm_vote_submission(
            db,
            &identity.round_id,
            identity.bundle_index,
            identity.require_proposal_id()?,
            tx_hash,
            &response.events,
        )
        .map(ChainConfirmation::Vote),
        ChainSubmissionKind::VoteBatch => confirm_vote_batch_submission(
            db,
            &identity.round_id,
            identity.bundle_index,
            &identity.require_batch_digest()?,
            tx_hash,
            &response.events,
        )
        .map(ChainConfirmation::VoteBatch),
    }
}

/// Rebuilds one canonical submission payload from durable state.
///
/// The rebuild takes a connection rather than a [`VotingDb`] on purpose:
/// [`VotingDb::conn`] guards a single shared connection, so a closure that could
/// reach back through `VotingDb` would deadlock when called from inside the
/// reservation transaction.
/// Rebuild closure for a delegation payload.
///
/// The spend-auth signature is not durable for a software signer, so it is
/// captured from the live call. Everything else is re-read from storage, which
/// is exactly the material a concurrent writer could have replaced.
fn delegation_rebuild(
    wallet_id: String,
    round_id: String,
    bundle_index: u32,
    spend_auth_sig: [u8; 64],
    sighash: [u8; 32],
) -> impl Fn(&rusqlite::Connection) -> Result<Vec<u8>, VotingError> + Send + Sync {
    move |conn| {
        let submission = delegate::submission_with_conn(
            conn,
            &wallet_id,
            &round_id,
            bundle_index,
            DelegationSigner::signature(spend_auth_sig, sighash),
        )?;
        Ok(DelegationSubmissionWire::try_from(&submission)?
            .to_json()?
            .into_bytes())
    }
}

fn singleton_vote_body(
    conn: &rusqlite::Connection,
    wallet_id: &str,
    round_id: &str,
    bundle_index: u32,
    proposal_id: u32,
) -> Result<Vec<u8>, VotingError> {
    let signed =
        vote::signed_commitment_with_conn(conn, wallet_id, round_id, bundle_index, proposal_id)?;
    Ok(VoteCommitmentWire::try_from(&signed)?
        .to_json()?
        .into_bytes())
}

fn batch_body_from_json(batch_json: &str) -> Result<Vec<u8>, VotingError> {
    let wire: VoteCommitmentBatchWire =
        serde_json::from_str(batch_json).map_err(|_| VotingError::Internal {
            message: "persisted atomic vote batch is not valid wire JSON".to_string(),
        })?;
    Ok(wire.to_json()?.into_bytes())
}

type PayloadRebuild<'a> =
    &'a (dyn Fn(&rusqlite::Connection) -> Result<Vec<u8>, VotingError> + Send + Sync);

fn stale_generation_error() -> VotingError {
    VotingError::InvalidInput {
        message: "durable recovery generation changed before chain dispatch; recover the current \
                  submission and retry"
            .to_string(),
    }
}

/// Journals one dispatch attempt, binding it to the state it was built from.
///
/// The payload was serialized from durable state before this call, and another
/// database connection can clear or replace that state in the interval. So the
/// identity's round, its owner, and the payload itself are all revalidated
/// inside this one immediate transaction: the canonical bytes are re-derived
/// from storage and must still hash to `payload_digest`. A mismatch fails here,
/// before any request is dispatched, rather than POSTing bytes that no longer
/// describe what is stored.
fn reserve_attempt(
    db: &VotingDb,
    wallet_id: &str,
    identity: &ChainSubmissionIdentity,
    payload_digest: &[u8; 32],
    rebuild: PayloadRebuild<'_>,
) -> Result<i64, VotingError> {
    let now = now_seconds()?;
    let mut conn = db.conn();
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(internal("begin chain attempt reservation"))?;
    let round_exists: bool = tx
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM rounds WHERE round_id=?1 AND wallet_id=?2)",
            rusqlite::params![identity.round_id, wallet_id],
            |row| row.get(0),
        )
        .map_err(internal("validate chain attempt round"))?;
    if !round_exists {
        return Err(VotingError::InvalidInput {
            message: "chain submission round does not exist for this wallet".to_string(),
        });
    }
    let rebuilt: [u8; 32] = Sha256::digest(rebuild(&tx)?).into();
    if rebuilt != *payload_digest {
        return Err(stale_generation_error());
    }
    tx.execute(
        "INSERT INTO chain_submission_attempts
         (round_id, wallet_id, kind, bundle_index, proposal_id, batch_digest,
          payload_digest, state, created_at, updated_at)
         VALUES (:round_id, :wallet_id, :kind, :bundle_index, :proposal_id,
                 :batch_digest, :payload_digest, 'attempting', :now, :now)",
        named_params! {
            ":round_id": identity.round_id,
            ":wallet_id": wallet_id,
            ":kind": identity.kind.as_str(),
            ":bundle_index": i64::from(identity.bundle_index),
            ":proposal_id": identity.proposal_key(),
            ":batch_digest": identity.batch_key(),
            ":payload_digest": payload_digest.as_slice(),
            ":now": now,
        },
    )
    .map_err(internal("reserve chain submission attempt"))?;
    let id = tx.last_insert_rowid();
    tx.commit()
        .map_err(internal("commit chain attempt reservation"))?;
    Ok(id)
}

/// Records one attempt's classified outcome.
///
/// Scoped by the wallet captured when the attempt was reserved, not by the
/// database's current wallet. A host that switches accounts while a POST is in
/// flight must still be able to journal the response it already received;
/// re-reading the current wallet here would update zero rows and lose an
/// accepted transaction hash.
fn mark_attempt(
    db: &VotingDb,
    wallet_id: &str,
    attempt_id: i64,
    state: &str,
    tx_hash: Option<&str>,
) -> Result<(), VotingError> {
    let now = now_seconds()?;
    let conn = db.conn();
    let updated = conn
        .execute(
            "UPDATE chain_submission_attempts
                SET state=:state,
                    chain_tx_hash=COALESCE(:tx_hash, chain_tx_hash),
                    updated_at=:now
              WHERE id=:id AND wallet_id=:wallet_id",
            named_params! {
                ":state": state,
                ":tx_hash": tx_hash,
                ":now": now,
                ":id": attempt_id,
                ":wallet_id": wallet_id,
            },
        )
        .map_err(internal("record chain attempt outcome"))?;
    if updated != 1 {
        return Err(VotingError::InvalidInput {
            message: "chain submission attempt no longer exists".to_string(),
        });
    }
    Ok(())
}

/// Removes a reservation whose request was definitely never dispatched.
///
/// Wallet-scoped like [`mark_attempt`], so an account switch cannot leave the
/// reservation behind.
fn delete_attempt(db: &VotingDb, wallet_id: &str, attempt_id: i64) -> Result<(), VotingError> {
    db.conn()
        .execute(
            "DELETE FROM chain_submission_attempts WHERE id=?1 AND wallet_id=?2",
            rusqlite::params![attempt_id, wallet_id],
        )
        .map(|_| ())
        .map_err(internal("delete definitely-unsent chain attempt"))
}

/// Marks every attempt carrying this hash as definitively rejected.
///
/// A candidate whose transaction committed with a nonzero code can never
/// confirm, so its journal row must stop being live evidence: otherwise later
/// submissions rediscover it and exit before dispatch, and ballot-intent
/// changes and recovery cleanup stay pinned to a generation that failed.
fn retire_attempts_for_hash(
    db: &VotingDb,
    wallet_id: &str,
    identity: &ChainSubmissionIdentity,
    tx_hash: &str,
) -> Result<(), VotingError> {
    db.conn()
        .execute(
            "UPDATE chain_submission_attempts
                SET state='rejected', updated_at=:now
              WHERE round_id=:round_id AND wallet_id=:wallet_id
                AND kind=:kind AND bundle_index=:bundle_index
                AND proposal_id=:proposal_id AND batch_digest=:batch_digest
                AND chain_tx_hash=:tx_hash AND state<>'rejected'",
            named_params! {
                ":now": now_seconds()?,
                ":round_id": identity.round_id,
                ":wallet_id": wallet_id,
                ":kind": identity.kind.as_str(),
                ":bundle_index": i64::from(identity.bundle_index),
                ":proposal_id": identity.proposal_key(),
                ":batch_digest": identity.batch_key(),
                ":tx_hash": tx_hash,
            },
        )
        .map(|_| ())
        .map_err(internal("retire committed-failure chain attempt"))
}

/// Fails when the database's wallet is no longer the one this operation began
/// under, so a mid-flight account switch cannot write to the wrong wallet.
fn ensure_operation_wallet(db: &VotingDb, wallet_id: &str) -> Result<(), VotingError> {
    if db.wallet_id() == wallet_id {
        Ok(())
    } else {
        Err(VotingError::InvalidInput {
            message: "wallet changed during a chain submission operation".to_string(),
        })
    }
}

/// Every chain transaction hash that could identify this submission.
///
/// Candidates come from two sources with different histories: the legacy domain
/// columns, which preserve whatever casing their caller passed, and the attempt
/// journal, which stores hashes as the chain client normalized them. They are
/// canonicalized before deduplication so one transaction recorded under two
/// casings is queried once, instead of being counted as two committed
/// candidates and reported as an invariant violation.
fn known_hashes(
    db: &VotingDb,
    wallet_id: &str,
    identity: &ChainSubmissionIdentity,
) -> Result<Vec<String>, VotingError> {
    let conn = db.conn();
    let mut candidates = match identity.kind {
        ChainSubmissionKind::Delegation => queries::get_delegation_tx_hash(
            &conn,
            &identity.round_id,
            wallet_id,
            identity.bundle_index,
        )?
        .into_iter()
        .collect(),
        ChainSubmissionKind::Vote => queries::get_vote_tx_hash(
            &conn,
            &identity.round_id,
            wallet_id,
            identity.bundle_index,
            identity.require_proposal_id()?,
        )?
        .into_iter()
        .collect(),
        ChainSubmissionKind::VoteBatch => {
            let recoveries = match vote::load_vote_batch_recoveries_with_conn(
                &conn,
                wallet_id,
                &identity.round_id,
                identity.bundle_index,
                identity.require_batch_digest()?,
            ) {
                Ok(recoveries) => recoveries,
                // The member rows can legitimately be gone, for example after
                // recovery cleanup on a batch with no live attempt. That removes
                // a source of candidate hashes; it is not a reason to fail
                // reconciliation, which can still use the attempt journal below.
                Err(VotingError::InvalidInput { .. }) => Vec::new(),
                Err(error) => return Err(error),
            };
            let mut hashes = Vec::new();
            for recovery in recoveries {
                if let Some(hash) = queries::get_vote_tx_hash(
                    &conn,
                    &identity.round_id,
                    wallet_id,
                    identity.bundle_index,
                    recovery.proposal_id,
                )? {
                    hashes.push(hash);
                }
            }
            hashes
        }
    };
    let mut stmt = conn
        .prepare(
            // A definitively rejected attempt never entered the mempool, so its
            // hash can never commit. Leaving it a candidate would make lookup
            // report it as pending and block the replacement payload from ever
            // being posted.
            "SELECT chain_tx_hash
               FROM chain_submission_attempts
              WHERE round_id=:round_id AND wallet_id=:wallet_id
                AND kind=:kind AND bundle_index=:bundle_index
                AND proposal_id=:proposal_id AND batch_digest=:batch_digest
                AND chain_tx_hash IS NOT NULL AND state<>'rejected'
              ORDER BY id",
        )
        .map_err(internal("prepare known chain hashes query"))?;
    let rows = stmt
        .query_map(
            named_params! {
                ":round_id": identity.round_id,
                ":wallet_id": wallet_id,
                ":kind": identity.kind.as_str(),
                ":bundle_index": i64::from(identity.bundle_index),
                ":proposal_id": identity.proposal_key(),
                ":batch_digest": identity.batch_key(),
            },
            |row| row.get::<_, String>(0),
        )
        .map_err(internal("query known chain hashes"))?;
    for row in rows {
        candidates.push(row.map_err(internal("read known chain hash"))?);
    }
    let mut hashes: Vec<String> = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        // Drop anything the transaction-status endpoint would reject outright.
        // A pre-lifecycle host could have recorded an opaque identifier, and one
        // such row must not turn every reconciliation for this identity into a
        // hard error.
        let Ok(canonical) = crate::chain::normalize_tx_hash(&candidate) else {
            continue;
        };
        if !hashes.contains(&canonical) {
            hashes.push(canonical);
        }
    }
    Ok(hashes)
}

/// Vote rows still covered by a journaled chain submission attempt.
///
/// A row is covered when a singleton attempt names its proposal, or when a
/// vote-batch attempt names the batch digest its recovery carries. Scoping by
/// digest matters: any batch attempt in a bundle used to freeze every vote in
/// that bundle, including a later unrelated singleton's retryable recovery.
///
/// `rejected` attempts are excluded. A rejection is definite for its own
/// attempt, and nothing ever deletes those rows, so treating them as coverage
/// would freeze a proposal's recovery state permanently with no recovery path.
///
/// Rows whose recovery JSON cannot be parsed are covered conservatively when
/// their bundle has any batch attempt: an unreadable row may still be a member,
/// and erasing a member of an in-flight batch is the failure this prevents.
pub(crate) fn attempt_protected_vote_rows(
    conn: &rusqlite::Connection,
    round_id: &str,
    wallet_id: &str,
) -> Result<BTreeSet<(u32, u32)>, VotingError> {
    let mut protected = BTreeSet::new();
    let mut batch_digests: BTreeMap<u32, BTreeSet<[u8; 32]>> = BTreeMap::new();
    {
        let mut stmt = conn
            .prepare(
                "SELECT kind, bundle_index, proposal_id, batch_digest
                   FROM chain_submission_attempts
                  WHERE round_id=:round_id AND wallet_id=:wallet_id
                    AND state<>'rejected' AND kind IN ('vote','vote_batch')",
            )
            .map_err(internal("prepare attempted vote coverage query"))?;
        let rows = stmt
            .query_map(
                named_params! { ":round_id": round_id, ":wallet_id": wallet_id },
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Vec<u8>>(3)?,
                    ))
                },
            )
            .map_err(internal("query attempted vote coverage"))?;
        for row in rows {
            let (kind, bundle_index, proposal_id, digest) =
                row.map_err(internal("read attempted vote coverage row"))?;
            let Ok(bundle_index) = u32::try_from(bundle_index) else {
                continue;
            };
            if kind == ChainSubmissionKind::Vote.as_str() {
                if let Ok(proposal_id) = u32::try_from(proposal_id) {
                    protected.insert((bundle_index, proposal_id));
                }
            } else if let Ok(digest) = <[u8; 32]>::try_from(digest.as_slice()) {
                batch_digests
                    .entry(bundle_index)
                    .or_default()
                    .insert(digest);
            }
        }
    }
    if batch_digests.is_empty() {
        return Ok(protected);
    }
    let mut stmt = conn
        .prepare(
            "SELECT bundle_index, proposal_id, commitment_bundle_json
               FROM votes
              WHERE round_id=:round_id AND wallet_id=:wallet_id
                AND commitment_bundle_json IS NOT NULL",
        )
        .map_err(internal("prepare batch membership query"))?;
    let rows = stmt
        .query_map(
            named_params! { ":round_id": round_id, ":wallet_id": wallet_id },
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .map_err(internal("query batch membership"))?;
    for row in rows {
        let (bundle_index, proposal_id, json) =
            row.map_err(internal("read batch membership row"))?;
        let (Ok(bundle_index), Ok(proposal_id)) =
            (u32::try_from(bundle_index), u32::try_from(proposal_id))
        else {
            continue;
        };
        let Some(digests) = batch_digests.get(&bundle_index) else {
            continue;
        };
        let covered = match vote::parse_recovery(&json) {
            Ok(recovery) => recovery
                .batch
                .as_ref()
                .is_some_and(|batch| digests.contains(&batch.digest)),
            Err(_) => true,
        };
        if covered {
            protected.insert((bundle_index, proposal_id));
        }
    }
    Ok(protected)
}

fn now_seconds() -> Result<i64, VotingError> {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| VotingError::Internal {
            message: format!("system clock before Unix epoch: {error}"),
        })?
        .as_secs();
    i64::try_from(seconds).map_err(|_| VotingError::Internal {
        message: "current Unix time does not fit in SQLite integer".to_string(),
    })
}

fn decode_canonical_array<const N: usize>(
    encoded: &str,
    field: &str,
) -> Result<[u8; N], VotingError> {
    let bytes = BASE64_STANDARD
        .decode(encoded)
        .map_err(|error| VotingError::InvalidInput {
            message: format!("{field} is not valid standard Base64: {error}"),
        })?;
    if BASE64_STANDARD.encode(&bytes) != encoded {
        return Err(VotingError::InvalidInput {
            message: format!("{field} must use canonical padded standard Base64"),
        });
    }
    bytes
        .try_into()
        .map_err(|bytes: Vec<u8>| VotingError::InvalidInput {
            message: format!("{field} must be {N} bytes, got {}", bytes.len()),
        })
}

fn internal(context: &'static str) -> impl FnOnce(rusqlite::Error) -> VotingError + Copy {
    move |error| VotingError::Internal {
        message: format!("{context} failed: {error}"),
    }
}

type OperationLock = Arc<tokio::sync::Mutex<()>>;

fn operation_lock(key: &str) -> Result<OperationLock, VotingError> {
    static LOCKS: OnceLock<Mutex<HashMap<String, OperationLock>>> = OnceLock::new();
    let mut locks = LOCKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .map_err(|error| VotingError::Internal {
            message: format!("chain operation lock registry poisoned: {error}"),
        })?;
    Ok(locks
        .entry(key.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone())
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Mutex, time::Duration};

    use super::*;
    use crate::{
        chain::{
            transport::{ChainFuture, ChainResponse, ChainTransport, ChainTransportError},
            ChainEndpointSet,
        },
        round::RoundParams,
        Network,
    };

    const ROUND_ID: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const TX_HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const WALLET: &str = "wallet-1";

    #[derive(Default)]
    struct MockTransport {
        responses: Mutex<VecDeque<Result<ChainResponse, ChainTransportError>>>,
        posts: Mutex<usize>,
        gets: Mutex<usize>,
    }

    impl ChainTransport for MockTransport {
        fn get<'a>(&'a self, _url: &'a str, _timeout: Duration) -> ChainFuture<'a> {
            Box::pin(async move {
                let response = self
                    .responses
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("mock GET response");
                *self.gets.lock().unwrap() += 1;
                response
            })
        }

        fn post_json<'a>(
            &'a self,
            _url: &'a str,
            _body: Vec<u8>,
            _timeout: Duration,
        ) -> ChainFuture<'a> {
            Box::pin(async move {
                *self.posts.lock().unwrap() += 1;
                self.responses
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("mock POST response")
            })
        }
    }

    fn response(status: u16, body: &str) -> ChainResponse {
        ChainResponse::json(status, body.as_bytes().to_vec())
    }

    /// Rebuild for tests that post a fixed body rather than a real payload.
    fn echo_rebuild(_conn: &rusqlite::Connection) -> Result<Vec<u8>, VotingError> {
        Ok(b"{}".to_vec())
    }

    fn test_db() -> VotingDb {
        let db = VotingDb::open_in_memory().unwrap();
        db.set_wallet_id("wallet-1");
        db.create_round(
            Network::Testnet,
            &RoundParams {
                vote_round_id: ROUND_ID.to_string(),
                snapshot_height: 100,
                ea_pk: vec![0xEA; 32],
                nc_root: vec![0xAA; 32],
                nullifier_imt_root: vec![0xBB; 32],
            },
            None,
        )
        .unwrap();
        db.conn()
            .execute(
                "INSERT INTO bundles (round_id, wallet_id, bundle_index)
                 VALUES (?1, ?2, 0)",
                rusqlite::params![ROUND_ID, db.wallet_id()],
            )
            .unwrap();
        db
    }

    #[tokio::test]
    async fn check_tx_acceptance_is_journaled_without_domain_mutation() {
        let db = test_db();
        let transport = Arc::new(MockTransport::default());
        transport.responses.lock().unwrap().push_back(Ok(response(
            200,
            &format!(r#"{{"tx_hash":"{TX_HASH}","code":0,"log":""}}"#),
        )));
        let client = ChainClient::new(
            transport,
            ChainEndpointSet::new(&["https://vote.example".to_string()]).unwrap(),
        );
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);
        let identity = ChainSubmissionIdentity::delegation(ROUND_ID, 0);

        let outcome = lifecycle
            .submit_body_locked(WALLET, identity, b"{}".to_vec(), &echo_rebuild, &|| false)
            .await
            .unwrap();

        assert_eq!(
            outcome,
            ChainLifecycleOutcome::Accepted {
                tx_hash: TX_HASH.to_string()
            }
        );
        assert_eq!(db.get_delegation_tx_hash(ROUND_ID, 0).unwrap(), None);
        let attempt: (String, String) = db
            .conn()
            .query_row(
                "SELECT state, chain_tx_hash FROM chain_submission_attempts",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(attempt, ("accepted".to_string(), TX_HASH.to_string()));
    }

    #[tokio::test]
    async fn known_pending_hash_is_reconciled_without_another_post() {
        let db = test_db();
        let transport = Arc::new(MockTransport::default());
        transport.responses.lock().unwrap().extend([
            Ok(response(
                200,
                &format!(r#"{{"tx_hash":"{TX_HASH}","code":0,"log":""}}"#),
            )),
            Ok(response(404, r#"{"message":"not indexed"}"#)),
        ]);
        let client = ChainClient::new(
            transport.clone(),
            ChainEndpointSet::new(&["https://vote.example".to_string()]).unwrap(),
        );
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);
        let identity = ChainSubmissionIdentity::delegation(ROUND_ID, 0);

        lifecycle
            .submit_body_locked(
                WALLET,
                identity.clone(),
                b"{}".to_vec(),
                &echo_rebuild,
                &|| false,
            )
            .await
            .unwrap();
        let outcome = lifecycle
            .submit_body_locked(WALLET, identity, b"{}".to_vec(), &echo_rebuild, &|| false)
            .await
            .unwrap();

        assert_eq!(
            outcome,
            ChainLifecycleOutcome::Pending {
                known_tx_hashes: vec![TX_HASH.to_string()]
            }
        );
        assert_eq!(*transport.posts.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn committed_failure_rejects_without_pinning_domain_hash() {
        let db = test_db();
        let transport = Arc::new(MockTransport::default());
        transport.responses.lock().unwrap().extend([
            Ok(response(
                200,
                &format!(r#"{{"tx_hash":"{TX_HASH}","code":0,"log":""}}"#),
            )),
            Ok(response(
                200,
                r#"{"height":42,"code":7,"log":"deliver failed","events":[]}"#,
            )),
        ]);
        let client = ChainClient::new(
            transport,
            ChainEndpointSet::new(&["https://vote.example".to_string()]).unwrap(),
        );
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);
        let identity = ChainSubmissionIdentity::delegation(ROUND_ID, 0);

        lifecycle
            .submit_body_locked(
                WALLET,
                identity.clone(),
                b"{}".to_vec(),
                &echo_rebuild,
                &|| false,
            )
            .await
            .unwrap();
        let outcome = lifecycle.reconcile(&identity, &|| false).await.unwrap();

        assert_eq!(
            outcome,
            ChainLifecycleOutcome::Rejected {
                code: 7,
                log: "deliver failed".to_string()
            }
        );
        assert_eq!(db.get_delegation_tx_hash(ROUND_ID, 0).unwrap(), None);
    }

    /// Rebuild that reads a durable column, standing in for a real payload
    /// reconstruction: the bytes it returns change when storage changes.
    fn durable_rebuild(conn: &rusqlite::Connection) -> Result<Vec<u8>, VotingError> {
        conn.query_row(
            "SELECT COALESCE(gov_comm, X'') FROM bundles WHERE round_id=?1 AND bundle_index=0",
            rusqlite::params![ROUND_ID],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .map_err(|error| VotingError::Internal {
            message: format!("test rebuild failed: {error}"),
        })
    }

    fn set_generation(db: &VotingDb, generation: &[u8]) {
        db.conn()
            .execute(
                "UPDATE bundles SET gov_comm=?1 WHERE round_id=?2 AND bundle_index=0",
                rusqlite::params![generation, ROUND_ID],
            )
            .unwrap();
    }

    fn accepted_client(transport: Arc<MockTransport>) -> ChainClient {
        ChainClient::new(
            transport,
            ChainEndpointSet::new(&["https://vote.example".to_string()]).unwrap(),
        )
    }

    #[tokio::test]
    async fn reservation_rejects_a_changed_durable_generation() {
        let db = test_db();
        let transport = Arc::new(MockTransport::default());
        let client = accepted_client(transport.clone());
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);
        let identity = ChainSubmissionIdentity::delegation(ROUND_ID, 0);

        // The payload was serialized from generation one; storage has since
        // moved to generation two.
        set_generation(&db, b"generation-two");
        let error = lifecycle
            .submit_body_locked(
                WALLET,
                identity,
                b"generation-one".to_vec(),
                &durable_rebuild,
                &|| false,
            )
            .await
            .unwrap_err();

        assert!(
            error.to_string().contains("durable recovery generation"),
            "got {error}"
        );
        assert_eq!(*transport.posts.lock().unwrap(), 0, "nothing may be sent");
        let attempts: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM chain_submission_attempts",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(attempts, 0, "a rejected reservation must not be journaled");
    }

    #[tokio::test]
    async fn reservation_accepts_the_matching_durable_generation() {
        let db = test_db();
        let transport = Arc::new(MockTransport::default());
        transport.responses.lock().unwrap().push_back(Ok(response(
            200,
            &format!(r#"{{"tx_hash":"{TX_HASH}","code":0,"log":""}}"#),
        )));
        let client = accepted_client(transport.clone());
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);

        set_generation(&db, b"generation-one");
        let outcome = lifecycle
            .submit_body_locked(
                WALLET,
                ChainSubmissionIdentity::delegation(ROUND_ID, 0),
                b"generation-one".to_vec(),
                &durable_rebuild,
                &|| false,
            )
            .await
            .unwrap();

        assert_eq!(
            outcome,
            ChainLifecycleOutcome::Accepted {
                tx_hash: TX_HASH.to_string()
            }
        );
        assert_eq!(*transport.posts.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn cancellation_after_lookup_suppresses_the_confirmation_write() {
        let db = test_db();
        let transport = Arc::new(MockTransport::default());
        transport.responses.lock().unwrap().extend([
            Ok(response(
                200,
                &format!(r#"{{"tx_hash":"{TX_HASH}","code":0,"log":""}}"#),
            )),
            // Committed success, but with no delegation event. Confirmation
            // would fail loudly, so returning `Cancelled` proves the durable
            // write was skipped rather than merely failing.
            Ok(response(
                200,
                r#"{"height":42,"code":0,"log":"","events":[]}"#,
            )),
        ]);
        let client = accepted_client(transport.clone());
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);
        let identity = ChainSubmissionIdentity::delegation(ROUND_ID, 0);

        lifecycle
            .submit_body_locked(
                WALLET,
                identity.clone(),
                b"{}".to_vec(),
                &echo_rebuild,
                &|| false,
            )
            .await
            .unwrap();

        // The host invalidates the session while the status request is in
        // flight, so cancellation first becomes observable once the GET has
        // completed and every earlier checkpoint has already passed.
        let outcome = lifecycle
            .reconcile(&identity, &|| *transport.gets.lock().unwrap() > 0)
            .await
            .unwrap();

        assert_eq!(outcome, ChainLifecycleOutcome::Cancelled);
        assert_eq!(db.get_delegation_tx_hash(ROUND_ID, 0).unwrap(), None);
    }

    #[tokio::test]
    async fn one_transaction_recorded_in_two_casings_is_looked_up_once() {
        let db = test_db();
        let transport = Arc::new(MockTransport::default());
        transport.responses.lock().unwrap().extend([
            Ok(response(
                200,
                &format!(r#"{{"tx_hash":"{TX_HASH}","code":0,"log":""}}"#),
            )),
            Ok(response(404, r#"{"message":"not indexed"}"#)),
        ]);
        let client = accepted_client(transport.clone());
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);
        let identity = ChainSubmissionIdentity::delegation(ROUND_ID, 0);

        lifecycle
            .submit_body_locked(
                WALLET,
                identity.clone(),
                b"{}".to_vec(),
                &echo_rebuild,
                &|| false,
            )
            .await
            .unwrap();
        // A row written before hashes were canonicalized at the storage
        // boundary, in the other casing.
        db.conn()
            .execute(
                "UPDATE bundles SET delegation_tx_hash=?1 WHERE round_id=?2 AND bundle_index=0",
                rusqlite::params![TX_HASH.to_ascii_uppercase(), ROUND_ID],
            )
            .unwrap();

        let outcome = lifecycle.reconcile(&identity, &|| false).await.unwrap();

        assert_eq!(
            outcome,
            ChainLifecycleOutcome::Pending {
                known_tx_hashes: vec![TX_HASH.to_string()]
            },
            "the two casings name one transaction, not two candidates"
        );
        assert!(
            transport.responses.lock().unwrap().is_empty(),
            "exactly one status lookup should have been issued"
        );
    }

    #[tokio::test]
    async fn a_legacy_opaque_hash_does_not_break_reconciliation() {
        let db = test_db();
        db.conn()
            .execute(
                "UPDATE bundles SET delegation_tx_hash='legacy-hash'
                  WHERE round_id=?1 AND bundle_index=0",
                rusqlite::params![ROUND_ID],
            )
            .unwrap();
        let transport = Arc::new(MockTransport::default());
        let client = accepted_client(transport.clone());
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);

        let outcome = lifecycle
            .reconcile(&ChainSubmissionIdentity::delegation(ROUND_ID, 0), &|| false)
            .await
            .unwrap();

        // A pre-lifecycle host could record an opaque identifier. It is not a
        // chain hash, so it is skipped rather than turning every reconciliation
        // for this identity into a hard error.
        assert_eq!(
            outcome,
            ChainLifecycleOutcome::Pending {
                known_tx_hashes: Vec::new()
            }
        );
    }

    fn journal_attempt(db: &VotingDb, state: &str, tx_hash: Option<&str>) -> i64 {
        let conn = db.conn();
        conn.execute(
            "INSERT INTO chain_submission_attempts
             (round_id, wallet_id, kind, bundle_index, proposal_id, batch_digest,
              payload_digest, chain_tx_hash, state, created_at, updated_at)
             VALUES (?1, ?2, 'delegation', 0, -1, X'', ?3, ?4, ?5, 1, 1)",
            rusqlite::params![ROUND_ID, WALLET, vec![0xCC_u8; 32], tx_hash, state],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn attempt_states(db: &VotingDb) -> Vec<String> {
        db.conn()
            .prepare("SELECT state FROM chain_submission_attempts ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    #[tokio::test]
    async fn a_rejected_attempt_hash_is_not_a_reconciliation_candidate() {
        let db = test_db();
        journal_attempt(&db, "rejected", Some(TX_HASH));
        let transport = Arc::new(MockTransport::default());
        let client = accepted_client(transport.clone());
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);

        let outcome = lifecycle
            .reconcile(&ChainSubmissionIdentity::delegation(ROUND_ID, 0), &|| false)
            .await
            .unwrap();

        // A CheckTx rejection never entered the mempool, so its hash can never
        // commit. Keeping it a candidate would have lookup report it as pending
        // and block the replacement payload from ever being posted.
        assert_eq!(
            outcome,
            ChainLifecycleOutcome::Pending {
                known_tx_hashes: Vec::new()
            }
        );
        assert_eq!(*transport.gets.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn a_committed_failure_retires_its_attempt_and_frees_the_next_submission() {
        let db = test_db();
        let transport = Arc::new(MockTransport::default());
        transport.responses.lock().unwrap().extend([
            Ok(response(
                200,
                &format!(r#"{{"tx_hash":"{TX_HASH}","code":0,"log":""}}"#),
            )),
            Ok(response(
                200,
                r#"{"height":42,"code":7,"log":"deliver failed","events":[]}"#,
            )),
            Ok(response(
                200,
                &format!(r#"{{"tx_hash":"{TX_HASH}","code":0,"log":""}}"#),
            )),
        ]);
        let client = accepted_client(transport.clone());
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);
        let identity = ChainSubmissionIdentity::delegation(ROUND_ID, 0);

        lifecycle
            .submit_body_locked(
                WALLET,
                identity.clone(),
                b"{}".to_vec(),
                &echo_rebuild,
                &|| false,
            )
            .await
            .unwrap();
        let rejected = lifecycle.reconcile(&identity, &|| false).await.unwrap();

        assert!(matches!(
            rejected,
            ChainLifecycleOutcome::Rejected { code: 7, .. }
        ));
        assert_eq!(attempt_states(&db), vec!["rejected".to_string()]);

        // The failed candidate must stop blocking dispatch: a replacement
        // payload is posted instead of rediscovering a transaction that can
        // never confirm.
        let outcome = lifecycle
            .submit_body_locked(WALLET, identity, b"{}".to_vec(), &echo_rebuild, &|| false)
            .await
            .unwrap();
        assert_eq!(
            outcome,
            ChainLifecycleOutcome::Accepted {
                tx_hash: TX_HASH.to_string()
            }
        );
        assert_eq!(*transport.posts.lock().unwrap(), 2);
    }

    #[tokio::test]
    async fn an_unreadable_lookup_is_reported_unknown_and_still_blocks_rebroadcast() {
        let db = test_db();
        journal_attempt(&db, "outcome_unknown", Some(TX_HASH));
        let transport = Arc::new(MockTransport::default());
        transport.responses.lock().unwrap().extend([
            Ok(response(200, "{not json")),
            Ok(response(200, "{not json")),
        ]);
        let client = accepted_client(transport.clone());
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);
        let identity = ChainSubmissionIdentity::delegation(ROUND_ID, 0);

        let outcome = lifecycle.reconcile(&identity, &|| false).await.unwrap();

        // A broken or incompatible endpoint must be distinguishable from a
        // genuine 404, or callers poll forever believing the transaction is
        // simply not indexed yet.
        assert!(
            matches!(
                &outcome,
                ChainLifecycleOutcome::OutcomeUnknown { known_tx_hashes, .. }
                    if known_tx_hashes == &vec![TX_HASH.to_string()]
            ),
            "got {outcome:?}"
        );

        let resubmit = lifecycle
            .submit_body_locked(WALLET, identity, b"{}".to_vec(), &echo_rebuild, &|| false)
            .await
            .unwrap();
        assert!(matches!(
            resubmit,
            ChainLifecycleOutcome::OutcomeUnknown { .. }
        ));
        assert_eq!(
            *transport.posts.lock().unwrap(),
            0,
            "a candidate whose status could not be read may still commit"
        );
    }

    #[test]
    fn outcomes_are_journaled_under_the_wallet_that_reserved_them() {
        let db = test_db();
        let attempt_id = journal_attempt(&db, "attempting", None);
        // The host switches accounts while the request is in flight.
        db.set_wallet_id("wallet-2");

        mark_attempt(&db, WALLET, attempt_id, "accepted", Some(TX_HASH)).unwrap();

        let identity = ChainSubmissionIdentity::delegation(ROUND_ID, 0);
        assert_eq!(
            known_hashes(&db, WALLET, &identity).unwrap(),
            vec![TX_HASH.to_string()],
            "an accepted hash must not be lost to an account switch"
        );
        // And the reservation's owner is what a definitely-unsent deletion uses.
        let unsent = journal_attempt(&db, "attempting", None);
        delete_attempt(&db, WALLET, unsent).unwrap();
        assert_eq!(attempt_states(&db), vec!["accepted".to_string()]);
    }

    #[tokio::test]
    async fn a_padded_legacy_hash_is_treated_as_opaque() {
        let db = test_db();
        db.conn()
            .execute(
                "UPDATE bundles SET delegation_tx_hash=?1 WHERE round_id=?2 AND bundle_index=0",
                rusqlite::params![format!(" {TX_HASH} "), ROUND_ID],
            )
            .unwrap();
        let transport = Arc::new(MockTransport::default());
        let client = accepted_client(transport.clone());
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);

        let outcome = lifecycle
            .reconcile(&ChainSubmissionIdentity::delegation(ROUND_ID, 0), &|| false)
            .await
            .unwrap();

        // Storage leaves a padded value unchanged, so accepting it here would
        // confirm a hash that then conflicts with the padded stored value.
        assert_eq!(
            outcome,
            ChainLifecycleOutcome::Pending {
                known_tx_hashes: Vec::new()
            }
        );
        assert_eq!(*transport.gets.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn a_batch_member_is_never_dispatched_as_a_singleton_vote() {
        use crate::types::EncryptedShare;
        use crate::vote::{VoteBatchRecovery, VoteRecoveryBundle};

        let db = test_db();
        queries::store_vote(&db.conn(), ROUND_ID, WALLET, 0, 1, 0, &[0xAA; 32]).unwrap();
        let recovery = VoteRecoveryBundle {
            vote_round_id: ROUND_ID.to_string(),
            bundle_index: 0,
            proposal_id: 1,
            vote_decision: 0,
            anchor_height: 123,
            vc_tree_position: 0,
            single_share: false,
            num_options: 3,
            van_nullifier: [0x10; 32],
            vote_authority_note_new: [0x11; 32],
            vote_commitment: [0x12; 32],
            proof: vec![0x13; 96],
            shares_hash: [0x14; 32],
            r_vpk: [0x15; 32],
            alpha_v: [0x16; 32],
            vote_auth_sig: [0x17; 64],
            encrypted_shares: vec![EncryptedShare {
                c1: vec![0x21; 32],
                c2: vec![0x22; 32],
                share_index: 0,
                plaintext_value: 5,
                randomness: vec![0x23; 32],
            }],
            share_blinds: vec![[0x41; 32]],
            share_comms: vec![[0x51; 32]],
            batch: Some(VoteBatchRecovery {
                digest: [0xD1; 32],
                index: 0,
                size: 1,
            }),
        };
        db.conn()
            .execute(
                "UPDATE votes SET commitment_bundle_json=?1
                  WHERE round_id=?2 AND wallet_id=?3 AND bundle_index=0 AND proposal_id=1",
                rusqlite::params![
                    crate::vote::serialize_recovery(&recovery).unwrap(),
                    ROUND_ID,
                    WALLET
                ],
            )
            .unwrap();
        let transport = Arc::new(MockTransport::default());
        let client = accepted_client(transport.clone());
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);

        let error = lifecycle
            .submit_vote(ROUND_ID, 0, 1, &|| false)
            .await
            .unwrap_err();

        // Posting one member to `cast-vote` could spend part of the batch
        // independently, and confirmation would reject it afterwards anyway.
        assert!(error.to_string().contains("atomic batch"), "{error}");
        assert_eq!(*transport.posts.lock().unwrap(), 0);
        let attempts: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM chain_submission_attempts",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(attempts, 0);
    }

    #[test]
    fn identities_cannot_pair_a_kind_with_the_wrong_key() {
        let delegation = ChainSubmissionIdentity::delegation(ROUND_ID, 0);
        assert_eq!(delegation.kind(), ChainSubmissionKind::Delegation);
        assert_eq!(delegation.proposal_id(), None);
        assert_eq!(delegation.batch_digest(), None);
        assert!(delegation.require_proposal_id().is_err());
        assert!(delegation.require_batch_digest().is_err());

        let vote = ChainSubmissionIdentity::vote(ROUND_ID, 1, 7);
        assert_eq!(vote.round_id(), ROUND_ID);
        assert_eq!(vote.bundle_index(), 1);
        assert_eq!(vote.proposal_id(), Some(7));
        assert_eq!(vote.batch_digest(), None);

        let batch = ChainSubmissionIdentity::vote_batch(ROUND_ID, 2, [9; 32]);
        assert_eq!(batch.proposal_id(), None);
        assert_eq!(batch.batch_digest(), Some([9; 32]));
        assert_eq!(batch.require_batch_digest().unwrap(), [9; 32]);
    }
}
