//! Durable vote-chain submission and confirmation lifecycle.

use std::{
    collections::HashMap,
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
    vote::{self, CommittedVote},
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainSubmissionIdentity {
    pub round_id: String,
    pub kind: ChainSubmissionKind,
    pub bundle_index: u32,
    pub proposal_id: Option<u32>,
    pub batch_digest: Option<[u8; 32]>,
}

impl ChainSubmissionIdentity {
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
        let lock = operation_lock(&identity.lock_key(&self.db.wallet_id()))?;
        let _guard = lock.lock().await;
        let recovered = delegate::submission(
            self.db,
            &round_id,
            bundle.bundle_index,
            DelegationSigner::signature(
                bundle.submission.spend_auth_sig,
                bundle.submission.sighash,
            ),
        )?;
        if recovered != bundle.submission {
            return Err(VotingError::InvalidInput {
                message: "signed delegation does not match durable bundle generation".to_string(),
            }
            .into());
        }
        let wire = DelegationSubmissionWire::try_from(&bundle.submission)?;
        let body = wire.to_json()?.into_bytes();
        self.submit_body_locked(identity, body, cancel).await
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
        let lock = operation_lock(&identity.lock_key(&self.db.wallet_id()))?;
        let _guard = lock.lock().await;
        let signature = decode_canonical_array::<64>(&wire.spend_auth_sig, "spend_auth_sig")?;
        let sighash = {
            let conn = self.db.conn();
            let bytes =
                queries::load_pczt_sighash(&conn, round_id, &self.db.wallet_id(), bundle_index)?;
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
        self.submit_body_locked(identity, body, cancel).await
    }

    pub async fn submit_vote(
        &self,
        round_id: &str,
        bundle_index: u32,
        proposal_id: u32,
        cancel: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<ChainLifecycleOutcome, ChainLifecycleError> {
        let identity = ChainSubmissionIdentity::vote(round_id, bundle_index, proposal_id);
        let lock = operation_lock(&identity.lock_key(&self.db.wallet_id()))?;
        let _guard = lock.lock().await;
        let committed = CommittedVote::recover(self.db, round_id, bundle_index, proposal_id)?;
        let signed = committed.signed_commitment(self.db)?;
        let wire = VoteCommitmentWire::try_from(&signed)?;
        let body = wire.to_json()?.into_bytes();
        self.submit_body_locked(identity, body, cancel).await
    }

    pub async fn submit_vote_batch(
        &self,
        round_id: &str,
        bundle_index: u32,
        proposal_id: u32,
        cancel: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<ChainLifecycleOutcome, ChainLifecycleError> {
        let lock_identity = ChainSubmissionIdentity::vote_batch(round_id, bundle_index, [0; 32]);
        let lock = operation_lock(&lock_identity.lock_key(&self.db.wallet_id()))?;
        let _guard = lock.lock().await;
        let batch = vote::recover_atomic_vote_batch(self.db, round_id, bundle_index, proposal_id)?;
        let wire: VoteCommitmentBatchWire =
            serde_json::from_str(&batch.batch_json).map_err(|_| VotingError::Internal {
                message: "persisted atomic vote batch is not valid wire JSON".to_string(),
            })?;
        let body = wire.to_json()?.into_bytes();
        let identity =
            ChainSubmissionIdentity::vote_batch(round_id, bundle_index, batch.batch_digest);
        self.submit_body_locked(identity, body, cancel).await
    }

    pub async fn reconcile(
        &self,
        identity: &ChainSubmissionIdentity,
        cancel: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<ChainLifecycleOutcome, ChainLifecycleError> {
        let lock = operation_lock(&identity.lock_key(&self.db.wallet_id()))?;
        let _guard = lock.lock().await;
        self.reconcile_locked(identity, cancel).await
    }

    async fn submit_body_locked(
        &self,
        identity: ChainSubmissionIdentity,
        body: Vec<u8>,
        cancel: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<ChainLifecycleOutcome, ChainLifecycleError> {
        let existing = self.reconcile_locked(&identity, cancel).await?;
        match &existing {
            ChainLifecycleOutcome::Confirmed { .. }
            | ChainLifecycleOutcome::Rejected { .. }
            | ChainLifecycleOutcome::Cancelled => return Ok(existing),
            ChainLifecycleOutcome::Pending { known_tx_hashes } if !known_tx_hashes.is_empty() => {
                return Ok(existing)
            }
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
                let reconciled = self.reconcile_locked(&identity, cancel).await?;
                if matches!(reconciled, ChainLifecycleOutcome::Confirmed { .. }) {
                    return Ok(reconciled);
                }
            }

            let attempt_id = reserve_attempt(self.db, &identity, &digest)?;
            if cancel() {
                delete_attempt(self.db, attempt_id)?;
                return Ok(ChainLifecycleOutcome::Cancelled);
            }

            let result = self
                .client
                .post_once(attempt_index, identity.kind.endpoint(), body.clone())
                .await;
            match result {
                Ok(ChainBroadcastOutcome::Accepted(result)) => {
                    mark_attempt(self.db, attempt_id, "accepted", Some(&result.tx_hash))?;
                    return Ok(ChainLifecycleOutcome::Accepted {
                        tx_hash: result.tx_hash,
                    });
                }
                Ok(ChainBroadcastOutcome::Rejected(result)) => {
                    let tx_hash = (!result.tx_hash.is_empty()).then_some(result.tx_hash.as_str());
                    mark_attempt(self.db, attempt_id, "rejected", tx_hash)?;
                    if is_spent_nullifier_log(&result.log) {
                        let reconciled = self.reconcile_locked(&identity, cancel).await?;
                        if matches!(reconciled, ChainLifecycleOutcome::Confirmed { .. }) {
                            return Ok(reconciled);
                        }
                        return Ok(ChainLifecycleOutcome::AlreadySpentUnresolved {
                            known_tx_hashes: known_hashes(self.db, &identity)?,
                            log: result.log,
                        });
                    }
                    return Ok(ChainLifecycleOutcome::Rejected {
                        code: result.code,
                        log: result.log,
                    });
                }
                Ok(ChainBroadcastOutcome::OutcomeUnknown { message }) => {
                    mark_attempt(self.db, attempt_id, "outcome_unknown", None)?;
                    last_unknown = Some(message);
                }
                Ok(ChainBroadcastOutcome::Cancelled) => {
                    mark_attempt(self.db, attempt_id, "outcome_unknown", None)?;
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
                        delete_attempt(self.db, attempt_id)?;
                    } else if error.is_ambiguous() {
                        mark_attempt(self.db, attempt_id, "outcome_unknown", None)?;
                    } else {
                        mark_attempt(self.db, attempt_id, "rejected", None)?;
                    }
                    if !error.is_retryable() || attempt_index + 1 == attempts {
                        if error.is_ambiguous() {
                            return Ok(ChainLifecycleOutcome::OutcomeUnknown {
                                known_tx_hashes: known_hashes(self.db, &identity)?,
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
            known_tx_hashes: known_hashes(self.db, &identity)?,
            message: last_unknown.unwrap_or_else(|| "submission outcome is unknown".to_string()),
        })
    }

    async fn reconcile_locked(
        &self,
        identity: &ChainSubmissionIdentity,
        cancel: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<ChainLifecycleOutcome, ChainLifecycleError> {
        let hashes = known_hashes(self.db, identity)?;
        if hashes.is_empty() {
            return Ok(ChainLifecycleOutcome::Pending {
                known_tx_hashes: hashes,
            });
        }
        let mut successful = Vec::new();
        let mut any_pending = false;
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
                Ok(ChainTxStatus::Committed(confirmation)) => committed_failures.push(confirmation),
                Err(ChainError::Cancelled) => return Ok(ChainLifecycleOutcome::Cancelled),
                Err(error) if error.is_retryable() || error.is_ambiguous() => any_pending = true,
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
            let confirmation = apply_confirmation(self.db, identity, &hash, &response)?;
            return Ok(ChainLifecycleOutcome::Confirmed {
                tx_hash: hash,
                confirmation,
                reconciled: true,
            });
        }
        if any_pending {
            return Ok(ChainLifecycleOutcome::Pending {
                known_tx_hashes: hashes,
            });
        }
        if let Some(failure) = committed_failures.into_iter().next() {
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
    identity: &ChainSubmissionIdentity,
    tx_hash: &str,
    response: &ChainTxConfirmation,
) -> Result<ChainConfirmation, VotingError> {
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
            identity.proposal_id.expect("vote identity has proposal"),
            tx_hash,
            &response.events,
        )
        .map(ChainConfirmation::Vote),
        ChainSubmissionKind::VoteBatch => confirm_vote_batch_submission(
            db,
            &identity.round_id,
            identity.bundle_index,
            identity.batch_key(),
            tx_hash,
            &response.events,
        )
        .map(ChainConfirmation::VoteBatch),
    }
}

fn reserve_attempt(
    db: &VotingDb,
    identity: &ChainSubmissionIdentity,
    payload_digest: &[u8; 32],
) -> Result<i64, VotingError> {
    let wallet_id = db.wallet_id();
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

fn mark_attempt(
    db: &VotingDb,
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
                ":wallet_id": db.wallet_id(),
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

fn delete_attempt(db: &VotingDb, attempt_id: i64) -> Result<(), VotingError> {
    db.conn()
        .execute(
            "DELETE FROM chain_submission_attempts WHERE id=?1 AND wallet_id=?2",
            rusqlite::params![attempt_id, db.wallet_id()],
        )
        .map(|_| ())
        .map_err(internal("delete definitely-unsent chain attempt"))
}

fn known_hashes(
    db: &VotingDb,
    identity: &ChainSubmissionIdentity,
) -> Result<Vec<String>, VotingError> {
    let mut hashes = match identity.kind {
        ChainSubmissionKind::Delegation => db
            .get_delegation_tx_hash(&identity.round_id, identity.bundle_index)?
            .into_iter()
            .collect(),
        ChainSubmissionKind::Vote => db
            .get_vote_tx_hash(
                &identity.round_id,
                identity.bundle_index,
                identity.proposal_id.expect("vote identity has proposal"),
            )?
            .into_iter()
            .collect(),
        ChainSubmissionKind::VoteBatch => {
            let recoveries = {
                let conn = db.conn();
                vote::load_vote_batch_recoveries_with_conn(
                    &conn,
                    &db.wallet_id(),
                    &identity.round_id,
                    identity.bundle_index,
                    identity.batch_digest.expect("batch identity has digest"),
                )?
            };
            let mut hashes = Vec::new();
            for recovery in recoveries {
                if let Some(hash) = db.get_vote_tx_hash(
                    &identity.round_id,
                    identity.bundle_index,
                    recovery.proposal_id,
                )? {
                    if !hashes.contains(&hash) {
                        hashes.push(hash);
                    }
                }
            }
            hashes
        }
    };
    let conn = db.conn();
    let mut stmt = conn
        .prepare(
            "SELECT chain_tx_hash
               FROM chain_submission_attempts
              WHERE round_id=:round_id AND wallet_id=:wallet_id
                AND kind=:kind AND bundle_index=:bundle_index
                AND proposal_id=:proposal_id AND batch_digest=:batch_digest
                AND chain_tx_hash IS NOT NULL
              ORDER BY id",
        )
        .map_err(internal("prepare known chain hashes query"))?;
    let rows = stmt
        .query_map(
            named_params! {
                ":round_id": identity.round_id,
                ":wallet_id": db.wallet_id(),
                ":kind": identity.kind.as_str(),
                ":bundle_index": i64::from(identity.bundle_index),
                ":proposal_id": identity.proposal_key(),
                ":batch_digest": identity.batch_key(),
            },
            |row| row.get::<_, String>(0),
        )
        .map_err(internal("query known chain hashes"))?;
    for row in rows {
        let hash = row.map_err(internal("read known chain hash"))?;
        if !hashes.contains(&hash) {
            hashes.push(hash);
        }
    }
    Ok(hashes)
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

    #[derive(Default)]
    struct MockTransport {
        responses: Mutex<VecDeque<Result<ChainResponse, ChainTransportError>>>,
        posts: Mutex<usize>,
    }

    impl ChainTransport for MockTransport {
        fn get<'a>(&'a self, _url: &'a str, _timeout: Duration) -> ChainFuture<'a> {
            Box::pin(async move {
                self.responses
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("mock GET response")
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
            .submit_body_locked(identity, b"{}".to_vec(), &|| false)
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
            .submit_body_locked(identity.clone(), b"{}".to_vec(), &|| false)
            .await
            .unwrap();
        let outcome = lifecycle
            .submit_body_locked(identity, b"{}".to_vec(), &|| false)
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
            .submit_body_locked(identity.clone(), b"{}".to_vec(), &|| false)
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
}
