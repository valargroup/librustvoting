//! Durable vote-chain submission and confirmation lifecycle.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::{Arc, Mutex, OnceLock, Weak},
    time::{SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use rusqlite::{named_params, OptionalExtension, TransactionBehavior};
use sha2::{Digest, Sha256};

use crate::{
    chain::{
        is_spent_nullifier_log, ChainBroadcastOutcome, ChainClient, ChainError,
        ChainTxConfirmation, ChainTxStatus,
    },
    confirmation::{
        confirm_delegation_submission_for_wallet, confirm_vote_batch_submission_for_wallet,
        confirm_vote_submission_for_wallet, StoredHashConflict,
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
    /// This submission was confirmed by an earlier call, and its event-derived
    /// positions are already recorded in the voting database.
    ///
    /// Distinct from [`ChainLifecycleOutcome::Confirmed`], which carries the
    /// confirmation this call just parsed. The exact per-transaction VAN
    /// position is not recoverable afterwards — `bundles.van_leaf_position` is a
    /// single pointer that later confirmations on the same bundle advance — so
    /// this variant reports the settled fact without inventing event data.
    AlreadyConfirmed {
        tx_hash: String,
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
        let recovered = {
            let conn = self.db.conn();
            delegate::submission_with_conn(
                &conn,
                &wallet_id,
                round_id,
                bundle_index,
                DelegationSigner::signature(signature, sighash),
            )?
        };
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

    /// Cancellation's outcome for this call.
    ///
    /// Cancellation observed after a broadcast completes does not replace that
    /// broadcast's result, so a dispatched attempt that may still commit is
    /// reported as `OutcomeUnknown`. `Cancelled` is reserved for calls with no
    /// completed ambiguous broadcast.
    fn cancelled_outcome(
        &self,
        wallet_id: &str,
        identity: &ChainSubmissionIdentity,
        last_unknown: &Option<String>,
    ) -> Result<ChainLifecycleOutcome, VotingError> {
        if last_unknown.is_some() || has_live_attempt(self.db, wallet_id, identity)? {
            return Ok(ChainLifecycleOutcome::OutcomeUnknown {
                // Supplementary detail. The decision to report ambiguity is
                // already made, so failing to list the candidates must not undo
                // it; see `outstanding_ambiguity`.
                known_tx_hashes: known_hashes(self.db, wallet_id, identity).unwrap_or_default(),
                message: last_unknown.clone().unwrap_or_else(|| {
                    "an earlier attempt was dispatched without a usable response".to_string()
                }),
            });
        }
        Ok(ChainLifecycleOutcome::Cancelled)
    }

    /// The ambiguity that outranks a definite failure reported here, if any.
    ///
    /// An earlier attempt in this call whose outcome is unknown, a journaled
    /// attempt that may still commit, or a known candidate hash all mean the
    /// submission may yet land, so a failure definite only for this attempt must
    /// not be reported as the call's result.
    fn outstanding_ambiguity(
        &self,
        wallet_id: &str,
        identity: &ChainSubmissionIdentity,
        last_unknown: &Option<String>,
        cause: &str,
    ) -> Result<Option<ChainLifecycleOutcome>, VotingError> {
        // `last_unknown` is in-memory evidence of a broadcast this call already
        // completed, and it needs no storage read to be true. A database that has
        // become unreadable — which is one of the things that gets us here — must
        // not be able to erase it, so the reads below happen only when there is
        // nothing in memory to go on, and the candidate list is best effort once
        // the decision to report ambiguity is made.
        if last_unknown.is_none() {
            let known_tx_hashes = known_hashes(self.db, wallet_id, identity)?;
            if known_tx_hashes.is_empty() && !has_live_attempt(self.db, wallet_id, identity)? {
                return Ok(None);
            }
            return Ok(Some(ChainLifecycleOutcome::OutcomeUnknown {
                known_tx_hashes,
                message: format!(
                    "an earlier attempt may still commit; this one did not settle: {cause}"
                ),
            }));
        }
        let known_tx_hashes = known_hashes(self.db, wallet_id, identity).unwrap_or_default();
        let message = match last_unknown {
            Some(earlier) => format!(
                "an earlier attempt's outcome is unknown ({earlier}); a later attempt did not \
                 settle: {cause}"
            ),
            // Unreachable: handled above, where the storage reads are skipped.
            None => {
                format!("an earlier attempt may still commit; this one did not settle: {cause}")
            }
        };
        Ok(Some(ChainLifecycleOutcome::OutcomeUnknown {
            known_tx_hashes,
            message,
        }))
    }

    async fn submit_body_locked(
        &self,
        wallet_id: &str,
        identity: ChainSubmissionIdentity,
        body: Vec<u8>,
        rebuild: PayloadRebuild<'_>,
        cancel: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<ChainLifecycleOutcome, ChainLifecycleError> {
        // Every step inside the attempt loop can fail — a lookup, a journal
        // write, a reservation, the cleanup of an unsent one — and each such
        // failure is definite only for the step that raised it. Once a broadcast
        // in this call has completed ambiguously, or a candidate is known, or an
        // attempt is still live, none of them may be reported as the call's
        // result: the transaction may still commit, and a host reading a plain
        // error as a definite failure could replace a generation that is about
        // to be confirmed. Catching that here rather than at each `?` keeps the
        // rule from having to be rediscovered every time a new fallible step is
        // added.
        let mut last_unknown = None;
        match self
            .attempt_submission(
                wallet_id,
                &identity,
                body,
                rebuild,
                cancel,
                &mut last_unknown,
            )
            .await
        {
            Ok(outcome) => Ok(outcome),
            // The exception: this one already carries the accepted hash, which
            // is the only handle on a transaction in the mempool and strictly
            // more than the ambiguity it implies.
            Err(error @ ChainLifecycleError::AcceptedButUnjournaled { .. }) => Err(error),
            Err(error) => {
                match self.outstanding_ambiguity(
                    wallet_id,
                    &identity,
                    &last_unknown,
                    &error.to_string(),
                )? {
                    Some(outcome) => Ok(outcome),
                    None => Err(error),
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn attempt_submission(
        &self,
        wallet_id: &str,
        identity: &ChainSubmissionIdentity,
        body: Vec<u8>,
        rebuild: PayloadRebuild<'_>,
        cancel: &(dyn Fn() -> bool + Send + Sync),
        last_unknown: &mut Option<String>,
    ) -> Result<ChainLifecycleOutcome, ChainLifecycleError> {
        let existing = self.reconcile_locked(wallet_id, identity, cancel).await?;
        if blocks_further_dispatch(&existing) {
            return Ok(existing);
        }

        let digest: [u8; 32] = Sha256::digest(&body).into();
        let attempts = self.client.retry_delays().len() + 1;
        for attempt_index in 0..attempts {
            if cancel() {
                return Ok(self.cancelled_outcome(wallet_id, identity, last_unknown)?);
            }
            if attempt_index > 0 {
                // Another process, or a concurrent legacy recording call, can
                // record a candidate while this call is between attempts. Apply
                // the same rule as the preflight below: a known candidate that
                // may still commit stops this call from broadcasting again.
                // A terminal lookup error for someone else's candidate is not
                // evidence about this call's earlier attempt, which may still
                // commit. Replacing that ambiguity with the unrelated error
                // would let the host read it as a definite failure.
                let reconciled = match self.reconcile_locked(wallet_id, identity, cancel).await {
                    Ok(reconciled) => reconciled,
                    Err(error) => {
                        if let Some(outcome) = self.outstanding_ambiguity(
                            wallet_id,
                            identity,
                            last_unknown,
                            &error.to_string(),
                        )? {
                            return Ok(outcome);
                        }
                        return Err(error);
                    }
                };
                // A cancelled reconciliation settled nothing, and cancellation
                // observed after a broadcast completed must not replace its
                // result. `cancelled_outcome` reports the earlier ambiguity when
                // there is one and `Cancelled` only when there is not.
                if matches!(reconciled, ChainLifecycleOutcome::Cancelled) {
                    return Ok(self.cancelled_outcome(wallet_id, identity, last_unknown)?);
                }
                if blocks_further_dispatch(&reconciled) {
                    return Ok(reconciled);
                }
            }

            // Each attempt re-derives the payload inside its own reservation
            // transaction. That repeats the delegation signature verification
            // and recovery reads up to `attempts` times per call, which is the
            // intended cost of proving the generation is still current.
            // Registered before the reservation is journaled, not after it
            // commits. The row is durable the instant that transaction commits,
            // and coverage decided by age can already read it as stale — a clock
            // step is all it takes — so a guard taken afterwards leaves a window
            // where cleanup may erase the generation these bytes were built
            // from, while this call goes on to POST them and journal a hash that
            // can no longer be confirmed against what is stored. Registering
            // first can only name an identity whose reservation then fails,
            // which over-protects its rows until the guard drops.
            let _in_flight = InFlightAttempt::register(wallet_id, identity);
            // A failure here is definite only for this attempt. An earlier one
            // in this call may still commit, and a concurrent change to the
            // now-uncovered generation is exactly what makes this rebuild fail
            // as stale — so reporting the persistence error alone would hide the
            // ambiguity and invite the host to treat the replacement as safe to
            // submit.
            let attempt_id = match reserve_attempt(self.db, wallet_id, identity, &digest, rebuild) {
                Ok(attempt_id) => attempt_id,
                Err(error) => {
                    if let Some(outcome) = self.outstanding_ambiguity(
                        wallet_id,
                        identity,
                        last_unknown,
                        &error.to_string(),
                    )? {
                        return Ok(outcome);
                    }
                    return Err(error.into());
                }
            };
            if cancel() {
                // This reservation is definitely unsent, but an earlier attempt
                // in this call may still commit.
                delete_attempt(self.db, wallet_id, attempt_id)?;
                return Ok(self.cancelled_outcome(wallet_id, identity, last_unknown)?);
            }

            let result = {
                // While the POST is outstanding, refresh the reservation's
                // `updated_at`. Without this the column means "when this was
                // reserved", and the age test downstream would be reading it as
                // "the owner was alive at this time" — the two diverge exactly
                // when the wall clock steps forward, and a reader in another
                // process, which cannot see this process's in-memory registry,
                // has nothing else to go on.
                let post =
                    self.client
                        .post_once(attempt_index, identity.kind.endpoint(), body.clone());
                tokio::pin!(post);
                loop {
                    tokio::select! {
                        outcome = &mut post => break outcome,
                        _ = tokio::time::sleep(RESERVATION_HEARTBEAT) => {
                            touch_attempt(self.db, wallet_id, attempt_id);
                        }
                    }
                }
            };
            match result {
                Ok(ChainBroadcastOutcome::Accepted(result)) => {
                    // The hash must not go down with a storage failure: it is
                    // the only handle anything will ever have on a transaction
                    // that is already in the mempool.
                    if let Err(error) = mark_attempt(
                        self.db,
                        wallet_id,
                        attempt_id,
                        "accepted",
                        Some(&result.tx_hash),
                    ) {
                        return Err(ChainLifecycleError::AcceptedButUnjournaled {
                            tx_hash: result.tx_hash,
                            source: error,
                        });
                    }
                    return Ok(ChainLifecycleOutcome::Accepted {
                        tx_hash: result.tx_hash,
                    });
                }
                Ok(ChainBroadcastOutcome::Rejected(result)) => {
                    let tx_hash = (!result.tx_hash.is_empty()).then_some(result.tx_hash.as_str());
                    // Deliberately not fatal. If this cannot be recorded the
                    // reservation stays durably `attempting`, so the precedence
                    // checks below see a live attempt and report
                    // `OutcomeUnknown` — which is right, because an earlier
                    // attempt in this call may still commit and a persistence
                    // failure is no evidence about it either way.
                    let journal_error =
                        mark_attempt(self.db, wallet_id, attempt_id, "rejected", tx_hash).err();
                    if is_spent_nullifier_log(&result.log) {
                        let reconciled = self.reconcile_locked(wallet_id, identity, cancel).await?;
                        match reconciled {
                            // Either confirmed outcome is proof of success.
                            // Another process may have applied the confirmation
                            // between this call's preflight and this response.
                            outcome @ (ChainLifecycleOutcome::Confirmed { .. }
                            | ChainLifecycleOutcome::AlreadyConfirmed { .. }) => {
                                return Ok(outcome)
                            }
                            // `AlreadySpentUnresolved` asserts that the known
                            // candidates were checked and none had succeeded. A
                            // reconciliation that settled nothing checked
                            // nothing, so reporting it would state as fact
                            // something this call never established, and would
                            // bury the ambiguity underneath it.
                            ChainLifecycleOutcome::Cancelled => {
                                return Ok(self.cancelled_outcome(
                                    wallet_id,
                                    identity,
                                    last_unknown,
                                )?)
                            }
                            outcome @ ChainLifecycleOutcome::OutcomeUnknown { .. } => {
                                return Ok(outcome)
                            }
                            _ => {}
                        }
                        return Ok(ChainLifecycleOutcome::AlreadySpentUnresolved {
                            known_tx_hashes: known_hashes(self.db, wallet_id, identity)?,
                            log: result.log,
                        });
                    }
                    // A candidate hash another writer recorded — a concurrent
                    // operation's `accepted` attempt, or a legacy domain hash —
                    // may still commit, and `has_live_attempt` cannot see it:
                    // `accepted` is neither `attempting` nor `outcome_unknown`.
                    // Settle it on the network before reporting a rejection that
                    // is definite only for this attempt's own payload.
                    if !known_hashes(self.db, wallet_id, identity)?.is_empty() {
                        let reconciled = self.reconcile_locked(wallet_id, identity, cancel).await?;
                        // `Cancelled` is excluded deliberately: cancellation
                        // observed after this broadcast completed must not
                        // replace its result, and the checks below still have to
                        // classify it.
                        if !matches!(reconciled, ChainLifecycleOutcome::Cancelled)
                            && blocks_further_dispatch(&reconciled)
                        {
                            return Ok(reconciled);
                        }
                    }
                    // A rejection is definite only for the attempt that
                    // received it. If an earlier attempt in this call may still
                    // commit, reporting a terminal rejection would let the host
                    // conclude the vote cannot land when it still can.
                    // This attempt is already journaled as rejected, so any
                    // remaining live attempt is an earlier one — from this call
                    // or a previous one — that may still commit.
                    //
                    // A candidate that survived the reconciliation above is such
                    // an attempt too, and `has_live_attempt` cannot see it:
                    // `accepted` is neither `attempting` nor `outcome_unknown`.
                    // Re-read rather than reuse the set from before, because a
                    // committed failure there retires its candidates. This also
                    // covers a reconciliation that was cancelled and so reached
                    // no conclusion at all.
                    let known_tx_hashes = known_hashes(self.db, wallet_id, identity)?;
                    if last_unknown.is_some()
                        || !known_tx_hashes.is_empty()
                        || has_live_attempt(self.db, wallet_id, identity)?
                    {
                        let earlier = match (&*last_unknown, &journal_error) {
                            (Some(message), _) => message.clone(),
                            (None, Some(error)) => {
                                format!("this rejection could not be journaled: {error}")
                            }
                            (None, _) if !known_tx_hashes.is_empty() => {
                                "a known candidate may still commit".to_string()
                            }
                            (None, _) => "dispatched, no response".to_string(),
                        };
                        return Ok(ChainLifecycleOutcome::OutcomeUnknown {
                            known_tx_hashes,
                            message: format!(
                                "an earlier attempt's outcome is unknown ({earlier}); a later attempt was rejected with code {}: {}",
                                result.code, result.log
                            ),
                        });
                    }
                    return Ok(ChainLifecycleOutcome::Rejected {
                        code: result.code,
                        log: result.log,
                    });
                }
                Ok(ChainBroadcastOutcome::OutcomeUnknown { message }) => {
                    *last_unknown = Some(message.clone());
                    // The request may still commit, and the reservation is still
                    // durably `attempting`, so the ambiguity is real whether or
                    // not this classification lands. Returning the storage error
                    // alone would let the host read a completed ambiguous
                    // broadcast as a definite failure.
                    if let Err(error) =
                        mark_attempt(self.db, wallet_id, attempt_id, "outcome_unknown", None)
                    {
                        return Ok(ChainLifecycleOutcome::OutcomeUnknown {
                            known_tx_hashes: known_hashes(self.db, wallet_id, identity)
                                .unwrap_or_default(),
                            message: format!(
                                "{message}; classifying that attempt also failed to persist: {error}"
                            ),
                        });
                    }
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
                        // As above: the reservation stays durably `attempting`,
                        // so a failed classification must not turn a possibly
                        // committed request into a definite error.
                        if let Err(persist) =
                            mark_attempt(self.db, wallet_id, attempt_id, "outcome_unknown", None)
                        {
                            return Ok(ChainLifecycleOutcome::OutcomeUnknown {
                                known_tx_hashes: known_hashes(self.db, wallet_id, identity)
                                    .unwrap_or_default(),
                                message: format!(
                                    "{error}; classifying that attempt also failed to persist: {persist}"
                                ),
                            });
                        }
                    } else {
                        mark_attempt(self.db, wallet_id, attempt_id, "rejected", None)?;
                    }
                    if !error.is_retryable() || attempt_index + 1 == attempts {
                        // As above: a definite failure here cannot disprove an
                        // earlier attempt that may still commit, and a candidate
                        // another writer recorded is such an attempt even though
                        // `has_live_attempt` cannot see its `accepted` state.
                        if error.is_ambiguous()
                            || last_unknown.is_some()
                            || has_live_attempt(self.db, wallet_id, identity)?
                            || !known_hashes(self.db, wallet_id, identity)?.is_empty()
                        {
                            let message = match &*last_unknown {
                                Some(earlier) => format!(
                                    "an earlier attempt's outcome is unknown ({earlier}); a later attempt failed: {error}"
                                ),
                                None => error.to_string(),
                            };
                            return Ok(ChainLifecycleOutcome::OutcomeUnknown {
                                known_tx_hashes: known_hashes(self.db, wallet_id, identity)?,
                                message,
                            });
                        }
                        return Err(error.into());
                    }
                    // A retryable failure is not automatically an ambiguous one.
                    // A pre-dispatch transport failure had its reservation
                    // deleted above, and HTTP 429 is journaled as rejected;
                    // neither leaves a transaction that might commit.
                    if error.is_ambiguous() {
                        *last_unknown = Some(error.to_string());
                    }
                }
            }

            if attempt_index + 1 < attempts {
                if cancel() {
                    return Ok(self.cancelled_outcome(wallet_id, identity, last_unknown)?);
                }
                tokio::time::sleep(self.client.retry_delays()[attempt_index]).await;
            }
        }

        Ok(ChainLifecycleOutcome::OutcomeUnknown {
            known_tx_hashes: known_hashes(self.db, wallet_id, identity)?,
            message: last_unknown
                .clone()
                .unwrap_or_else(|| "submission outcome is unknown".to_string()),
        })
    }

    async fn reconcile_locked(
        &self,
        wallet_id: &str,
        identity: &ChainSubmissionIdentity,
        cancel: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<ChainLifecycleOutcome, ChainLifecycleError> {
        // Checked before the no-candidate fast path below, which would
        // otherwise report a stale operation as actively pending.
        if cancel() {
            return Ok(ChainLifecycleOutcome::Cancelled);
        }
        // A confirmation this lifecycle already applied is the strongest
        // evidence there is, and it is durable. Re-querying the network could
        // only weaken it: a lagging or pruned endpoint answering 404 would
        // report a completed submission as indefinitely uncommitted.
        if let Some(tx_hash) = durable_confirmation_hash(self.db, wallet_id, identity)? {
            return Ok(ChainLifecycleOutcome::AlreadyConfirmed { tx_hash });
        }
        let hashes = known_hashes(self.db, wallet_id, identity)?;
        if hashes.is_empty() {
            // A timeout or an unusable accepted response leaves an
            // `outcome_unknown` attempt with no hash, and an interruption leaves
            // an `attempting` one. Both mean a transaction may still commit, and
            // that evidence has to survive across calls: reporting `Pending`
            // would let a later call return a terminal rejection.
            if has_live_attempt(self.db, wallet_id, identity)? {
                return Ok(ChainLifecycleOutcome::OutcomeUnknown {
                    known_tx_hashes: hashes,
                    message: "an earlier attempt was dispatched without a usable response"
                        .to_string(),
                });
            }
            return Ok(ChainLifecycleOutcome::Pending {
                known_tx_hashes: hashes,
            });
        }
        let mut successful = Vec::new();
        let mut any_pending = false;
        let mut unresolved = None;
        let mut terminal_error = None;
        let mut committed_failures = Vec::new();
        for hash in &hashes {
            if cancel() {
                return Ok(ChainLifecycleOutcome::Cancelled);
            }
            // A committed response that parses may still describe some other
            // submission. Judging that inside the lookup keeps it part of
            // endpoint failover, so one endpoint answering about the wrong
            // transaction cannot end the search for a confirmation another
            // endpoint can still serve.
            let binds = |confirmation: &ChainTxConfirmation| {
                events_bind_to(self.db, wallet_id, identity, hash, confirmation)
            };
            let status = self
                .client
                .transaction_status_where(hash, cancel, &binds)
                .await;
            // Cancellation can arrive while a status request is in flight. Every
            // branch below either classifies the submission or retires failure
            // evidence, and a cancelled operation must do neither: the candidate
            // is still journaled, so the next reconciliation re-derives whatever
            // this one was about to conclude.
            if cancel() {
                return Ok(ChainLifecycleOutcome::Cancelled);
            }
            match status {
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
                // Retained rather than returned: an earlier candidate may
                // already have committed successfully, and discarding that
                // would leave a transaction that definitely landed unapplied
                // for as long as this per-hash error persists.
                Err(error) => {
                    terminal_error.get_or_insert(error);
                }
            }
        }
        if successful.len() > 1 {
            return Err(VotingError::Internal {
                message: "multiple chain candidates committed successfully for one submission"
                    .to_string(),
            }
            .into());
        }
        // Every candidate this lookup proved failed can never confirm, so none of
        // them may stay live evidence on any path out of here: leaving one
        // `accepted` makes later submissions rediscover it and exit before
        // dispatch, and keeps ballot-intent changes, recovery cleanup, and
        // bundle pruning pinned to a generation that can never confirm. Retiring
        // per exit missed the mixed-status ones, where another candidate's
        // unusable response or terminal error returns first.
        if !committed_failures.is_empty() {
            // Retirement is a durable mutation, so it is bracketed by the same
            // cancellation checks as any other write here. The second one covers
            // the loop's own time waiting on SQLite.
            if cancel() {
                return Ok(ChainLifecycleOutcome::Cancelled);
            }
            for (failed_hash, _) in &committed_failures {
                retire_failed_candidate(self.db, wallet_id, identity, failed_hash)?;
            }
            if cancel() {
                return Ok(ChainLifecycleOutcome::Cancelled);
            }
        }
        // Retirement above just marked the proven failures `rejected`, so the
        // set read before the lookup now names transactions the journal no
        // longer treats as candidates. Reporting those back would have the host
        // keep polling one this call proved had failed.
        let hashes = if committed_failures.is_empty() {
            hashes
        } else {
            let rebuilt = known_hashes(self.db, wallet_id, identity)?;
            // That read can wait on SQLite, so cancellation may have arrived
            // since the check above and every classification below is a claim
            // this operation is no longer entitled to make.
            if cancel() {
                return Ok(ChainLifecycleOutcome::Cancelled);
            }
            rebuilt
        };
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
        // The entry shortcut ran before these lookups. Another process can apply
        // the confirmation while they are in flight, and it outranks every
        // weaker answer below: a lagging 404 would otherwise report `Pending`,
        // and a different candidate's committed failure would report `Rejected`,
        // for a submission that is now durably confirmed.
        if let Some(tx_hash) = durable_confirmation_hash(self.db, wallet_id, identity)? {
            return Ok(ChainLifecycleOutcome::AlreadyConfirmed { tx_hash });
        }
        if let Some(error) = terminal_error {
            // A lookup that could not be completed is an absence of evidence,
            // not evidence of absence, so it must not outrank a dispatched
            // request that may still commit. `reconcile` reaches this exit
            // directly, without the submission loop's ambiguity handling around
            // it, so the check belongs here.
            if has_live_attempt(self.db, wallet_id, identity)? {
                return Ok(ChainLifecycleOutcome::OutcomeUnknown {
                    known_tx_hashes: hashes,
                    message: format!(
                        "an earlier attempt was dispatched without a usable response; \
                         a candidate lookup also failed: {error}"
                    ),
                });
            }
            return Err(error.into());
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
        if !committed_failures.is_empty() {
            // These rejections are definite only for the candidates they name.
            // A hashless attempt that may still commit outranks them, and so
            // does a candidate another writer journaled while this lookup was
            // running: retirement above marked the proven failures `rejected`,
            // so anything `known_hashes` still returns is a transaction nobody
            // has disproved. `has_live_attempt` cannot see it, because a
            // candidate arrives `accepted`.
            if !hashes.is_empty() || has_live_attempt(self.db, wallet_id, identity)? {
                return Ok(ChainLifecycleOutcome::OutcomeUnknown {
                    known_tx_hashes: hashes,
                    message: "another candidate for this submission may still commit".to_string(),
                });
            }
            let (_, failure) = committed_failures.remove(0);
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

/// The transaction hash of a confirmation this lifecycle already applied.
///
/// Reads only durable domain state: a hash plus the event-derived tree position
/// that the confirmation transaction commits alongside it. A partial row means
/// confirmation has not happened, so this returns `None` and normal
/// reconciliation continues.
///
/// Deliberately returns only the hash. `bundles.van_leaf_position` is a single
/// mutable pointer that a later vote or batch on the same bundle advances, so an
/// earlier transaction's own position is not recoverable from storage and must
/// not be synthesized from it.
fn durable_confirmation_hash(
    db: &VotingDb,
    wallet_id: &str,
    identity: &ChainSubmissionIdentity,
) -> Result<Option<String>, VotingError> {
    let conn = db.conn();
    let bundle: Option<(Option<String>, Option<i64>)> = conn
        .query_row(
            "SELECT delegation_tx_hash, van_leaf_position FROM bundles
              WHERE round_id=?1 AND wallet_id=?2 AND bundle_index=?3",
            rusqlite::params![
                identity.round_id,
                wallet_id,
                i64::from(identity.bundle_index)
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(internal("load durable confirmation bundle fields"))?;
    let Some((delegation_tx_hash, van_leaf_position)) = bundle else {
        return Ok(None);
    };
    if van_leaf_position.is_none() {
        return Ok(None);
    }
    match identity.kind {
        ChainSubmissionKind::Delegation => Ok(delegation_tx_hash),
        ChainSubmissionKind::Vote => {
            let proposal_id = identity.require_proposal_id()?;
            let Some(state) = queries::load_vote_row_state(
                &conn,
                &identity.round_id,
                wallet_id,
                identity.bundle_index,
                proposal_id,
            )?
            else {
                return Ok(None);
            };
            // A confirmed batch member carries both fields too, so this would
            // otherwise report a `cast-vote-batch` transaction as confirmation
            // of a singleton submission — which the singleton dispatch path
            // refuses to create in the first place. A row whose recovery cannot
            // be read keeps the legacy behaviour: there is nothing to say it is
            // a member.
            let is_batch_member = state
                .commitment_bundle_json
                .as_deref()
                .and_then(|json| vote::parse_recovery(json).ok())
                .is_some_and(|recovery| recovery.batch.is_some());
            if is_batch_member {
                return Ok(None);
            }
            Ok(match (state.tx_hash, state.vc_tree_position) {
                (Some(tx_hash), Some(_)) => Some(tx_hash),
                _ => None,
            })
        }
        ChainSubmissionKind::VoteBatch => {
            let batch_digest = identity.require_batch_digest()?;
            let recoveries = match vote::load_vote_batch_recoveries_with_conn(
                &conn,
                wallet_id,
                &identity.round_id,
                identity.bundle_index,
                batch_digest,
            ) {
                Ok(recoveries) => recoveries,
                // Without the member rows there is nothing to report as
                // confirmed; ordinary reconciliation takes over.
                Err(VotingError::InvalidInput { .. }) => return Ok(None),
                Err(error) => return Err(error),
            };
            let mut tx_hash: Option<String> = None;
            for recovery in &recoveries {
                let Some(state) = queries::load_vote_row_state(
                    &conn,
                    &identity.round_id,
                    wallet_id,
                    identity.bundle_index,
                    recovery.proposal_id,
                )?
                else {
                    return Ok(None);
                };
                let (Some(member_hash), Some(_)) = (state.tx_hash, state.vc_tree_position) else {
                    return Ok(None);
                };
                // A batch advances together or not at all, so members that
                // disagree are not a confirmed batch.
                if tx_hash.get_or_insert_with(|| member_hash.clone()) != &member_hash {
                    return Ok(None);
                }
            }
            Ok(tx_hash)
        }
    }
}

fn apply_confirmation(
    db: &VotingDb,
    wallet_id: &str,
    identity: &ChainSubmissionIdentity,
    tx_hash: &str,
    response: &ChainTxConfirmation,
) -> Result<ChainConfirmation, VotingError> {
    // The captured wallet is passed into the confirmation transaction rather
    // than checked here and re-read there: an account switch between the check
    // and the write would otherwise scope the confirmation to the new wallet.
    match identity.kind {
        ChainSubmissionKind::Delegation => confirm_delegation_submission_for_wallet(
            db,
            wallet_id,
            &identity.round_id,
            identity.bundle_index,
            tx_hash,
            &response.events,
            StoredHashConflict::ClearUnconfirmed,
        )
        .map(ChainConfirmation::Delegation),
        ChainSubmissionKind::Vote => confirm_vote_submission_for_wallet(
            db,
            wallet_id,
            &identity.round_id,
            identity.bundle_index,
            identity.require_proposal_id()?,
            tx_hash,
            &response.events,
            StoredHashConflict::ClearUnconfirmed,
        )
        .map(ChainConfirmation::Vote),
        ChainSubmissionKind::VoteBatch => confirm_vote_batch_submission_for_wallet(
            db,
            wallet_id,
            &identity.round_id,
            identity.bundle_index,
            &identity.require_batch_digest()?,
            tx_hash,
            &response.events,
            StoredHashConflict::ClearUnconfirmed,
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

/// How often an outstanding reservation refreshes its `updated_at`.
///
/// Far below [`INTERRUPTED_RESERVATION_GRACE_SECS`], so a reservation whose
/// owner is alive stays comfortably inside the grace period however the wall
/// clock moves, and far above the cost of the write, which most calls never
/// perform at all: the default request deadline is ten seconds.
const RESERVATION_HEARTBEAT: std::time::Duration = std::time::Duration::from_secs(30);

/// Marks an outstanding reservation as still owned, best effort.
///
/// Deliberately infallible, and deliberately non-blocking. This is a liveness
/// hint for readers that cannot see this process's in-memory registry; failing a
/// submission because the hint could not be written would trade a real
/// transaction for a bookkeeping error. Waiting for the connection would be
/// worse still: it runs inside the same task as the POST, so blocking on the
/// database mutex stops that task being polled, and with it the request
/// deadline this heartbeat exists to keep the reservation inside. A skipped
/// refresh costs the reservation its cross-process coverage only once the grace
/// period elapses, and only if the clock has moved that far.
fn touch_attempt(db: &VotingDb, wallet_id: &str, attempt_id: i64) {
    let Ok(now) = now_seconds() else {
        return;
    };
    let Some(conn) = db.try_conn() else {
        return;
    };
    let _ = conn.execute(
        "UPDATE chain_submission_attempts SET updated_at=:now
          WHERE id=:id AND wallet_id=:wallet_id AND state='attempting'",
        named_params! { ":now": now, ":id": attempt_id, ":wallet_id": wallet_id },
    );
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

/// Retires every record of a candidate whose transaction failed at commit.
///
/// Such a transaction can never confirm, so it must stop being live evidence:
/// otherwise later submissions rediscover it and exit before dispatch, and
/// ballot-intent changes and recovery cleanup stay pinned to a generation that
/// failed.
///
/// Both reconciliation sources are retired together. The attempt journal is one;
/// the other is the legacy domain column, which a pre-lifecycle host may have
/// written and which `known_hashes` still reads. Clearing the domain hash is
/// scoped to an exact match on a row with no recorded confirmation position, so
/// it can only ever remove a hash this reconciliation just proved failed, never
/// a confirmed one.
fn retire_failed_candidate(
    db: &VotingDb,
    wallet_id: &str,
    identity: &ChainSubmissionIdentity,
    tx_hash: &str,
) -> Result<(), VotingError> {
    let now = now_seconds()?;
    let mut conn = db.conn();
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(internal("begin failed candidate retirement"))?;
    tx.execute(
        "UPDATE chain_submission_attempts
            SET state='rejected', updated_at=:now
          WHERE round_id=:round_id AND wallet_id=:wallet_id
            AND kind=:kind AND bundle_index=:bundle_index
            AND proposal_id=:proposal_id AND batch_digest=:batch_digest
            AND chain_tx_hash=:tx_hash AND state<>'rejected'",
        named_params! {
            ":now": now,
            ":round_id": identity.round_id,
            ":wallet_id": wallet_id,
            ":kind": identity.kind.as_str(),
            ":bundle_index": i64::from(identity.bundle_index),
            ":proposal_id": identity.proposal_key(),
            ":batch_digest": identity.batch_key(),
            ":tx_hash": tx_hash,
        },
    )
    .map_err(internal("retire committed-failure chain attempt"))?;
    match identity.kind {
        ChainSubmissionKind::Delegation => {
            tx.execute(
                "UPDATE bundles SET delegation_tx_hash=NULL
                  WHERE round_id=:round_id AND wallet_id=:wallet_id
                    AND bundle_index=:bundle_index
                    AND delegation_tx_hash=:tx_hash
                    AND van_leaf_position IS NULL",
                named_params! {
                    ":round_id": identity.round_id,
                    ":wallet_id": wallet_id,
                    ":bundle_index": i64::from(identity.bundle_index),
                    ":tx_hash": tx_hash,
                },
            )
            .map_err(internal("clear failed delegation domain hash"))?;
        }
        // A batch records the same hash on every member, so the bundle-wide
        // match below is the batch's own rows; an exact hash match cannot reach
        // an unrelated submission.
        ChainSubmissionKind::Vote | ChainSubmissionKind::VoteBatch => {
            tx.execute(
                // `proposal_key` is the proposal for a singleton and the -1
                // sentinel for a batch, which widens the match to the batch's
                // own member rows.
                "UPDATE votes SET tx_hash=NULL
                  WHERE round_id=:round_id AND wallet_id=:wallet_id
                    AND bundle_index=:bundle_index
                    AND (:proposal_id = -1 OR proposal_id = :proposal_id)
                    AND tx_hash=:tx_hash
                    AND vc_tree_position IS NULL",
                named_params! {
                    ":round_id": identity.round_id,
                    ":wallet_id": wallet_id,
                    ":bundle_index": i64::from(identity.bundle_index),
                    ":proposal_id": identity.proposal_key(),
                    ":tx_hash": tx_hash,
                },
            )
            .map_err(internal("clear failed vote domain hash"))?;
        }
    }
    tx.commit()
        .map_err(internal("commit failed candidate retirement"))
}

/// Whether a committed transaction's events describe this submission.
///
/// Only successes are judged. A nonzero code is definite evidence about the
/// transaction whatever its events say, and a committed failure carries none of
/// the bindings this checks, so rejecting one would turn proven failure into an
/// unusable response and keep the candidate blocking a replacement forever.
///
/// Everything about a confirmation that can be judged without mutating anything
/// participates here, including the batch bindings that need persisted recovery:
/// leaving those until the confirmation transaction would let the first endpoint
/// to answer with the wrong proposals or nullifiers end the search, and stable
/// endpoint ordering would repeat that while another endpoint could serve the
/// real confirmation. What stays behind is only what the write itself decides.
fn events_bind_to(
    db: &VotingDb,
    wallet_id: &str,
    identity: &ChainSubmissionIdentity,
    tx_hash: &str,
    confirmation: &ChainTxConfirmation,
) -> bool {
    if confirmation.code != 0 {
        return true;
    }
    match identity.kind {
        ChainSubmissionKind::Delegation => crate::confirmation::delegation_events_bind(
            tx_hash,
            &identity.round_id,
            &confirmation.events,
        ),
        ChainSubmissionKind::Vote => {
            crate::confirmation::vote_events_bind(tx_hash, &identity.round_id, &confirmation.events)
        }
        ChainSubmissionKind::VoteBatch => match identity.batch_digest {
            Some(digest) => crate::confirmation::vote_batch_binds_to_recovery(
                &db.conn(),
                wallet_id,
                &identity.round_id,
                identity.bundle_index,
                digest,
                tx_hash,
                &confirmation.events,
            ),
            None => false,
        },
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

/// The attempts whose transaction this SDK could still identify.
///
/// Coverage exists to protect confirmation, and confirmation needs a chain
/// transaction hash: [`known_hashes`] is the only way an attempt ever reaches
/// [`ChainClient::transaction_status`], and this SDK deliberately cannot predict
/// a hash or locate a transaction from its commitment. So an attempt that has no
/// hash and can no longer be given one protects nothing — while freezing the
/// recovery generation, ballot intent, and bundle pruning of its row forever,
/// because nothing ever retires a hashless attempt.
///
/// `attempting` is the one hashless state that may still learn a hash: a POST
/// that has not yet been classified can still return one, and the guarded rows
/// are exactly what that response would be applied to. That is a claim with an
/// expiry, not a durable fact, so it is checked against
/// [`INTERRUPTED_RESERVATION_GRACE_SECS`] on every query rather than rewritten
/// once. A reservation an interrupted process left behind therefore stops
/// covering as soon as the grace period elapses, wherever the guard is evaluated
/// from and whether or not anything reopens the database — a durable downgrade
/// performed at open would freeze the row until the next open instead, which for
/// a crash-and-restart inside the grace period is the whole life of the round.
///
/// Dropping coverage for a hashless `outcome_unknown` attempt cannot produce the
/// mismatch the guards exist to prevent: attaching a transaction's hash and
/// event positions to a generation it did not witness needs that hash, and there
/// is none. The transaction may still have committed; that is reported as
/// ambiguity by [`has_live_attempt`], which is unchanged, and as
/// `AlreadySpentUnresolved` if a replacement is later refused on chain.
pub(crate) fn can_still_learn_a_hash() -> Result<String, VotingError> {
    Ok(format!(
        "(chain_tx_hash IS NOT NULL OR (state='attempting' AND updated_at >= {}))",
        fresh_attempt_cutoff()?
    ))
}

/// The `:fresh_cutoff` bound [`can_still_learn_a_hash`] expects, as of now.
///
/// A reservation touched at or after this instant may still be in flight; an
/// older one cannot be, because no configurable request deadline reaches back
/// this far. Every caller of the predicate must bind it.
///
/// This reads the wall clock, which can step. A forward jump larger than the
/// grace period would age out a POST that is genuinely in flight, which is why
/// it is the weaker of the two tests and never the only one: a reservation this
/// process is waiting on is named by [`in_flight_attempt_ids`] and stays covered
/// whatever the clock does.
pub(crate) fn fresh_attempt_cutoff() -> Result<i64, VotingError> {
    Ok(now_seconds()?.saturating_sub(INTERRUPTED_RESERVATION_GRACE_SECS))
}

/// One submission this process has an outstanding POST for.
///
/// Keyed by the durable identity rather than by the journal row's id. Row ids
/// restart per database file, so two handles on different files mint the same
/// id: an id-keyed registry would report one database's expired reservation as
/// live because another currently owns that number, and releasing either guard
/// would uncover the other. The identity is the thing coverage is actually
/// about, and it means the same thing in every database.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct InFlightKey {
    wallet_id: String,
    round_id: String,
    kind: &'static str,
    bundle_index: u32,
    proposal_key: i64,
    batch_digest: Vec<u8>,
}

/// Outstanding reservations, counted so overlapping registrations of one
/// identity cannot uncover each other when the first of them is released.
static IN_FLIGHT_ATTEMPTS: OnceLock<Mutex<BTreeMap<InFlightKey, usize>>> = OnceLock::new();

fn in_flight_registry() -> &'static Mutex<BTreeMap<InFlightKey, usize>> {
    IN_FLIGHT_ATTEMPTS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Submissions this process is waiting on a response for in one round, right
/// now.
///
/// Exact and clock-free, which is what keeps a live POST covered across a
/// wall-clock adjustment. The age test in [`CAN_STILL_LEARN_A_HASH`] exists for
/// the reservations this registry cannot know about: another process's, and the
/// crashed ones no registry will ever hold.
fn in_flight_for_round(round_id: &str, wallet_id: &str) -> Vec<InFlightKey> {
    let Ok(live) = in_flight_registry().lock() else {
        return Vec::new();
    };
    live.keys()
        .filter(|key| key.round_id == round_id && key.wallet_id == wallet_id)
        .cloned()
        .collect()
}

/// Whether this process has an outstanding POST for a bundle in the pruned
/// range.
pub(crate) fn has_in_flight_at_or_after(round_id: &str, wallet_id: &str, from_index: u32) -> bool {
    in_flight_for_round(round_id, wallet_id)
        .iter()
        .any(|key| key.bundle_index >= from_index)
}

/// Keeps one reservation registered as in flight for as long as it is held.
///
/// Releasing on drop rather than at each exit means an early return, an error,
/// or a panic between the POST and its classification cannot leave an entry
/// pinning coverage for the life of the process.
struct InFlightAttempt(InFlightKey);

impl InFlightAttempt {
    fn register(wallet_id: &str, identity: &ChainSubmissionIdentity) -> Self {
        let key = InFlightKey {
            wallet_id: wallet_id.to_string(),
            round_id: identity.round_id.clone(),
            kind: identity.kind.as_str(),
            bundle_index: identity.bundle_index,
            proposal_key: identity.proposal_key(),
            batch_digest: identity.batch_key().to_vec(),
        };
        if let Ok(mut live) = in_flight_registry().lock() {
            *live.entry(key.clone()).or_insert(0) += 1;
        }
        Self(key)
    }
}

impl Drop for InFlightAttempt {
    fn drop(&mut self) {
        if let Ok(mut live) = in_flight_registry().lock() {
            if let Some(count) = live.get_mut(&self.0) {
                *count -= 1;
                if *count == 0 {
                    live.remove(&self.0);
                }
            }
        }
    }
}

/// How long a reservation may go untouched before no process can still be
/// waiting on it.
///
/// A row is `attempting` only between its reservation and the response
/// classification that follows its POST, so it is bounded by that call's request
/// deadline. Deriving this from [`MAX_REQUEST_TIMEOUT`] rather than picking a
/// number keeps the two from drifting apart: a host cannot configure a deadline
/// that outlives the grace period and make a live reservation look abandoned.
/// The doubling leaves room for the database work and scheduling either side of
/// the request itself.
///
/// It bounds the freeze a crashed reservation causes by minutes rather than by
/// the life of the round.
const INTERRUPTED_RESERVATION_GRACE_SECS: i64 =
    2 * crate::chain::MAX_REQUEST_TIMEOUT.as_secs() as i64;

#[cfg(test)]
pub(crate) fn interrupted_reservation_grace_secs() -> i64 {
    INTERRUPTED_RESERVATION_GRACE_SECS
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
/// So are attempts that can no longer learn a chain transaction hash; see
/// [`can_still_learn_a_hash`].
///
/// Rows whose recovery JSON cannot be parsed are covered conservatively when
/// their bundle has any batch attempt: an unreadable row may still be a member,
/// and erasing a member of an in-flight batch is the failure this prevents.
pub(crate) fn attempt_protected_vote_rows(
    conn: &rusqlite::Connection,
    round_id: &str,
    wallet_id: &str,
) -> Result<BTreeSet<(u32, u32)>, VotingError> {
    attempt_vote_rows(conn, round_id, wallet_id, &can_still_learn_a_hash()?)
}

/// The journaled chain transaction for each vote row that has one.
///
/// The same evidence that decides a row has a pending candidate also yields the
/// hash to poll, so restart planning cannot report one and then fail to find the
/// other. Later attempts win, matching the order `known_hashes` returns them in.
pub(crate) fn vote_candidate_hashes(
    conn: &rusqlite::Connection,
    round_id: &str,
    wallet_id: &str,
) -> Result<BTreeMap<(u32, u32), Vec<String>>, VotingError> {
    let mut hashes: BTreeMap<(u32, u32), Vec<String>> = BTreeMap::new();
    let mut batch_hashes: BTreeMap<(u32, [u8; 32]), Vec<String>> = BTreeMap::new();
    {
        let mut stmt = conn
            .prepare(
                "SELECT kind, bundle_index, proposal_id, batch_digest, chain_tx_hash
                   FROM chain_submission_attempts
                  WHERE round_id=:round_id AND wallet_id=:wallet_id
                    AND state<>'rejected' AND kind IN ('vote','vote_batch')
                    AND chain_tx_hash IS NOT NULL
                  ORDER BY id",
            )
            .map_err(internal("prepare vote candidate hash query"))?;
        let rows = stmt
            .query_map(
                named_params! { ":round_id": round_id, ":wallet_id": wallet_id },
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Vec<u8>>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                },
            )
            .map_err(internal("query vote candidate hashes"))?;
        for row in rows {
            let (kind, bundle_index, proposal_id, digest, hash) =
                row.map_err(internal("read vote candidate hash row"))?;
            let Ok(bundle_index) = u32::try_from(bundle_index) else {
                continue;
            };
            // Every non-rejected attempt is a candidate. Concurrent processes
            // can each be accepted with a different hash for one submission, and
            // keeping only the newest would have a host poll the one it kept
            // while the one it dropped commits.
            if kind == ChainSubmissionKind::Vote.as_str() {
                if let Ok(proposal_id) = u32::try_from(proposal_id) {
                    push_unique(hashes.entry((bundle_index, proposal_id)).or_default(), hash);
                }
            } else if let Ok(digest) = <[u8; 32]>::try_from(digest.as_slice()) {
                push_unique(
                    batch_hashes.entry((bundle_index, digest)).or_default(),
                    hash,
                );
            }
        }
    }
    if batch_hashes.is_empty() {
        return Ok(hashes);
    }
    let mut stmt = conn
        .prepare(
            "SELECT bundle_index, proposal_id, commitment_bundle_json
               FROM votes
              WHERE round_id=:round_id AND wallet_id=:wallet_id
                AND commitment_bundle_json IS NOT NULL",
        )
        .map_err(internal("prepare batch candidate membership query"))?;
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
        .map_err(internal("query batch candidate membership"))?;
    for row in rows {
        let (bundle_index, proposal_id, json) =
            row.map_err(internal("read batch candidate membership row"))?;
        let (Ok(bundle_index), Ok(proposal_id)) =
            (u32::try_from(bundle_index), u32::try_from(proposal_id))
        else {
            continue;
        };
        let Ok(recovery) = vote::parse_recovery(&json) else {
            continue;
        };
        let Some(batch) = recovery.batch.as_ref() else {
            continue;
        };
        if let Some(batch_candidates) = batch_hashes.get(&(bundle_index, batch.digest)) {
            let row = hashes.entry((bundle_index, proposal_id)).or_default();
            for hash in batch_candidates {
                push_unique(row, hash.clone());
            }
        }
    }
    Ok(hashes)
}

fn push_unique(into: &mut Vec<String>, hash: String) {
    if !into.contains(&hash) {
        into.push(hash);
    }
}

/// Every chain transaction that could identify one vote row.
///
/// The vote counterpart of [`delegation_candidates`], and the same rule: a
/// caller shown one of two live candidates can wait on a transaction that never
/// commits while the other does, which here also stalls the helper-share
/// delivery that follows confirmation.
pub(crate) fn vote_candidates(
    conn: &rusqlite::Connection,
    round_id: &str,
    wallet_id: &str,
    bundle_index: u32,
    proposal_id: u32,
) -> Result<Vec<String>, VotingError> {
    let mut candidates = Vec::new();
    if let Some(hash) =
        queries::get_vote_tx_hash(conn, round_id, wallet_id, bundle_index, proposal_id)?
    {
        candidates.push(hash);
    }
    if let Some(journaled) =
        vote_candidate_hashes(conn, round_id, wallet_id)?.remove(&(bundle_index, proposal_id))
    {
        candidates.extend(journaled);
    }
    let mut hashes: Vec<String> = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        let Ok(canonical) = crate::chain::normalize_tx_hash(&candidate) else {
            continue;
        };
        if !hashes.contains(&canonical) {
            hashes.push(canonical);
        }
    }
    Ok(hashes)
}

/// Every chain transaction that could identify one bundle's delegation.
///
/// The union `known_hashes` reconciles: the legacy domain column and the attempt
/// journal, canonicalized and deduplicated. A legacy writer recording one hash
/// while a lifecycle POST journals another leaves two live candidates, and a
/// caller shown only one of them can wait on a transaction that never commits
/// while the other does.
pub(crate) fn delegation_candidates(
    conn: &rusqlite::Connection,
    round_id: &str,
    wallet_id: &str,
    bundle_index: u32,
) -> Result<Vec<String>, VotingError> {
    let mut candidates = Vec::new();
    if let Some(hash) = queries::get_delegation_tx_hash(conn, round_id, wallet_id, bundle_index)? {
        candidates.push(hash);
    }
    if let Some(journaled) =
        delegation_candidate_hashes(conn, round_id, wallet_id)?.remove(&bundle_index)
    {
        candidates.extend(journaled);
    }
    let mut hashes: Vec<String> = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        let Ok(canonical) = crate::chain::normalize_tx_hash(&candidate) else {
            continue;
        };
        if !hashes.contains(&canonical) {
            hashes.push(canonical);
        }
    }
    Ok(hashes)
}

/// The journaled chain transaction for each bundle's delegation, where there is
/// one. Paired with [`vote_candidate_hashes`]; see its note on sourcing.
pub(crate) fn delegation_candidate_hashes(
    conn: &rusqlite::Connection,
    round_id: &str,
    wallet_id: &str,
) -> Result<BTreeMap<u32, Vec<String>>, VotingError> {
    let mut stmt = conn
        .prepare(
            "SELECT bundle_index, chain_tx_hash FROM chain_submission_attempts
              WHERE round_id=:round_id AND wallet_id=:wallet_id
                AND kind='delegation' AND state<>'rejected'
                AND chain_tx_hash IS NOT NULL
              ORDER BY id",
        )
        .map_err(internal("prepare delegation candidate hash query"))?;
    let rows = stmt
        .query_map(
            named_params! { ":round_id": round_id, ":wallet_id": wallet_id },
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .map_err(internal("query delegation candidate hashes"))?;
    let mut hashes: BTreeMap<u32, Vec<String>> = BTreeMap::new();
    for row in rows {
        let (bundle_index, hash) = row.map_err(internal("read delegation candidate hash"))?;
        if let Ok(bundle_index) = u32::try_from(bundle_index) {
            // As for votes: every non-rejected attempt is a candidate.
            push_unique(hashes.entry(bundle_index).or_default(), hash);
        }
    }
    Ok(hashes)
}

fn attempt_vote_rows(
    conn: &rusqlite::Connection,
    round_id: &str,
    wallet_id: &str,
    attempt_filter: &str,
) -> Result<BTreeSet<(u32, u32)>, VotingError> {
    let mut protected = BTreeSet::new();
    let mut batch_digests: BTreeMap<u32, BTreeSet<[u8; 32]>> = BTreeMap::new();
    {
        let mut stmt = conn
            .prepare(&format!(
                "SELECT kind, bundle_index, proposal_id, batch_digest
                       FROM chain_submission_attempts
                      WHERE round_id=:round_id AND wallet_id=:wallet_id
                        AND state<>'rejected' AND kind IN ('vote','vote_batch')
                        AND {attempt_filter}"
            ))
            .map_err(internal("prepare attempted vote coverage query"))?;
        let rows = stmt
            .query_map(
                // The freshness bound is interpolated rather than bound, so a
                // filter that does not need it is not forced to name it.
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
    // A reservation this process is waiting on is exact and clock-free, so it
    // covers whatever the age test above made of its row.
    for key in in_flight_for_round(round_id, wallet_id) {
        if key.kind == ChainSubmissionKind::Vote.as_str() {
            if let Ok(proposal_id) = u32::try_from(key.proposal_key) {
                protected.insert((key.bundle_index, proposal_id));
            }
        } else if key.kind == ChainSubmissionKind::VoteBatch.as_str() {
            if let Ok(digest) = <[u8; 32]>::try_from(key.batch_digest.as_slice()) {
                batch_digests
                    .entry(key.bundle_index)
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

/// Whether an outcome means this call must not broadcast again.
///
/// A settled result speaks for itself, and a known candidate that is pending or
/// unresolved may still commit. Used by both the preflight and the between-retry
/// reconciliation so the two cannot drift apart.
fn blocks_further_dispatch(outcome: &ChainLifecycleOutcome) -> bool {
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

/// Whether a dispatched attempt may still commit.
///
/// `attempting` and `outcome_unknown` are exactly the states that mean "a
/// request may have reached the chain without producing a usable response",
/// including the hashless case a timeout or an interruption leaves behind.
fn has_live_attempt(
    db: &VotingDb,
    wallet_id: &str,
    identity: &ChainSubmissionIdentity,
) -> Result<bool, VotingError> {
    db.conn()
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM chain_submission_attempts
                  WHERE round_id=:round_id AND wallet_id=:wallet_id
                    AND kind=:kind AND bundle_index=:bundle_index
                    AND proposal_id=:proposal_id AND batch_digest=:batch_digest
                    AND state IN ('attempting','outcome_unknown')
             )",
            named_params! {
                ":round_id": identity.round_id,
                ":wallet_id": wallet_id,
                ":kind": identity.kind.as_str(),
                ":bundle_index": i64::from(identity.bundle_index),
                ":proposal_id": identity.proposal_key(),
                ":batch_digest": identity.batch_key(),
            },
            |row| row.get(0),
        )
        .map_err(internal("query live chain attempts"))
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

/// Returns the process-wide lock serializing operations for one identity.
///
/// The registry holds weak references and the caller holds the only strong one
/// for the duration of its operation. Two concurrent operations on the same
/// identity still share one mutex, because the second upgrades the entry while
/// the first is holding it; once no operation is left, the entry becomes
/// reclaimable. A long-lived wallet moves through many rounds and proposals, so
/// keeping a strong reference per identity forever would grow without bound.
fn operation_lock(key: &str) -> Result<OperationLock, VotingError> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    let mut locks = LOCKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .map_err(|error| VotingError::Internal {
            message: format!("chain operation lock registry poisoned: {error}"),
        })?;
    if let Some(live) = locks.get(key).and_then(Weak::upgrade) {
        return Ok(live);
    }
    locks.retain(|_, entry| entry.strong_count() > 0);
    let lock: OperationLock = Arc::new(tokio::sync::Mutex::new(()));
    locks.insert(key.to_string(), Arc::downgrade(&lock));
    Ok(lock)
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Mutex, time::Duration};

    use super::*;
    use crate::{
        chain::{
            transport::{ChainFuture, ChainResponse, ChainTransport, ChainTransportError},
            ChainClientConfig, ChainEndpointSet,
        },
        round::RoundParams,
        Network,
    };

    const ROUND_ID: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const TX_HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const WALLET: &str = "wallet-1";
    const TX_HASH_2: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

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

    fn file_test_db(path: &str) -> VotingDb {
        let db = VotingDb::open(path).unwrap();
        init_test_db(db)
    }

    fn test_db() -> VotingDb {
        init_test_db(VotingDb::open_in_memory().unwrap())
    }

    fn init_test_db(db: VotingDb) -> VotingDb {
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

    /// Stores a vote row whose columns agree with the recovery it carries, the
    /// way the vote lifecycle writes them.
    fn store_vote_with_recovery(
        db: &VotingDb,
        proposal_id: u32,
        recovery: &crate::vote::VoteRecoveryBundle,
    ) {
        let commitment = crate::vote::stored_vote_commitment_bytes(recovery).unwrap();
        queries::store_vote(
            &db.conn(),
            ROUND_ID,
            WALLET,
            0,
            proposal_id,
            recovery.vote_decision,
            &commitment,
        )
        .unwrap();
        db.conn()
            .execute(
                "UPDATE votes SET commitment_bundle_json=?1
                  WHERE round_id=?2 AND wallet_id=?3 AND bundle_index=0 AND proposal_id=?4",
                rusqlite::params![
                    crate::vote::serialize_recovery(recovery).unwrap(),
                    ROUND_ID,
                    WALLET,
                    proposal_id as i64
                ],
            )
            .unwrap();
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

    fn journal_vote_attempt(db: &VotingDb, state: &str, tx_hash: Option<&str>) {
        db.conn()
            .execute(
                "INSERT INTO chain_submission_attempts
                 (round_id, wallet_id, kind, bundle_index, proposal_id, batch_digest,
                  payload_digest, chain_tx_hash, state, created_at, updated_at)
                 VALUES (?1, ?2, 'vote', 0, 3, X'', ?3, ?4, ?5, 1, 1)",
                rusqlite::params![ROUND_ID, WALLET, vec![0xCC_u8; 32], tx_hash, state],
            )
            .unwrap();
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
        store_vote_with_recovery(&db, 1, &recovery);
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

    #[tokio::test]
    async fn a_committed_failure_also_clears_the_legacy_domain_hash() {
        let db = test_db();
        // A pre-lifecycle host recorded this submission in the legacy column.
        db.conn()
            .execute(
                "UPDATE bundles SET delegation_tx_hash=?1 WHERE round_id=?2 AND bundle_index=0",
                rusqlite::params![TX_HASH, ROUND_ID],
            )
            .unwrap();
        let transport = Arc::new(MockTransport::default());
        transport.responses.lock().unwrap().push_back(Ok(response(
            200,
            r#"{"height":42,"code":7,"log":"deliver failed","events":[]}"#,
        )));
        let client = accepted_client(transport.clone());
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);
        let identity = ChainSubmissionIdentity::delegation(ROUND_ID, 0);

        let outcome = lifecycle.reconcile(&identity, &|| false).await.unwrap();

        assert!(matches!(
            outcome,
            ChainLifecycleOutcome::Rejected { code: 7, .. }
        ));
        // The domain column is a reconciliation source too, so leaving the
        // failed hash there would rediscover it forever and never dispatch a
        // replacement.
        assert_eq!(db.get_delegation_tx_hash(ROUND_ID, 0).unwrap(), None);
        let next = lifecycle.reconcile(&identity, &|| false).await.unwrap();
        assert_eq!(
            next,
            ChainLifecycleOutcome::Pending {
                known_tx_hashes: Vec::new()
            }
        );
    }

    #[test]
    fn retirement_never_clears_a_confirmed_domain_hash() {
        let db = test_db();
        db.conn()
            .execute(
                "UPDATE bundles SET delegation_tx_hash=?1, van_leaf_position=5
                  WHERE round_id=?2 AND bundle_index=0",
                rusqlite::params![TX_HASH, ROUND_ID],
            )
            .unwrap();

        retire_failed_candidate(
            &db,
            WALLET,
            &ChainSubmissionIdentity::delegation(ROUND_ID, 0),
            TX_HASH,
        )
        .unwrap();

        // A recorded confirmation position means this row is not the failed
        // candidate's, so retirement must leave it alone.
        assert_eq!(
            db.get_delegation_tx_hash(ROUND_ID, 0).unwrap().as_deref(),
            Some(TX_HASH)
        );
    }

    #[tokio::test]
    async fn a_durable_confirmation_is_not_downgraded_by_a_lagging_endpoint() {
        let db = test_db();
        db.conn()
            .execute(
                "UPDATE bundles SET delegation_tx_hash=?1, van_leaf_position=5
                  WHERE round_id=?2 AND bundle_index=0",
                rusqlite::params![TX_HASH, ROUND_ID],
            )
            .unwrap();
        let transport = Arc::new(MockTransport::default());
        // A pruned or lagging endpoint that no longer indexes the transaction.
        transport
            .responses
            .lock()
            .unwrap()
            .push_back(Ok(response(404, r#"{"message":"not indexed"}"#)));
        let client = accepted_client(transport.clone());
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);

        let outcome = lifecycle
            .reconcile(&ChainSubmissionIdentity::delegation(ROUND_ID, 0), &|| false)
            .await
            .unwrap();

        // The applied confirmation is durable and is the strongest evidence
        // there is; a later lookup must not be able to weaken it back to
        // "pending" after a restart or an endpoint switch.
        // Reported without synthesizing event data: the per-transaction VAN
        // position is not recoverable, because `bundles.van_leaf_position` is a
        // single pointer that later confirmations on the bundle advance.
        assert_eq!(
            outcome,
            ChainLifecycleOutcome::AlreadyConfirmed {
                tx_hash: TX_HASH.to_string(),
            }
        );
        assert_eq!(*transport.gets.lock().unwrap(), 0, "no lookup is needed");
    }

    #[tokio::test]
    async fn a_recovery_row_identity_mismatch_is_refused_before_dispatch() {
        use crate::types::EncryptedShare;
        use crate::vote::VoteRecoveryBundle;

        let db = test_db();
        // Recovery JSON whose embedded proposal disagrees with the row it is
        // stored on, as a migrated or inconsistent database could hold.
        let recovery = VoteRecoveryBundle {
            vote_round_id: ROUND_ID.to_string(),
            bundle_index: 0,
            proposal_id: 2,
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
            batch: None,
        };
        store_vote_with_recovery(&db, 1, &recovery);
        let transport = Arc::new(MockTransport::default());
        let client = accepted_client(transport.clone());
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);

        let error = lifecycle
            .submit_vote(ROUND_ID, 0, 1, &|| false)
            .await
            .unwrap_err();

        // Serializing the embedded identity while journaling the requested one
        // would spend this bundle's VAN on the wrong proposal, and the
        // reservation rebuild would reproduce the mismatch rather than catch it.
        assert!(error.to_string().contains("mismatch"), "{error}");
        assert_eq!(*transport.posts.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn cancellation_is_observed_before_the_no_candidate_fast_path() {
        let db = test_db();
        let transport = Arc::new(MockTransport::default());
        let client = accepted_client(transport);
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);

        let outcome = lifecycle
            .reconcile(&ChainSubmissionIdentity::delegation(ROUND_ID, 0), &|| true)
            .await
            .unwrap();

        // With no known candidate there is nothing to look up, but a cancelled
        // operation must not be presented to the host as actively pending.
        assert_eq!(outcome, ChainLifecycleOutcome::Cancelled);
    }

    #[test]
    fn confirmation_persists_under_the_captured_wallet() {
        let db = test_db();
        let events = vec![crate::confirmation::TxEvent {
            event_type: "delegate_vote".to_string(),
            attributes: vec![
                crate::confirmation::TxEventAttribute {
                    key: "vote_round_id".to_string(),
                    value: ROUND_ID.to_string(),
                },
                crate::confirmation::TxEventAttribute {
                    key: "leaf_index".to_string(),
                    value: "5".to_string(),
                },
            ],
        }];
        // The host switches accounts after this operation captured its wallet.
        db.set_wallet_id("wallet-2");

        apply_confirmation(
            &db,
            WALLET,
            &ChainSubmissionIdentity::delegation(ROUND_ID, 0),
            TX_HASH,
            &ChainTxConfirmation {
                height: 9,
                code: 0,
                log: String::new(),
                events,
            },
        )
        .unwrap();

        let stored: Option<String> = db
            .conn()
            .query_row(
                "SELECT delegation_tx_hash FROM bundles
                  WHERE round_id=?1 AND wallet_id=?2 AND bundle_index=0",
                rusqlite::params![ROUND_ID, WALLET],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored.as_deref(), Some(TX_HASH));
    }

    #[test]
    fn the_identity_lock_registry_is_bounded_by_live_operations() {
        let key = format!("registry-test/{ROUND_ID}/delegation/0/-1");
        for _ in 0..64 {
            let lock = operation_lock(&key).unwrap();
            // Two live acquisitions of one identity must share one mutex, or
            // the lock would stop serializing that identity.
            let concurrent = operation_lock(&key).unwrap();
            assert!(Arc::ptr_eq(&lock, &concurrent));
            // Only the registry's weak reference survives this scope.
            assert_eq!(Arc::strong_count(&lock), 2);
        }
        // A long-lived wallet moves through many identities; each must become
        // reclaimable once its operation ends rather than being retained for
        // the process lifetime.
        let after = operation_lock(&key).unwrap();
        assert_eq!(Arc::strong_count(&after), 1);
    }

    #[tokio::test]
    async fn an_earlier_unknown_attempt_survives_a_later_rejection() {
        let db = test_db();
        let transport = Arc::new(MockTransport::default());
        transport.responses.lock().unwrap().extend([
            // Ambiguous: this POST may still have reached the chain.
            Ok(response(503, r#"{"message":"busy"}"#)),
            // The retry is definitively rejected.
            Ok(response(
                200,
                r#"{"tx_hash":"","code":5,"log":"bad nonce"}"#,
            )),
        ]);
        let config = ChainClientConfig::default()
            .with_retry_delays(vec![Duration::from_millis(1)])
            .unwrap();
        let client = ChainClient::with_config(
            transport.clone(),
            ChainEndpointSet::new(&["https://vote.example".to_string()]).unwrap(),
            config,
        );
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);

        let outcome = lifecycle
            .submit_body_locked(
                WALLET,
                ChainSubmissionIdentity::delegation(ROUND_ID, 0),
                b"{}".to_vec(),
                &echo_rebuild,
                &|| false,
            )
            .await
            .unwrap();

        // The rejection is definite only for the attempt that received it. The
        // earlier attempt may still commit, so a terminal-looking `Rejected`
        // would let the host conclude the submission cannot land.
        assert!(
            matches!(&outcome, ChainLifecycleOutcome::OutcomeUnknown { message, .. }
                if message.contains("earlier attempt") && message.contains("bad nonce")),
            "got {outcome:?}"
        );
        assert_eq!(*transport.posts.lock().unwrap(), 2);
    }

    #[tokio::test]
    async fn a_hashless_unknown_attempt_survives_across_calls() {
        let db = test_db();
        // A timeout or unusable accepted response leaves this behind.
        journal_attempt(&db, "outcome_unknown", None);
        let transport = Arc::new(MockTransport::default());
        let client = accepted_client(transport.clone());
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);
        let identity = ChainSubmissionIdentity::delegation(ROUND_ID, 0);

        let reconciled = lifecycle.reconcile(&identity, &|| false).await.unwrap();

        // There is no hash to look up, but the attempt may still commit, so the
        // durable evidence must not be reported as a plain pending submission.
        assert!(
            matches!(&reconciled, ChainLifecycleOutcome::OutcomeUnknown { known_tx_hashes, .. }
                if known_tx_hashes.is_empty()),
            "got {reconciled:?}"
        );

        // A later call may retry, but a rejection of that retry cannot be
        // terminal while the earlier attempt is still unresolved.
        transport.responses.lock().unwrap().push_back(Ok(response(
            200,
            r#"{"tx_hash":"","code":5,"log":"bad nonce"}"#,
        )));
        let outcome = lifecycle
            .submit_body_locked(WALLET, identity, b"{}".to_vec(), &echo_rebuild, &|| false)
            .await
            .unwrap();
        assert!(
            matches!(&outcome, ChainLifecycleOutcome::OutcomeUnknown { message, .. }
                if message.contains("earlier attempt")),
            "got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn a_definite_pre_dispatch_failure_is_not_recorded_as_ambiguity() {
        let db = test_db();
        let transport = Arc::new(MockTransport::default());
        transport.responses.lock().unwrap().extend([
            // Definitely never dispatched: its reservation is deleted.
            Err(ChainTransportError::Transport("connection refused".into())),
            Ok(response(
                200,
                r#"{"tx_hash":"","code":5,"log":"bad nonce"}"#,
            )),
        ]);
        let config = ChainClientConfig::default()
            .with_retry_delays(vec![Duration::from_millis(1)])
            .unwrap();
        let client = ChainClient::with_config(
            transport.clone(),
            ChainEndpointSet::new(&["https://vote.example".to_string()]).unwrap(),
            config,
        );
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);

        let outcome = lifecycle
            .submit_body_locked(
                WALLET,
                ChainSubmissionIdentity::delegation(ROUND_ID, 0),
                b"{}".to_vec(),
                &echo_rebuild,
                &|| false,
            )
            .await
            .unwrap();

        // Nothing was ever dispatched by the first attempt, so the rejection is
        // the whole truth and must stay terminal.
        assert_eq!(
            outcome,
            ChainLifecycleOutcome::Rejected {
                code: 5,
                log: "bad nonce".to_string()
            }
        );
    }

    #[tokio::test]
    async fn a_candidate_recorded_between_attempts_stops_further_dispatch() {
        let db = test_db();
        let transport = Arc::new(MockTransport::default());
        transport.responses.lock().unwrap().extend([
            // First POST is ambiguous, so the call retries.
            Ok(response(503, r#"{"message":"busy"}"#)),
            // The between-attempt reconciliation finds a candidate another
            // writer recorded, and its lookup says it is still pending.
            Ok(response(404, r#"{"message":"not indexed"}"#)),
        ]);
        let config = ChainClientConfig::default()
            .with_retry_delays(vec![Duration::from_millis(1)])
            .unwrap();
        let client = ChainClient::with_config(
            transport.clone(),
            ChainEndpointSet::new(&["https://vote.example".to_string()]).unwrap(),
            config,
        );
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);
        let identity = ChainSubmissionIdentity::delegation(ROUND_ID, 0);

        // Stand in for another process recording the hash while this call is
        // between attempts: the cancellation hook is the only callback the
        // lifecycle invokes mid-call, and it always reports "not cancelled".
        let recorded = Mutex::new(false);
        let record_after_first_post = || {
            let mut recorded = recorded.lock().unwrap();
            if !*recorded && *transport.posts.lock().unwrap() == 1 {
                journal_attempt(&db, "accepted", Some(TX_HASH));
                *recorded = true;
            }
            false
        };

        let outcome = lifecycle
            .submit_body_locked(
                WALLET,
                identity,
                b"{}".to_vec(),
                &echo_rebuild,
                &record_after_first_post,
            )
            .await
            .unwrap();

        assert_eq!(
            outcome,
            ChainLifecycleOutcome::Pending {
                known_tx_hashes: vec![TX_HASH.to_string()]
            }
        );
        assert_eq!(
            *transport.posts.lock().unwrap(),
            1,
            "a known candidate that may still commit stops further broadcasts"
        );
    }

    fn delegate_vote_events(leaf_index: &str) -> Vec<crate::confirmation::TxEvent> {
        vec![crate::confirmation::TxEvent {
            event_type: "delegate_vote".to_string(),
            attributes: vec![
                crate::confirmation::TxEventAttribute {
                    key: "vote_round_id".to_string(),
                    value: ROUND_ID.to_string(),
                },
                crate::confirmation::TxEventAttribute {
                    key: "leaf_index".to_string(),
                    value: leaf_index.to_string(),
                },
            ],
        }]
    }

    #[tokio::test]
    async fn a_committed_failure_does_not_override_hashless_ambiguity() {
        let db = test_db();
        // An earlier dispatch left no hash, and a later one did.
        journal_attempt(&db, "outcome_unknown", None);
        journal_attempt(&db, "accepted", Some(TX_HASH));
        let transport = Arc::new(MockTransport::default());
        transport.responses.lock().unwrap().push_back(Ok(response(
            200,
            r#"{"height":42,"code":7,"log":"deliver failed","events":[]}"#,
        )));
        let client = accepted_client(transport);
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);

        let outcome = lifecycle
            .reconcile(&ChainSubmissionIdentity::delegation(ROUND_ID, 0), &|| false)
            .await
            .unwrap();

        // The rejection is definite only for the candidate it names; the
        // hashless attempt may still commit, so the host must keep polling.
        assert!(
            matches!(&outcome, ChainLifecycleOutcome::OutcomeUnknown { .. }),
            "got {outcome:?}"
        );
        assert_eq!(
            attempt_states(&db),
            vec!["outcome_unknown".to_string(), "rejected".to_string()],
            "the failed candidate is still retired"
        );
    }

    #[tokio::test]
    async fn every_committed_failure_candidate_is_retired() {
        let db = test_db();
        journal_attempt(&db, "accepted", Some(TX_HASH));
        journal_attempt(&db, "accepted", Some(TX_HASH_2));
        let transport = Arc::new(MockTransport::default());
        transport.responses.lock().unwrap().extend([
            Ok(response(
                200,
                r#"{"height":42,"code":7,"log":"deliver failed","events":[]}"#,
            )),
            Ok(response(
                200,
                r#"{"height":43,"code":9,"log":"deliver failed","events":[]}"#,
            )),
        ]);
        let client = accepted_client(transport);
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);

        let outcome = lifecycle
            .reconcile(&ChainSubmissionIdentity::delegation(ROUND_ID, 0), &|| false)
            .await
            .unwrap();

        assert!(matches!(
            outcome,
            ChainLifecycleOutcome::Rejected { code: 7, .. }
        ));
        // Retiring only the reported one would leave the other blocking a
        // replacement until the host reconciled again per failed candidate.
        assert_eq!(
            attempt_states(&db),
            vec!["rejected".to_string(), "rejected".to_string()]
        );
    }

    #[tokio::test]
    async fn a_successful_candidate_survives_a_terminal_lookup_error() {
        let db = test_db();
        journal_attempt(&db, "accepted", Some(TX_HASH));
        journal_attempt(&db, "accepted", Some(TX_HASH_2));
        let transport = Arc::new(MockTransport::default());
        let events = serde_json::to_string(&delegate_vote_events("5")).unwrap();
        transport.responses.lock().unwrap().extend([
            Ok(response(
                200,
                &format!(r#"{{"height":42,"code":0,"log":"","events":{events}}}"#),
            )),
            // A stable per-hash failure on an unrelated candidate.
            Ok(response(401, r#"{"message":"unauthorized"}"#)),
        ]);
        let client = accepted_client(transport);
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);

        let outcome = lifecycle
            .reconcile(&ChainSubmissionIdentity::delegation(ROUND_ID, 0), &|| false)
            .await
            .unwrap();

        // Returning the error immediately would leave a transaction that
        // definitely committed unapplied for as long as the error persisted.
        assert!(
            matches!(&outcome, ChainLifecycleOutcome::Confirmed { tx_hash, .. }
                if tx_hash == TX_HASH),
            "got {outcome:?}"
        );
        assert_eq!(
            db.get_delegation_tx_hash(ROUND_ID, 0).unwrap().as_deref(),
            Some(TX_HASH)
        );
    }

    #[tokio::test]
    async fn a_spent_nullifier_response_accepts_a_durable_confirmation() {
        let db = test_db();
        let transport = Arc::new(MockTransport::default());
        transport.responses.lock().unwrap().push_back(Ok(response(
            200,
            r#"{"tx_hash":"","code":9,"log":"nullifier already spent: ab"}"#,
        )));
        let client = accepted_client(transport.clone());
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);

        // Another process applies the confirmation after this call's preflight
        // but before its POST is answered.
        let applied = Mutex::new(false);
        let confirm_after_post = || {
            let mut applied = applied.lock().unwrap();
            if !*applied && *transport.posts.lock().unwrap() == 1 {
                db.conn()
                    .execute(
                        "UPDATE bundles SET delegation_tx_hash=?1, van_leaf_position=5
                          WHERE round_id=?2 AND bundle_index=0",
                        rusqlite::params![TX_HASH, ROUND_ID],
                    )
                    .unwrap();
                *applied = true;
            }
            false
        };

        let outcome = lifecycle
            .submit_body_locked(
                WALLET,
                ChainSubmissionIdentity::delegation(ROUND_ID, 0),
                b"{}".to_vec(),
                &echo_rebuild,
                &confirm_after_post,
            )
            .await
            .unwrap();

        // Durable proof of success outranks the spent-nullifier ambiguity.
        assert_eq!(
            outcome,
            ChainLifecycleOutcome::AlreadyConfirmed {
                tx_hash: TX_HASH.to_string()
            }
        );
    }

    #[tokio::test]
    async fn cancellation_after_an_ambiguous_post_preserves_the_ambiguity() {
        let db = test_db();
        let transport = Arc::new(MockTransport::default());
        // Ambiguous: this POST may still have reached the chain.
        transport
            .responses
            .lock()
            .unwrap()
            .push_back(Ok(response(503, r#"{"message":"busy"}"#)));
        let config = ChainClientConfig::default()
            .with_retry_delays(vec![Duration::from_millis(1)])
            .unwrap();
        let client = ChainClient::with_config(
            transport.clone(),
            ChainEndpointSet::new(&["https://vote.example".to_string()]).unwrap(),
            config,
        );
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);

        // The host cancels while that POST is in flight.
        let outcome = lifecycle
            .submit_body_locked(
                WALLET,
                ChainSubmissionIdentity::delegation(ROUND_ID, 0),
                b"{}".to_vec(),
                &echo_rebuild,
                &|| *transport.posts.lock().unwrap() > 0,
            )
            .await
            .unwrap();

        // Cancellation observed after a broadcast completes does not replace
        // that broadcast's result: the transaction may still commit.
        assert!(
            matches!(&outcome, ChainLifecycleOutcome::OutcomeUnknown { .. }),
            "got {outcome:?}"
        );
        assert_eq!(attempt_states(&db), vec!["outcome_unknown".to_string()]);
    }

    #[tokio::test]
    async fn cancellation_before_any_dispatch_is_reported_as_cancelled() {
        let db = test_db();
        let transport = Arc::new(MockTransport::default());
        let client = accepted_client(transport.clone());
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);

        let outcome = lifecycle
            .submit_body_locked(
                WALLET,
                ChainSubmissionIdentity::delegation(ROUND_ID, 0),
                b"{}".to_vec(),
                &echo_rebuild,
                &|| true,
            )
            .await
            .unwrap();

        assert_eq!(outcome, ChainLifecycleOutcome::Cancelled);
        assert_eq!(*transport.posts.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn adopting_a_success_still_retires_the_failed_candidates() {
        let db = test_db();
        journal_attempt(&db, "accepted", Some(TX_HASH));
        journal_attempt(&db, "accepted", Some(TX_HASH_2));
        let transport = Arc::new(MockTransport::default());
        let events = serde_json::to_string(&delegate_vote_events("5")).unwrap();
        transport.responses.lock().unwrap().extend([
            Ok(response(
                200,
                &format!(r#"{{"height":42,"code":0,"log":"","events":{events}}}"#),
            )),
            Ok(response(
                200,
                r#"{"height":43,"code":9,"log":"deliver failed","events":[]}"#,
            )),
        ]);
        let client = accepted_client(transport);
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);

        let outcome = lifecycle
            .reconcile(&ChainSubmissionIdentity::delegation(ROUND_ID, 0), &|| false)
            .await
            .unwrap();

        assert!(
            matches!(&outcome, ChainLifecycleOutcome::Confirmed { tx_hash, .. }
                if tx_hash == TX_HASH),
            "got {outcome:?}"
        );
        // The duplicate that failed must not stay live: it would keep
        // protecting recovery rows and blocking bundle pruning.
        assert_eq!(
            attempt_states(&db),
            vec!["accepted".to_string(), "rejected".to_string()]
        );
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

    #[tokio::test]
    async fn a_hashless_unknown_attempt_stops_covering_its_vote_row() {
        let db = test_db();
        {
            let conn = db.conn();
            queries::store_vote(&conn, ROUND_ID, WALLET, 0, 3, 1, &[0xCC; 32]).unwrap();
        }
        let transport = Arc::new(MockTransport::default());
        // A timeout, then a definite rejection of the byte-identical payload.
        transport
            .responses
            .lock()
            .unwrap()
            .push_back(Err(ChainTransportError::Timeout));
        transport.responses.lock().unwrap().push_back(Ok(response(
            422,
            r#"{"tx_hash":"","code":7,"log":"invalid proof"}"#,
        )));
        let client = ChainClient::with_config(
            transport,
            ChainEndpointSet::new(&["https://vote.example".to_string()]).unwrap(),
            ChainClientConfig::default()
                .with_retry_delays(vec![Duration::from_millis(1)])
                .unwrap(),
        );
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);
        let identity = ChainSubmissionIdentity::vote(ROUND_ID, 0, 3);

        let outcome = lifecycle
            .submit_body_locked(
                WALLET,
                identity.clone(),
                b"{}".to_vec(),
                &echo_rebuild,
                &|| false,
            )
            .await
            .unwrap();

        // The first attempt may still commit, so the call stays ambiguous and
        // `has_live_attempt` keeps saying so across later calls.
        assert!(matches!(
            outcome,
            ChainLifecycleOutcome::OutcomeUnknown { .. }
        ));
        assert!(has_live_attempt(&db, WALLET, &identity).unwrap());

        // But it never learned a transaction hash and never can, so it cannot
        // be looked up or confirmed. Covering the row would only freeze this
        // proposal's recovery generation, ballot intent, and bundle pruning for
        // the life of the round, because nothing ever retires such an attempt.
        let protected = {
            let conn = db.conn();
            attempt_protected_vote_rows(&conn, ROUND_ID, WALLET).unwrap()
        };
        assert!(protected.is_empty(), "{protected:?}");
    }

    #[tokio::test]
    async fn an_accepted_hash_still_covers_its_vote_row() {
        let db = test_db();
        {
            let conn = db.conn();
            queries::store_vote(&conn, ROUND_ID, WALLET, 0, 3, 1, &[0xCC; 32]).unwrap();
        }
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
        let identity = ChainSubmissionIdentity::vote(ROUND_ID, 0, 3);

        lifecycle
            .submit_body_locked(WALLET, identity, b"{}".to_vec(), &echo_rebuild, &|| false)
            .await
            .unwrap();

        // This one can be looked up, so the recovery it would be confirmed
        // against must survive.
        let protected = {
            let conn = db.conn();
            attempt_protected_vote_rows(&conn, ROUND_ID, WALLET).unwrap()
        };
        assert!(protected.contains(&(0, 3)), "{protected:?}");
    }

    /// A transport that lets another writer record a candidate between this
    /// call's preflight and its response, the way a second operation or a
    /// legacy recording call would.
    struct RacingTransport {
        inner: MockTransport,
        db_path: String,
        raced: Mutex<bool>,
    }

    impl ChainTransport for RacingTransport {
        fn get<'a>(&'a self, url: &'a str, timeout: Duration) -> ChainFuture<'a> {
            self.inner.get(url, timeout)
        }

        fn post_json<'a>(
            &'a self,
            url: &'a str,
            body: Vec<u8>,
            timeout: Duration,
        ) -> ChainFuture<'a> {
            {
                let mut raced = self.raced.lock().unwrap();
                if !*raced {
                    *raced = true;
                    let conn = rusqlite::Connection::open(&self.db_path).unwrap();
                    conn.execute(
                        "INSERT INTO chain_submission_attempts
                         (round_id, wallet_id, kind, bundle_index, proposal_id, batch_digest,
                          payload_digest, chain_tx_hash, state, created_at, updated_at)
                         VALUES (?1, ?2, 'vote', 0, 3, X'', ?3, ?4, 'accepted', 1, 1)",
                        rusqlite::params![ROUND_ID, WALLET, vec![0xEE_u8; 32], TX_HASH_2],
                    )
                    .unwrap();
                }
            }
            self.inner.post_json(url, body, timeout)
        }
    }

    #[tokio::test]
    async fn an_accepted_candidate_recorded_mid_call_is_not_overridden_by_a_rejection() {
        let path = std::env::temp_dir().join(format!(
            "zcash_voting_racing_candidate_{}.sqlite",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let db = file_test_db(path.to_str().unwrap());
        {
            let conn = db.conn();
            queries::store_vote(&conn, ROUND_ID, WALLET, 0, 3, 1, &[0xCC; 32]).unwrap();
        }

        let inner = MockTransport::default();
        // This attempt's own definite rejection, then the racing candidate's
        // lookup answering "not yet committed".
        inner.responses.lock().unwrap().push_back(Ok(response(
            422,
            r#"{"tx_hash":"","code":7,"log":"invalid proof"}"#,
        )));
        inner
            .responses
            .lock()
            .unwrap()
            .push_back(Ok(response(404, r#"{"detail":"not found"}"#)));
        let transport = Arc::new(RacingTransport {
            inner,
            db_path: path.to_str().unwrap().to_string(),
            raced: Mutex::new(false),
        });
        let client = ChainClient::new(
            transport,
            ChainEndpointSet::new(&["https://vote.example".to_string()]).unwrap(),
        );
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);
        let identity = ChainSubmissionIdentity::vote(ROUND_ID, 0, 3);

        let outcome = lifecycle
            .submit_body_locked(WALLET, identity, b"{}".to_vec(), &echo_rebuild, &|| false)
            .await
            .unwrap();

        // The candidate is `accepted`, which is neither `attempting` nor
        // `outcome_unknown`, so `has_live_attempt` cannot see it. Reporting this
        // attempt's rejection as terminal would tell the host the vote cannot
        // land while another transaction for the same identity is still pending.
        assert_eq!(
            outcome,
            ChainLifecycleOutcome::Pending {
                known_tx_hashes: vec![TX_HASH_2.to_string()],
            }
        );
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn adopting_a_success_clears_a_conflicting_unconfirmed_domain_hash() {
        let db = test_db();
        // A pre-lifecycle host recorded an opaque identifier the v18 migration
        // deliberately preserves. `known_hashes` skips it as a candidate, but
        // the domain writer still refuses to overwrite it.
        db.conn()
            .execute(
                "UPDATE bundles SET delegation_tx_hash='legacy-hash'
                  WHERE round_id=?1 AND wallet_id=?2 AND bundle_index=0",
                rusqlite::params![ROUND_ID, WALLET],
            )
            .unwrap();
        journal_attempt(&db, "accepted", Some(TX_HASH));
        let transport = Arc::new(MockTransport::default());
        let events = serde_json::to_string(&delegate_vote_events("5")).unwrap();
        transport.responses.lock().unwrap().push_back(Ok(response(
            200,
            &format!(r#"{{"height":42,"code":0,"log":"","events":{events}}}"#),
        )));
        let client = accepted_client(transport);
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);

        let outcome = lifecycle
            .reconcile(&ChainSubmissionIdentity::delegation(ROUND_ID, 0), &|| false)
            .await
            .unwrap();

        // Without clearing the conflict the confirmation transaction fails, and
        // every later reconciliation rediscovers the same committed transaction
        // and fails the same way: the VAN position stays unset for good.
        assert!(
            matches!(&outcome, ChainLifecycleOutcome::Confirmed { tx_hash, .. }
                if tx_hash == TX_HASH),
            "got {outcome:?}"
        );
        let (hash, position): (Option<String>, Option<i64>) = db
            .conn()
            .query_row(
                "SELECT delegation_tx_hash, van_leaf_position FROM bundles
                  WHERE round_id=?1 AND wallet_id=?2 AND bundle_index=0",
                rusqlite::params![ROUND_ID, WALLET],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(hash.as_deref(), Some(TX_HASH));
        assert_eq!(position, Some(5));
    }

    #[tokio::test]
    async fn a_confirmation_that_fails_validation_keeps_the_competing_hash() {
        let db = test_db();
        // A vote row carrying a competing unconfirmed hash a legacy recording
        // call wrote, and no recovery JSON — so confirmation gets past the
        // event checks and then fails inside its own transaction, which is where
        // the validation that needs durable state lives.
        {
            let conn = db.conn();
            queries::store_vote(&conn, ROUND_ID, WALLET, 0, 3, 1, &[0xCC; 32]).unwrap();
            conn.execute(
                "UPDATE votes SET tx_hash=?3
                  WHERE round_id=?1 AND wallet_id=?2 AND bundle_index=0 AND proposal_id=3",
                rusqlite::params![ROUND_ID, WALLET, TX_HASH_2],
            )
            .unwrap();
        }
        journal_vote_attempt(&db, "accepted", Some(TX_HASH));
        let transport = Arc::new(MockTransport::default());
        let events = serde_json::to_string(&vec![crate::confirmation::TxEvent {
            event_type: "cast_vote".to_string(),
            attributes: vec![
                crate::confirmation::TxEventAttribute {
                    key: "vote_round_id".to_string(),
                    value: ROUND_ID.to_string(),
                },
                crate::confirmation::TxEventAttribute {
                    key: "leaf_index".to_string(),
                    value: "5,7".to_string(),
                },
            ],
        }])
        .unwrap();
        // `known_hashes` reads the domain column before the journal, so the
        // competing candidate is queried first and is still pending; the
        // journaled candidate comes back committed.
        transport.responses.lock().unwrap().extend([
            Ok(response(404, r#"{"detail":"not found"}"#)),
            Ok(response(
                200,
                &format!(r#"{{"height":42,"code":0,"log":"","events":{events}}}"#),
            )),
        ]);
        let client = accepted_client(transport);
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);

        let result = lifecycle
            .reconcile(&ChainSubmissionIdentity::vote(ROUND_ID, 0, 3), &|| false)
            .await;

        assert!(result.is_err(), "got {result:?}");
        // Clearing happens inside the confirmation transaction and after the
        // checks that can still reject it, so a confirmation that cannot be
        // applied takes the clearing back with it and leaves the competing
        // candidate available to the next reconciliation.
        let stored: Option<String> = db
            .conn()
            .query_row(
                "SELECT tx_hash FROM votes
                  WHERE round_id=?1 AND wallet_id=?2 AND bundle_index=0 AND proposal_id=3",
                rusqlite::params![ROUND_ID, WALLET],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored.as_deref(), Some(TX_HASH_2));
    }

    #[test]
    fn a_reservation_this_process_awaits_survives_a_wall_clock_jump() {
        let db = test_db();
        {
            let conn = db.conn();
            queries::store_vote(&conn, ROUND_ID, WALLET, 0, 3, 1, &[0xCC; 32]).unwrap();
        }
        const ATTEMPT_ID: i64 = 9_000_001;
        // Timestamps that look older than the grace period. A forward step of
        // the system clock while the POST is in flight produces exactly this:
        // the row is untouched, but "now" has moved past its deadline.
        let ancient = 1_i64;
        db.conn()
            .execute(
                "INSERT INTO chain_submission_attempts
                 (id, round_id, wallet_id, kind, bundle_index, proposal_id, batch_digest,
                  payload_digest, state, created_at, updated_at)
                 VALUES (?1, ?2, ?3, 'vote', 0, 3, X'', ?4, 'attempting', ?5, ?5)",
                rusqlite::params![ATTEMPT_ID, ROUND_ID, WALLET, vec![0xCC_u8; 32], ancient],
            )
            .unwrap();

        let identity = ChainSubmissionIdentity::vote(ROUND_ID, 0, 3);
        let in_flight = InFlightAttempt::register(WALLET, &identity);
        let covered = {
            let conn = db.conn();
            attempt_protected_vote_rows(&conn, ROUND_ID, WALLET).unwrap()
        };
        // The age test is the weaker of the two and never the only one: this
        // process knows it is waiting on the response, so the recovery that
        // response would be confirmed against must not become erasable just
        // because the clock moved.
        assert!(covered.contains(&(0, 3)), "{covered:?}");

        drop(in_flight);
        let after = {
            let conn = db.conn();
            attempt_protected_vote_rows(&conn, ROUND_ID, WALLET).unwrap()
        };
        // Once nothing is waiting on it, the stale reservation is an
        // interrupted one and stops covering.
        assert!(after.is_empty(), "{after:?}");
    }

    #[tokio::test]
    async fn an_accepted_hash_survives_a_failure_to_journal_it() {
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

        // Make journaling the outcome fail the way a full disk or a stuck
        // writer would, after CheckTx has already accepted the transaction.
        db.conn()
            .execute_batch(
                "CREATE TRIGGER fail_attempt_update
                 BEFORE UPDATE ON chain_submission_attempts
                 BEGIN SELECT RAISE(ABORT, 'storage failure'); END",
            )
            .unwrap();

        let error = lifecycle
            .submit_body_locked(WALLET, identity, b"{}".to_vec(), &echo_rebuild, &|| false)
            .await
            .unwrap_err();

        // The transaction is in the mempool and this hash is the only handle
        // anything will ever have on it: the SDK does not predict chain hashes
        // and cannot find a transaction from its commitment.
        match error {
            ChainLifecycleError::AcceptedButUnjournaled { tx_hash, .. } => {
                assert_eq!(tx_hash, TX_HASH);
            }
            other => panic!("got {other:?}"),
        }
    }

    /// Flips a cancellation flag once the status request has actually been
    /// issued, so cancellation lands after the lookup rather than before it.
    struct CancelOnLookupTransport {
        inner: MockTransport,
        cancelled: Arc<std::sync::atomic::AtomicBool>,
    }

    impl ChainTransport for CancelOnLookupTransport {
        fn get<'a>(&'a self, url: &'a str, timeout: Duration) -> ChainFuture<'a> {
            self.cancelled
                .store(true, std::sync::atomic::Ordering::SeqCst);
            self.inner.get(url, timeout)
        }

        fn post_json<'a>(
            &'a self,
            url: &'a str,
            body: Vec<u8>,
            timeout: Duration,
        ) -> ChainFuture<'a> {
            self.inner.post_json(url, body, timeout)
        }
    }

    #[tokio::test]
    async fn cancellation_after_a_lookup_stops_before_retiring_evidence() {
        let db = test_db();
        journal_attempt(&db, "accepted", Some(TX_HASH));
        let inner = MockTransport::default();
        // A committed failure: classifying it would retire the attempt.
        inner.responses.lock().unwrap().push_back(Ok(response(
            422,
            r#"{"height":42,"code":7,"log":"invalid proof","events":[]}"#,
        )));
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let transport = Arc::new(CancelOnLookupTransport {
            inner,
            cancelled: Arc::clone(&cancelled),
        });
        let client = ChainClient::new(
            transport,
            ChainEndpointSet::new(&["https://vote.example".to_string()]).unwrap(),
        );
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);
        let cancel = || cancelled.load(std::sync::atomic::Ordering::SeqCst);

        let outcome = lifecycle
            .reconcile(&ChainSubmissionIdentity::delegation(ROUND_ID, 0), &cancel)
            .await
            .unwrap();

        assert_eq!(outcome, ChainLifecycleOutcome::Cancelled);
        // A cancelled operation must not mutate durable state on the way out.
        // The candidate is still journaled, so the next reconciliation
        // re-derives what this one was about to conclude.
        assert_eq!(attempt_states(&db), vec!["accepted".to_string()]);
    }

    #[tokio::test]
    async fn an_unrelated_confirmation_fails_over_to_the_next_endpoint() {
        let db = test_db();
        journal_attempt(&db, "accepted", Some(TX_HASH));
        let transport = Arc::new(MockTransport::default());
        // The first endpoint answers about this hash with a structurally valid
        // committed success whose events belong to a different round.
        let wrong = serde_json::to_string(&vec![crate::confirmation::TxEvent {
            event_type: "delegate_vote".to_string(),
            attributes: vec![
                crate::confirmation::TxEventAttribute {
                    key: "vote_round_id".to_string(),
                    value: "9".repeat(64),
                },
                crate::confirmation::TxEventAttribute {
                    key: "leaf_index".to_string(),
                    value: "5".to_string(),
                },
            ],
        }])
        .unwrap();
        let right = serde_json::to_string(&delegate_vote_events("5")).unwrap();
        transport.responses.lock().unwrap().extend([
            Ok(response(
                200,
                &format!(r#"{{"height":42,"code":0,"log":"","events":{wrong}}}"#),
            )),
            Ok(response(
                200,
                &format!(r#"{{"height":42,"code":0,"log":"","events":{right}}}"#),
            )),
        ]);
        let client = ChainClient::new(
            transport,
            ChainEndpointSet::new(&[
                "https://one.example".to_string(),
                "https://two.example".to_string(),
            ])
            .unwrap(),
        );
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);

        let outcome = lifecycle
            .reconcile(&ChainSubmissionIdentity::delegation(ROUND_ID, 0), &|| false)
            .await
            .unwrap();

        // Whether a committed result describes this submission is part of
        // endpoint failover. Returning on the first structurally valid answer
        // would let one endpoint's wrong events end the search, and stable
        // endpoint ordering would repeat that on every later call while the
        // second endpoint could serve the real confirmation all along.
        assert!(
            matches!(&outcome, ChainLifecycleOutcome::Confirmed { tx_hash, .. }
                if tx_hash == TX_HASH),
            "got {outcome:?}"
        );
    }

    #[test]
    fn a_heartbeat_marks_only_an_outstanding_reservation_as_still_owned() {
        let db = test_db();
        let outstanding = journal_attempt(&db, "attempting", None);
        let settled = journal_attempt(&db, "outcome_unknown", None);
        db.conn()
            .execute("UPDATE chain_submission_attempts SET updated_at=1", [])
            .unwrap();

        touch_attempt(&db, WALLET, outstanding);
        touch_attempt(&db, WALLET, settled);
        // A different account must not be able to refresh this reservation.
        touch_attempt(&db, "someone-else", outstanding);

        let stamps: Vec<i64> = db
            .conn()
            .prepare("SELECT updated_at FROM chain_submission_attempts ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        // Only a reservation still awaiting its response is refreshed: the
        // column has to mean "the owner was alive at this time", which is what
        // the age test downstream reads it as.
        assert!(stamps[0] > 1, "{stamps:?}");
        assert_eq!(stamps[1], 1, "{stamps:?}");
        // The refresh has to be frequent enough that a live reservation stays
        // well inside the window the age test allows.
        assert!(
            RESERVATION_HEARTBEAT.as_secs() as i64 * 4 < INTERRUPTED_RESERVATION_GRACE_SECS,
            "heartbeat must be far below the grace period"
        );
    }

    #[test]
    fn in_flight_coverage_does_not_cross_databases() {
        let first = test_db();
        let second = test_db();
        {
            let conn = first.conn();
            queries::store_vote(&conn, ROUND_ID, WALLET, 0, 3, 1, &[0xCC; 32]).unwrap();
        }
        {
            let conn = second.conn();
            queries::store_vote(&conn, ROUND_ID, WALLET, 0, 4, 1, &[0xCC; 32]).unwrap();
        }
        // Both databases mint row id 1 for their first reservation, and both
        // reservations are stale, so an id-keyed registry would let the one
        // being registered stand in for the other.
        for (db, proposal_id) in [(&first, 3_i64), (&second, 4_i64)] {
            let conn = db.conn();
            conn.execute(
                "INSERT INTO chain_submission_attempts
                 (round_id, wallet_id, kind, bundle_index, proposal_id, batch_digest,
                  payload_digest, state, created_at, updated_at)
                 VALUES (?1, ?2, 'vote', 0, ?3, X'', ?4, 'attempting', 1, 1)",
                rusqlite::params![ROUND_ID, WALLET, proposal_id, vec![0xCC_u8; 32]],
            )
            .unwrap();
            assert_eq!(
                conn.last_insert_rowid(),
                1,
                "both databases must mint the same row id"
            );
        }

        let identity = ChainSubmissionIdentity::vote(ROUND_ID, 0, 3);
        let in_flight = InFlightAttempt::register(WALLET, &identity);

        let covered_first = {
            let conn = first.conn();
            attempt_protected_vote_rows(&conn, ROUND_ID, WALLET).unwrap()
        };
        let covered_second = {
            let conn = second.conn();
            attempt_protected_vote_rows(&conn, ROUND_ID, WALLET).unwrap()
        };
        assert!(covered_first.contains(&(0, 3)), "{covered_first:?}");
        // The other database's expired reservation must not be reported live
        // just because an unrelated handle currently owns that row id.
        assert!(!covered_second.contains(&(0, 4)), "{covered_second:?}");
        // Keying by identity does carry the registration into any database
        // holding the same wallet and round, such as a copy opened alongside.
        // That over-protects a row rather than under-protecting one, which is
        // the safe direction and the opposite of an id collision.
        assert!(covered_second.contains(&(0, 3)), "{covered_second:?}");
        drop(in_flight);
    }

    #[tokio::test]
    async fn a_reservation_failure_after_an_ambiguous_post_stays_unknown() {
        let db = test_db();
        let transport = Arc::new(MockTransport::default());
        // First attempt times out: ambiguous, no hash. The retry's rebuild then
        // fails because the durable generation changed underneath it.
        transport
            .responses
            .lock()
            .unwrap()
            .push_back(Err(ChainTransportError::Timeout));
        let client = ChainClient::with_config(
            transport,
            ChainEndpointSet::new(&["https://vote.example".to_string()]).unwrap(),
            ChainClientConfig::default()
                .with_retry_delays(vec![Duration::from_millis(1)])
                .unwrap(),
        );
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);
        let identity = ChainSubmissionIdentity::delegation(ROUND_ID, 0);
        let stale = |_: &rusqlite::Connection| -> Result<Vec<u8>, VotingError> {
            Ok(b"different bytes".to_vec())
        };
        let rebuild = move |conn: &rusqlite::Connection| -> Result<Vec<u8>, VotingError> {
            // Matches on the first reservation, diverges on the retry.
            static SEEN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            if SEEN.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                echo_rebuild(conn)
            } else {
                stale(conn)
            }
        };

        let outcome = lifecycle
            .submit_body_locked(WALLET, identity, b"{}".to_vec(), &rebuild, &|| false)
            .await
            .unwrap();

        // The first attempt's transaction may still commit. Reporting only the
        // persistence error would invite the host to treat the replacement
        // generation as safe to submit.
        assert!(
            matches!(&outcome, ChainLifecycleOutcome::OutcomeUnknown { message, .. }
                if message.contains("did not settle")),
            "got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn the_in_flight_guard_is_held_before_the_reservation_is_journaled() {
        let db = test_db();
        {
            let conn = db.conn();
            queries::store_vote(&conn, ROUND_ID, WALLET, 0, 3, 1, &[0xCC; 32]).unwrap();
        }
        let transport = Arc::new(MockTransport::default());
        transport.responses.lock().unwrap().push_back(Ok(response(
            200,
            &format!(r#"{{"tx_hash":"{TX_HASH}","code":0,"log":""}}"#),
        )));
        let client = accepted_client(transport);
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);

        // The rebuild runs inside the reservation transaction, before its row is
        // committed, so only the registry can be covering the identity here.
        let covered_during_reservation = Arc::new(Mutex::new(false));
        let observed = Arc::clone(&covered_during_reservation);
        let rebuild = move |conn: &rusqlite::Connection| -> Result<Vec<u8>, VotingError> {
            let protected = attempt_protected_vote_rows(conn, ROUND_ID, WALLET)?;
            *observed.lock().unwrap() = protected.contains(&(0, 3));
            echo_rebuild(conn)
        };

        lifecycle
            .submit_body_locked(
                WALLET,
                ChainSubmissionIdentity::vote(ROUND_ID, 0, 3),
                b"{}".to_vec(),
                &rebuild,
                &|| false,
            )
            .await
            .unwrap();

        // A guard taken after the reservation commits leaves a window in which
        // cleanup can erase the generation these bytes were built from, while
        // the call goes on to POST them.
        assert!(
            *covered_during_reservation.lock().unwrap(),
            "the identity must be covered before its reservation is journaled"
        );
    }

    /// Records a racing accepted candidate during the POST, then cancels, so
    /// the rejection branch's reconciliation reaches no conclusion.
    struct RacingThenCancelTransport {
        inner: MockTransport,
        db_path: String,
        cancelled: Arc<std::sync::atomic::AtomicBool>,
    }

    impl ChainTransport for RacingThenCancelTransport {
        fn get<'a>(&'a self, url: &'a str, timeout: Duration) -> ChainFuture<'a> {
            self.cancelled
                .store(true, std::sync::atomic::Ordering::SeqCst);
            self.inner.get(url, timeout)
        }

        fn post_json<'a>(
            &'a self,
            url: &'a str,
            body: Vec<u8>,
            timeout: Duration,
        ) -> ChainFuture<'a> {
            let conn = rusqlite::Connection::open(&self.db_path).unwrap();
            conn.execute(
                "INSERT INTO chain_submission_attempts
                 (round_id, wallet_id, kind, bundle_index, proposal_id, batch_digest,
                  payload_digest, chain_tx_hash, state, created_at, updated_at)
                 VALUES (?1, ?2, 'delegation', 0, -1, X'', ?3, ?4, 'accepted', 1, 1)",
                rusqlite::params![ROUND_ID, WALLET, vec![0xEE_u8; 32], TX_HASH_2],
            )
            .unwrap();
            self.inner.post_json(url, body, timeout)
        }
    }

    #[tokio::test]
    async fn a_cancelled_reconciliation_does_not_make_a_rejection_terminal() {
        let path = std::env::temp_dir().join(format!(
            "zcash_voting_cancel_race_{}.sqlite",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let db = file_test_db(path.to_str().unwrap());

        let inner = MockTransport::default();
        inner.responses.lock().unwrap().extend([
            Ok(response(
                422,
                r#"{"tx_hash":"","code":7,"log":"invalid proof"}"#,
            )),
            // Never classified: the lookup's own cancellation check fires first.
            Ok(response(404, r#"{"detail":"not found"}"#)),
        ]);
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let transport = Arc::new(RacingThenCancelTransport {
            inner,
            db_path: path.to_str().unwrap().to_string(),
            cancelled: Arc::clone(&cancelled),
        });
        let client = ChainClient::new(
            transport,
            ChainEndpointSet::new(&["https://vote.example".to_string()]).unwrap(),
        );
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);
        let cancel = || cancelled.load(std::sync::atomic::Ordering::SeqCst);

        let outcome = lifecycle
            .submit_body_locked(
                WALLET,
                ChainSubmissionIdentity::delegation(ROUND_ID, 0),
                b"{}".to_vec(),
                &echo_rebuild,
                &cancel,
            )
            .await
            .unwrap();

        // The reconciliation was cancelled, so it settled nothing, and the
        // racing candidate is `accepted` — which `has_live_attempt` does not
        // match. Reporting this attempt's rejection as terminal would tell the
        // host the submission cannot land while that candidate may still commit.
        assert!(
            matches!(&outcome, ChainLifecycleOutcome::OutcomeUnknown { known_tx_hashes, .. }
                if known_tx_hashes == &vec![TX_HASH_2.to_string()]),
            "got {outcome:?}"
        );
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// Times out the POST after recording a racing candidate, then cancels once
    /// the between-retry reconciliation looks that candidate up.
    struct AmbiguousThenCancelTransport {
        inner: MockTransport,
        db_path: String,
        cancelled: Arc<std::sync::atomic::AtomicBool>,
    }

    impl ChainTransport for AmbiguousThenCancelTransport {
        fn get<'a>(&'a self, url: &'a str, timeout: Duration) -> ChainFuture<'a> {
            self.cancelled
                .store(true, std::sync::atomic::Ordering::SeqCst);
            self.inner.get(url, timeout)
        }

        fn post_json<'a>(
            &'a self,
            url: &'a str,
            body: Vec<u8>,
            timeout: Duration,
        ) -> ChainFuture<'a> {
            let conn = rusqlite::Connection::open(&self.db_path).unwrap();
            conn.execute(
                "INSERT INTO chain_submission_attempts
                 (round_id, wallet_id, kind, bundle_index, proposal_id, batch_digest,
                  payload_digest, chain_tx_hash, state, created_at, updated_at)
                 VALUES (?1, ?2, 'delegation', 0, -1, X'', ?3, ?4, 'accepted', 1, 1)",
                rusqlite::params![ROUND_ID, WALLET, vec![0xEE_u8; 32], TX_HASH_2],
            )
            .unwrap();
            self.inner.post_json(url, body, timeout)
        }
    }

    #[tokio::test]
    async fn a_cancelled_retry_reconciliation_preserves_earlier_ambiguity() {
        let path = std::env::temp_dir().join(format!(
            "zcash_voting_retry_cancel_{}.sqlite",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let db = file_test_db(path.to_str().unwrap());
        let inner = MockTransport::default();
        // The first POST is ambiguous, and records a candidate on its way out.
        inner
            .responses
            .lock()
            .unwrap()
            .push_back(Err(ChainTransportError::Timeout));
        // The between-retry reconciliation looks that candidate up and is
        // cancelled while doing so.
        inner
            .responses
            .lock()
            .unwrap()
            .push_back(Ok(response(404, r#"{"detail":"not found"}"#)));
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let client = ChainClient::with_config(
            Arc::new(AmbiguousThenCancelTransport {
                inner,
                db_path: path.to_str().unwrap().to_string(),
                cancelled: Arc::clone(&cancelled),
            }),
            ChainEndpointSet::new(&["https://vote.example".to_string()]).unwrap(),
            ChainClientConfig::default()
                .with_retry_delays(vec![Duration::from_millis(1)])
                .unwrap(),
        );
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);
        let cancel = || cancelled.load(std::sync::atomic::Ordering::SeqCst);

        let outcome = lifecycle
            .submit_body_locked(
                WALLET,
                ChainSubmissionIdentity::delegation(ROUND_ID, 0),
                b"{}".to_vec(),
                &echo_rebuild,
                &cancel,
            )
            .await
            .unwrap();

        // The first attempt completed ambiguously and its transaction may still
        // commit. A reconciliation cancelled afterwards settled nothing, and
        // must not replace that broadcast's result with `Cancelled`.
        assert!(
            matches!(&outcome, ChainLifecycleOutcome::OutcomeUnknown { message, .. }
                if message.contains("timed out")),
            "got {outcome:?}"
        );
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// Applies a durable confirmation through its own connection while the
    /// lookup is in flight, the way another process would.
    struct ConfirmDuringLookupTransport {
        inner: MockTransport,
        db_path: String,
        applied: Mutex<bool>,
    }

    impl ChainTransport for ConfirmDuringLookupTransport {
        fn get<'a>(&'a self, url: &'a str, timeout: Duration) -> ChainFuture<'a> {
            {
                let mut applied = self.applied.lock().unwrap();
                if !*applied {
                    *applied = true;
                    let conn = rusqlite::Connection::open(&self.db_path).unwrap();
                    conn.execute(
                        "UPDATE bundles SET delegation_tx_hash=?3, van_leaf_position=5
                          WHERE round_id=?1 AND wallet_id=?2 AND bundle_index=0",
                        rusqlite::params![ROUND_ID, WALLET, TX_HASH],
                    )
                    .unwrap();
                }
            }
            self.inner.get(url, timeout)
        }

        fn post_json<'a>(
            &'a self,
            url: &'a str,
            body: Vec<u8>,
            timeout: Duration,
        ) -> ChainFuture<'a> {
            self.inner.post_json(url, body, timeout)
        }
    }

    #[tokio::test]
    async fn a_confirmation_applied_during_lookup_outranks_a_lagging_404() {
        let path = std::env::temp_dir().join(format!(
            "zcash_voting_confirm_race_{}.sqlite",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let db = file_test_db(path.to_str().unwrap());
        journal_attempt(&db, "accepted", Some(TX_HASH));
        let inner = MockTransport::default();
        inner
            .responses
            .lock()
            .unwrap()
            .push_back(Ok(response(404, r#"{"detail":"not found"}"#)));
        let client = ChainClient::new(
            Arc::new(ConfirmDuringLookupTransport {
                inner,
                db_path: path.to_str().unwrap().to_string(),
                applied: Mutex::new(false),
            }),
            ChainEndpointSet::new(&["https://vote.example".to_string()]).unwrap(),
        );
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);

        let outcome = lifecycle
            .reconcile(&ChainSubmissionIdentity::delegation(ROUND_ID, 0), &|| false)
            .await
            .unwrap();

        // The entry shortcut ran before this lookup. Reporting its 404 as
        // `Pending` would downgrade a submission that is now durably confirmed.
        assert_eq!(
            outcome,
            ChainLifecycleOutcome::AlreadyConfirmed {
                tx_hash: TX_HASH.to_string()
            }
        );
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn an_ambiguous_outcome_survives_a_failure_to_journal_it() {
        let db = test_db();
        let transport = Arc::new(MockTransport::default());
        transport
            .responses
            .lock()
            .unwrap()
            .push_back(Err(ChainTransportError::Timeout));
        let client = ChainClient::new(
            transport,
            ChainEndpointSet::new(&["https://vote.example".to_string()]).unwrap(),
        );
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);
        // Make classifying the outcome fail the way a full disk would, after the
        // request has already been dispatched.
        db.conn()
            .execute_batch(
                "CREATE TRIGGER fail_attempt_update
                 BEFORE UPDATE ON chain_submission_attempts
                 BEGIN SELECT RAISE(ABORT, 'storage failure'); END",
            )
            .unwrap();

        let outcome = lifecycle
            .submit_body_locked(
                WALLET,
                ChainSubmissionIdentity::delegation(ROUND_ID, 0),
                b"{}".to_vec(),
                &echo_rebuild,
                &|| false,
            )
            .await
            .unwrap();

        // The request may still commit and the reservation is still durably
        // `attempting`, so the ambiguity is real whether or not the
        // classification landed. Returning the storage error alone would let the
        // host read this as a definite failure.
        assert!(
            matches!(&outcome, ChainLifecycleOutcome::OutcomeUnknown { message, .. }
                if message.contains("failed to persist")),
            "got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn a_batch_confirmation_with_wrong_members_fails_over() {
        use crate::types::EncryptedShare;
        use crate::vote::{VoteBatchRecovery, VoteRecoveryBundle};

        let db = test_db();
        let mut recovery = VoteRecoveryBundle {
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
                digest: [0; 32],
                index: 0,
                size: 1,
            }),
        };
        // The stored digest must be the one the members actually hash to, or
        // recovery is rejected before any of this is reached.
        let digest = crate::vote_commitment::cast_vote_batch_sighash(
            ROUND_ID,
            recovery.anchor_height as u64,
            &[crate::vote_commitment::CastVoteBatchSighashAction {
                r_vpk: &recovery.r_vpk,
                van_nullifier: &recovery.van_nullifier,
                vote_authority_note_new: &recovery.vote_authority_note_new,
                vote_commitment: &recovery.vote_commitment,
                proposal_id: recovery.proposal_id,
            }],
        )
        .unwrap();
        recovery.batch.as_mut().unwrap().digest = digest;
        let batch_digest = digest;
        store_vote_with_recovery(&db, 1, &recovery);
        db.conn()
            .execute(
                "INSERT INTO chain_submission_attempts
                 (round_id, wallet_id, kind, bundle_index, proposal_id, batch_digest,
                  payload_digest, chain_tx_hash, state, created_at, updated_at)
                 VALUES (?1, ?2, 'vote_batch', 0, -1, ?3, ?4, ?5, 'accepted', 1, 1)",
                rusqlite::params![
                    ROUND_ID,
                    WALLET,
                    batch_digest.as_slice(),
                    vec![0xCC_u8; 32],
                    TX_HASH
                ],
            )
            .unwrap();

        let batch_events = |proposal_ids: &str, nullifiers: &str| {
            serde_json::to_string(&vec![crate::confirmation::TxEvent {
                event_type: "cast_vote_batch".to_string(),
                attributes: vec![
                    ("vote_round_id", ROUND_ID.to_string()),
                    ("batch_digest", hex::encode(batch_digest)),
                    ("batch_size", "1".to_string()),
                    ("final_van_leaf_index", "5".to_string()),
                    ("vote_commitment_leaf_indices", "7".to_string()),
                    ("proposal_ids", proposal_ids.to_string()),
                    ("van_nullifiers", nullifiers.to_string()),
                ]
                .into_iter()
                .map(|(key, value)| crate::confirmation::TxEventAttribute {
                    key: key.to_string(),
                    value,
                })
                .collect(),
            }])
            .unwrap()
        };
        let transport = Arc::new(MockTransport::default());
        transport.responses.lock().unwrap().extend([
            // Right round and digest, wrong members.
            Ok(response(
                200,
                &format!(
                    r#"{{"height":42,"code":0,"log":"","events":{}}}"#,
                    batch_events("9", &hex::encode([0x99; 32]))
                ),
            )),
            Ok(response(
                200,
                &format!(
                    r#"{{"height":42,"code":0,"log":"","events":{}}}"#,
                    batch_events("1", &hex::encode([0x10; 32]))
                ),
            )),
        ]);
        let client = ChainClient::new(
            transport,
            ChainEndpointSet::new(&[
                "https://one.example".to_string(),
                "https://two.example".to_string(),
            ])
            .unwrap(),
        );
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);

        let outcome = lifecycle
            .reconcile(
                &ChainSubmissionIdentity::vote_batch(ROUND_ID, 0, batch_digest),
                &|| false,
            )
            .await
            .unwrap();

        // The proposal and nullifier bindings live in durable recovery, so
        // checking them only in the confirmation transaction would let the first
        // endpoint's wrong members end the search while the second endpoint
        // could serve the real confirmation.
        assert!(
            matches!(&outcome, ChainLifecycleOutcome::Confirmed { tx_hash, .. }
                if tx_hash == TX_HASH),
            "got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn a_failed_candidate_is_retired_even_on_a_mixed_status_exit() {
        let db = test_db();
        journal_attempt(&db, "accepted", Some(TX_HASH));
        journal_attempt(&db, "accepted", Some(TX_HASH_2));
        let transport = Arc::new(MockTransport::default());
        transport.responses.lock().unwrap().extend([
            // One candidate is proven to have failed at commit...
            Ok(response(
                422,
                r#"{"height":42,"code":7,"log":"invalid proof","events":[]}"#,
            )),
            // ...while the other's answer is unusable, which returns first.
            Ok(response(200, "not json at all")),
        ]);
        let client = accepted_client(transport);
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);

        let outcome = lifecycle
            .reconcile(&ChainSubmissionIdentity::delegation(ROUND_ID, 0), &|| false)
            .await
            .unwrap();

        assert!(
            matches!(&outcome, ChainLifecycleOutcome::OutcomeUnknown { .. }),
            "got {outcome:?}"
        );
        // Leaving the proven failure `accepted` would have later submissions
        // rediscover it and exit before dispatch, and keep cleanup and pruning
        // pinned to a generation that can never confirm.
        assert_eq!(
            attempt_states(&db),
            vec!["rejected".to_string(), "accepted".to_string()]
        );
    }

    #[tokio::test]
    async fn cancellation_after_all_failure_retirement_is_observed() {
        let db = test_db();
        journal_attempt(&db, "accepted", Some(TX_HASH));
        let inner = MockTransport::default();
        inner.responses.lock().unwrap().push_back(Ok(response(
            422,
            r#"{"height":42,"code":7,"log":"invalid proof","events":[]}"#,
        )));
        let client = ChainClient::new(
            Arc::new(inner),
            ChainEndpointSet::new(&["https://vote.example".to_string()]).unwrap(),
        );
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);
        // Cancellation lands only after the retirement loop: the checks on
        // entry, at the loop top, inside the lookup, after the lookup, and
        // before retiring all see a live operation.
        let checks = std::sync::atomic::AtomicUsize::new(0);
        let cancel = || checks.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= 5;

        let outcome = lifecycle
            .reconcile(&ChainSubmissionIdentity::delegation(ROUND_ID, 0), &cancel)
            .await
            .unwrap();

        // Retiring is durable work that can wait on SQLite, so a cancelled
        // operation must not go on to classify the submission afterwards.
        assert_eq!(outcome, ChainLifecycleOutcome::Cancelled);
    }

    #[tokio::test]
    async fn a_confirmed_batch_member_is_not_a_singleton_confirmation() {
        use crate::types::EncryptedShare;
        use crate::vote::{VoteBatchRecovery, VoteRecoveryBundle};

        let db = test_db();
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
        store_vote_with_recovery(&db, 1, &recovery);
        // The batch confirmed, so the bundle has a VAN position and its member
        // row carries both durable fields.
        let conn = db.conn();
        conn.execute(
            "UPDATE bundles SET van_leaf_position=5
              WHERE round_id=?1 AND wallet_id=?2 AND bundle_index=0",
            rusqlite::params![ROUND_ID, WALLET],
        )
        .unwrap();
        conn.execute(
            "UPDATE votes SET tx_hash=?3, vc_tree_position=7
              WHERE round_id=?1 AND wallet_id=?2 AND bundle_index=0 AND proposal_id=1",
            rusqlite::params![ROUND_ID, WALLET, TX_HASH],
        )
        .unwrap();
        drop(conn);

        let hash =
            durable_confirmation_hash(&db, WALLET, &ChainSubmissionIdentity::vote(ROUND_ID, 0, 1))
                .unwrap();

        // Reporting the batch's transaction as confirmation of a singleton
        // submission contradicts the dispatch path, which refuses to create one
        // for a batch member at all.
        assert_eq!(hash, None);
    }

    #[tokio::test]
    async fn a_retry_reconciliation_error_preserves_earlier_ambiguity() {
        let path = std::env::temp_dir().join(format!(
            "zcash_voting_retry_error_{}.sqlite",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let db = file_test_db(path.to_str().unwrap());
        let inner = MockTransport::default();
        inner
            .responses
            .lock()
            .unwrap()
            .push_back(Err(ChainTransportError::Timeout));
        // A terminal, non-retryable lookup error for the racing candidate.
        inner
            .responses
            .lock()
            .unwrap()
            .push_back(Ok(response(401, r#"{"message":"unauthorized"}"#)));
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let client = ChainClient::with_config(
            Arc::new(AmbiguousThenCancelTransport {
                inner,
                db_path: path.to_str().unwrap().to_string(),
                cancelled,
            }),
            ChainEndpointSet::new(&["https://vote.example".to_string()]).unwrap(),
            ChainClientConfig::default()
                .with_retry_delays(vec![Duration::from_millis(1)])
                .unwrap(),
        );
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);

        let outcome = lifecycle
            .submit_body_locked(
                WALLET,
                ChainSubmissionIdentity::delegation(ROUND_ID, 0),
                b"{}".to_vec(),
                &echo_rebuild,
                &|| false,
            )
            .await
            .unwrap();

        // The lookup error is about someone else's candidate; it says nothing
        // about this call's earlier attempt, whose transaction may still commit.
        assert!(
            matches!(&outcome, ChainLifecycleOutcome::OutcomeUnknown { message, .. }
                if message.contains("dispatched without a usable response")),
            "got {outcome:?}"
        );
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn a_rejection_that_cannot_be_journaled_stays_unknown() {
        let db = test_db();
        let transport = Arc::new(MockTransport::default());
        transport.responses.lock().unwrap().push_back(Ok(response(
            422,
            r#"{"tx_hash":"","code":7,"log":"invalid proof"}"#,
        )));
        let client = accepted_client(transport);
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);
        db.conn()
            .execute_batch(
                "CREATE TRIGGER fail_attempt_update
                 BEFORE UPDATE ON chain_submission_attempts
                 BEGIN SELECT RAISE(ABORT, 'storage failure'); END",
            )
            .unwrap();

        let outcome = lifecycle
            .submit_body_locked(
                WALLET,
                ChainSubmissionIdentity::delegation(ROUND_ID, 0),
                b"{}".to_vec(),
                &echo_rebuild,
                &|| false,
            )
            .await
            .unwrap();

        // The reservation is still durably `attempting`, so this submission is
        // not settled: reporting the storage error, or the rejection as
        // terminal, would both overstate what is known.
        assert!(
            matches!(&outcome, ChainLifecycleOutcome::OutcomeUnknown { message, .. }
                if message.contains("could not be journaled")),
            "got {outcome:?}"
        );
    }

    /// Records a racing candidate on the *second* POST only, so the rejection
    /// branch is the first place that candidate is reconciled.
    struct CandidateOnSecondPostTransport {
        inner: MockTransport,
        db_path: String,
        posts: Mutex<usize>,
    }

    impl ChainTransport for CandidateOnSecondPostTransport {
        fn get<'a>(&'a self, url: &'a str, timeout: Duration) -> ChainFuture<'a> {
            self.inner.get(url, timeout)
        }

        fn post_json<'a>(
            &'a self,
            url: &'a str,
            body: Vec<u8>,
            timeout: Duration,
        ) -> ChainFuture<'a> {
            {
                let mut posts = self.posts.lock().unwrap();
                *posts += 1;
                if *posts == 2 {
                    let conn = rusqlite::Connection::open(&self.db_path).unwrap();
                    conn.execute(
                        "INSERT INTO chain_submission_attempts
                         (round_id, wallet_id, kind, bundle_index, proposal_id, batch_digest,
                          payload_digest, chain_tx_hash, state, created_at, updated_at)
                         VALUES (?1, ?2, 'delegation', 0, -1, X'', ?3, ?4, 'accepted', 1, 1)",
                        rusqlite::params![ROUND_ID, WALLET, vec![0xEE_u8; 32], TX_HASH_2],
                    )
                    .unwrap();
                }
            }
            self.inner.post_json(url, body, timeout)
        }
    }

    #[tokio::test]
    async fn a_rejection_path_lookup_error_preserves_earlier_ambiguity() {
        let path = std::env::temp_dir().join(format!(
            "zcash_voting_reject_lookup_err_{}.sqlite",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let db = file_test_db(path.to_str().unwrap());
        let inner = MockTransport::default();
        inner.responses.lock().unwrap().extend([
            // Attempt one is ambiguous.
            Err(ChainTransportError::Timeout),
            // Attempt two is definitely rejected, and records a candidate.
            Ok(response(
                422,
                r#"{"tx_hash":"","code":7,"log":"invalid proof"}"#,
            )),
            // Reconciling that candidate fails terminally.
            Ok(response(401, r#"{"message":"unauthorized"}"#)),
        ]);
        let client = ChainClient::with_config(
            Arc::new(CandidateOnSecondPostTransport {
                inner,
                db_path: path.to_str().unwrap().to_string(),
                posts: Mutex::new(0),
            }),
            ChainEndpointSet::new(&["https://vote.example".to_string()]).unwrap(),
            ChainClientConfig::default()
                .with_retry_delays(vec![Duration::from_millis(1)])
                .unwrap(),
        );
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);

        let outcome = lifecycle
            .submit_body_locked(
                WALLET,
                ChainSubmissionIdentity::delegation(ROUND_ID, 0),
                b"{}".to_vec(),
                &echo_rebuild,
                &|| false,
            )
            .await
            .unwrap();

        // The lookup error says nothing about the first attempt, whose
        // transaction may still commit.
        assert!(
            matches!(&outcome, ChainLifecycleOutcome::OutcomeUnknown { message, .. }
                if message.contains("dispatched without a usable response")),
            "got {outcome:?}"
        );
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn a_failed_unsent_cleanup_preserves_earlier_ambiguity() {
        let db = test_db();
        let transport = Arc::new(MockTransport::default());
        transport.responses.lock().unwrap().extend([
            // Attempt one is ambiguous.
            Err(ChainTransportError::Timeout),
            // Attempt two fails definitely before dispatch, so its reservation
            // is removed — and that removal cannot be written.
            Err(ChainTransportError::Transport(
                "connect refused".to_string(),
            )),
        ]);
        let client = ChainClient::with_config(
            transport,
            ChainEndpointSet::new(&["https://vote.example".to_string()]).unwrap(),
            ChainClientConfig::default()
                .with_retry_delays(vec![Duration::from_millis(1)])
                .unwrap(),
        );
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);
        db.conn()
            .execute_batch(
                "CREATE TRIGGER fail_attempt_delete
                 BEFORE DELETE ON chain_submission_attempts
                 BEGIN SELECT RAISE(ABORT, 'storage failure'); END",
            )
            .unwrap();

        let outcome = lifecycle
            .submit_body_locked(
                WALLET,
                ChainSubmissionIdentity::delegation(ROUND_ID, 0),
                b"{}".to_vec(),
                &echo_rebuild,
                &|| false,
            )
            .await
            .unwrap();

        // Failing to tidy up a reservation that was never sent says nothing
        // about the first attempt, which may still commit.
        assert!(
            matches!(&outcome, ChainLifecycleOutcome::OutcomeUnknown { message, .. }
                if message.contains("timed out")),
            "got {outcome:?}"
        );
    }

    #[test]
    fn in_memory_ambiguity_survives_an_unreadable_database() {
        let db = test_db();
        let transport = Arc::new(MockTransport::default());
        let client = accepted_client(transport);
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);
        let identity = ChainSubmissionIdentity::delegation(ROUND_ID, 0);
        // The journal is gone, so every supplementary read below fails.
        db.conn()
            .execute_batch("DROP TABLE chain_submission_attempts")
            .unwrap();

        let outcome = lifecycle
            .outstanding_ambiguity(
                WALLET,
                &identity,
                &Some("request timed out".to_string()),
                "storage failed",
            )
            .unwrap();

        // A broadcast this call already completed is in-memory evidence that
        // needs no storage read to be true, and a database that has become
        // unreadable is one of the things that gets us here. Losing it would let
        // the host treat a generation as safe to replace.
        assert!(
            matches!(&outcome, Some(ChainLifecycleOutcome::OutcomeUnknown { message, .. })
                if message.contains("timed out")),
            "got {outcome:?}"
        );
        // With nothing in memory there is no evidence to preserve, so the read
        // failure is reported rather than invented around.
        assert!(lifecycle
            .outstanding_ambiguity(WALLET, &identity, &None, "storage failed")
            .is_err());
    }

    #[tokio::test]
    async fn a_spent_nullifier_with_an_unsettled_lookup_stays_unknown() {
        let path = std::env::temp_dir().join(format!(
            "zcash_voting_spent_unsettled_{}.sqlite",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let db = file_test_db(path.to_str().unwrap());
        {
            let conn = db.conn();
            queries::store_vote(&conn, ROUND_ID, WALLET, 0, 3, 1, &[0xCC; 32]).unwrap();
        }
        let inner = MockTransport::default();
        inner.responses.lock().unwrap().extend([
            Ok(response(
                422,
                r#"{"tx_hash":"","code":9,"log":"nullifier already spent: abcd"}"#,
            )),
            // The candidate's own lookup cannot be read, so nothing is settled.
            Ok(response(200, "not json at all")),
        ]);
        let client = ChainClient::new(
            Arc::new(RacingTransport {
                inner,
                db_path: path.to_str().unwrap().to_string(),
                raced: Mutex::new(false),
            }),
            ChainEndpointSet::new(&["https://vote.example".to_string()]).unwrap(),
        );
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);

        let outcome = lifecycle
            .submit_body_locked(
                WALLET,
                ChainSubmissionIdentity::vote(ROUND_ID, 0, 3),
                b"{}".to_vec(),
                &echo_rebuild,
                &|| false,
            )
            .await
            .unwrap();

        // `AlreadySpentUnresolved` asserts the known candidates were checked and
        // none had succeeded. This lookup checked nothing, so saying so would
        // state as fact something the call never established.
        assert!(
            matches!(&outcome, ChainLifecycleOutcome::OutcomeUnknown { known_tx_hashes, .. }
                if known_tx_hashes == &vec![TX_HASH_2.to_string()]),
            "got {outcome:?}"
        );
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn a_spent_nullifier_cancelled_mid_lookup_is_not_reported_unresolved() {
        let path = std::env::temp_dir().join(format!(
            "zcash_voting_spent_cancel_{}.sqlite",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let db = file_test_db(path.to_str().unwrap());
        let inner = MockTransport::default();
        inner.responses.lock().unwrap().extend([
            Ok(response(
                422,
                r#"{"tx_hash":"","code":9,"log":"nullifier already spent: abcd"}"#,
            )),
            Ok(response(404, r#"{"detail":"not found"}"#)),
        ]);
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let client = ChainClient::new(
            Arc::new(RacingThenCancelTransport {
                inner,
                db_path: path.to_str().unwrap().to_string(),
                cancelled: Arc::clone(&cancelled),
            }),
            ChainEndpointSet::new(&["https://vote.example".to_string()]).unwrap(),
        );
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);
        let cancel = || cancelled.load(std::sync::atomic::Ordering::SeqCst);

        let outcome = lifecycle
            .submit_body_locked(
                WALLET,
                ChainSubmissionIdentity::delegation(ROUND_ID, 0),
                b"{}".to_vec(),
                &echo_rebuild,
                &cancel,
            )
            .await
            .unwrap();

        // Nothing was dispatched that may still commit and the candidates were
        // never checked, so the honest answer is that the operation stopped.
        assert_eq!(outcome, ChainLifecycleOutcome::Cancelled);
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn a_committed_failure_yields_to_a_candidate_journaled_during_the_lookup() {
        let path = std::env::temp_dir().join(format!(
            "zcash_voting_failure_race_{}.sqlite",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let db = file_test_db(path.to_str().unwrap());
        journal_attempt(&db, "accepted", Some(TX_HASH));

        /// Journals a second candidate while the first one is being looked up.
        struct JournalDuringLookup {
            inner: MockTransport,
            db_path: String,
            done: Mutex<bool>,
        }
        impl ChainTransport for JournalDuringLookup {
            fn get<'a>(&'a self, url: &'a str, timeout: Duration) -> ChainFuture<'a> {
                {
                    let mut done = self.done.lock().unwrap();
                    if !*done {
                        *done = true;
                        let conn = rusqlite::Connection::open(&self.db_path).unwrap();
                        conn.execute(
                            "INSERT INTO chain_submission_attempts
                             (round_id, wallet_id, kind, bundle_index, proposal_id, batch_digest,
                              payload_digest, chain_tx_hash, state, created_at, updated_at)
                             VALUES (?1, ?2, 'delegation', 0, -1, X'', ?3, ?4, 'accepted', 1, 1)",
                            rusqlite::params![ROUND_ID, WALLET, vec![0xEE_u8; 32], TX_HASH_2],
                        )
                        .unwrap();
                    }
                }
                self.inner.get(url, timeout)
            }
            fn post_json<'a>(
                &'a self,
                url: &'a str,
                body: Vec<u8>,
                timeout: Duration,
            ) -> ChainFuture<'a> {
                self.inner.post_json(url, body, timeout)
            }
        }

        let inner = MockTransport::default();
        inner.responses.lock().unwrap().push_back(Ok(response(
            422,
            r#"{"height":42,"code":7,"log":"invalid proof","events":[]}"#,
        )));
        let client = ChainClient::new(
            Arc::new(JournalDuringLookup {
                inner,
                db_path: path.to_str().unwrap().to_string(),
                done: Mutex::new(false),
            }),
            ChainEndpointSet::new(&["https://vote.example".to_string()]).unwrap(),
        );
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);

        let outcome = lifecycle
            .reconcile(&ChainSubmissionIdentity::delegation(ROUND_ID, 0), &|| false)
            .await
            .unwrap();

        // The failure is definite only for the candidate it names. Retirement
        // marked that one `rejected`, so what `known_hashes` still returns is a
        // transaction nobody has disproved — and it arrives `accepted`, which
        // the live-attempt query does not match.
        assert!(
            matches!(&outcome, ChainLifecycleOutcome::OutcomeUnknown { known_tx_hashes, .. }
                if known_tx_hashes == &vec![TX_HASH_2.to_string()]),
            "got {outcome:?}"
        );
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn a_retired_candidate_is_not_reported_as_pending() {
        let db = test_db();
        journal_attempt(&db, "accepted", Some(TX_HASH));
        journal_attempt(&db, "accepted", Some(TX_HASH_2));
        let transport = Arc::new(MockTransport::default());
        transport.responses.lock().unwrap().extend([
            // One candidate is proven to have failed at commit...
            Ok(response(
                422,
                r#"{"height":42,"code":7,"log":"invalid proof","events":[]}"#,
            )),
            // ...while the other is genuinely not yet committed.
            Ok(response(404, r#"{"detail":"not found"}"#)),
        ]);
        let client = accepted_client(transport);
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);

        let outcome = lifecycle
            .reconcile(&ChainSubmissionIdentity::delegation(ROUND_ID, 0), &|| false)
            .await
            .unwrap();

        // The retired candidate is no longer one the journal will offer, so
        // handing it back would have the host keep polling a transaction this
        // call just proved had failed.
        assert_eq!(
            outcome,
            ChainLifecycleOutcome::Pending {
                known_tx_hashes: vec![TX_HASH_2.to_string()],
            }
        );
        assert_eq!(
            attempt_states(&db),
            vec!["rejected".to_string(), "accepted".to_string()]
        );
    }

    #[tokio::test]
    async fn a_terminal_lookup_error_does_not_downgrade_durable_ambiguity() {
        let db = test_db();
        // An earlier retry left this behind: dispatched, never classified.
        journal_attempt(&db, "outcome_unknown", None);
        journal_attempt(&db, "accepted", Some(TX_HASH));
        let transport = Arc::new(MockTransport::default());
        transport
            .responses
            .lock()
            .unwrap()
            .push_back(Ok(response(401, r#"{"message":"unauthorized"}"#)));
        let client = accepted_client(transport);
        let lifecycle = ChainSubmissionLifecycle::new(&db, &client);

        let outcome = lifecycle
            .reconcile(&ChainSubmissionIdentity::delegation(ROUND_ID, 0), &|| false)
            .await
            .unwrap();

        // `reconcile` reaches this exit directly, with none of the submission
        // loop's ambiguity handling around it. A lookup that could not be
        // completed is an absence of evidence, not evidence of absence, so it
        // must not outrank a dispatched request that may still commit.
        assert!(
            matches!(&outcome, ChainLifecycleOutcome::OutcomeUnknown { known_tx_hashes, .. }
                if known_tx_hashes == &vec![TX_HASH.to_string()]),
            "got {outcome:?}"
        );
    }

    #[test]
    fn every_journaled_candidate_survives_for_one_submission() {
        let db = test_db();
        {
            let conn = db.conn();
            queries::store_vote(&conn, ROUND_ID, WALLET, 0, 3, 1, &[0xCC; 32]).unwrap();
        }
        // Two processes were each accepted with a different hash for the same
        // vote. Keeping only the newest would have a host poll the one it kept
        // while the one it dropped commits.
        for hash in [TX_HASH, TX_HASH_2] {
            db.conn()
                .execute(
                    "INSERT INTO chain_submission_attempts
                     (round_id, wallet_id, kind, bundle_index, proposal_id, batch_digest,
                      payload_digest, chain_tx_hash, state, created_at, updated_at)
                     VALUES (?1, ?2, 'vote', 0, 3, X'', ?3, ?4, 'accepted', 1, 1)",
                    rusqlite::params![ROUND_ID, WALLET, vec![0xCC_u8; 32], hash],
                )
                .unwrap();
        }

        let conn = db.conn();
        let candidates = vote_candidates(&conn, ROUND_ID, WALLET, 0, 3).unwrap();

        // The same set `known_hashes` reconciles.
        assert_eq!(candidates, vec![TX_HASH.to_string(), TX_HASH_2.to_string()]);
    }
}
