//! Submission and reconciliation orchestration.

use sha2::{Digest, Sha256};

use super::{
    candidate_transaction_hashes, canonical_batch_payload, canonical_singleton_vote_payload,
    decode_canonical_array, delegation_payload_rebuild, delete_definitely_unsent_attempt,
    durable_confirmation_hash, has_ambiguous_attempt, identity_operation_lock,
    journal_accepted_hash, outcome_blocks_dispatch, record_attempt_evidence,
    refresh_attempt_reservation, reserve_dispatch_attempt, ChainLifecycleError,
    ChainLifecycleOutcome, ChainSubmissionIdentity, ChainSubmissionLifecycle, InFlightAttempt,
    PayloadRebuild, RESERVATION_HEARTBEAT,
};
use crate::{
    chain::{is_spent_nullifier_log, ChainBroadcastOutcome, ChainClient, ChainError},
    delegate::{self, DelegationSigner, SignedDelegationBundle},
    storage::{queries, VotingDb},
    types::VotingError,
    vote,
    wire::DelegationSubmissionWire,
};

impl<'a> ChainSubmissionLifecycle<'a> {
    /// Creates a lifecycle over one voting database and configured chain client.
    pub fn new(db: &'a VotingDb, client: &'a ChainClient) -> Self {
        Self { db, client }
    }

    /// Validates and submits an SDK-native signed delegation bundle.
    pub async fn submit_delegation(
        &self,
        bundle: &SignedDelegationBundle,
        cancel: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<ChainLifecycleOutcome, ChainLifecycleError> {
        let round_id = bundle.submission.vote_round_id.clone();
        let identity = ChainSubmissionIdentity::delegation(&round_id, bundle.bundle_index);
        let wallet_id = self.db.wallet_id();
        let lock = identity_operation_lock(&identity.lock_key(&wallet_id))?;
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
        let canonical_payload = wire.to_json()?.into_bytes();
        let rebuild = delegation_payload_rebuild(
            wallet_id.clone(),
            round_id.clone(),
            bundle.bundle_index,
            bundle.submission.spend_auth_sig,
            bundle.submission.sighash,
        );
        self.submit_canonical_payload_locked(
            &wallet_id,
            identity,
            canonical_payload,
            &rebuild,
            cancel,
        )
        .await
    }

    /// Validates and submits an FFI-safe delegation wire value.
    ///
    /// The wire value must match the exact persisted delegation generation.
    pub async fn submit_delegation_wire(
        &self,
        round_id: &str,
        bundle_index: u32,
        wire: &DelegationSubmissionWire,
        cancel: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<ChainLifecycleOutcome, ChainLifecycleError> {
        let identity = ChainSubmissionIdentity::delegation(round_id, bundle_index);
        let wallet_id = self.db.wallet_id();
        let lock = identity_operation_lock(&identity.lock_key(&wallet_id))?;
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
        let canonical_payload = expected.to_json()?.into_bytes();
        let rebuild = delegation_payload_rebuild(
            wallet_id.clone(),
            round_id.to_string(),
            bundle_index,
            signature,
            sighash,
        );
        self.submit_canonical_payload_locked(
            &wallet_id,
            identity,
            canonical_payload,
            &rebuild,
            cancel,
        )
        .await
    }

    /// Recovers and submits one persisted singleton vote generation.
    ///
    /// A proposal recorded as a member of an atomic batch is refused.
    pub async fn submit_vote(
        &self,
        round_id: &str,
        bundle_index: u32,
        proposal_id: u32,
        cancel: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<ChainLifecycleOutcome, ChainLifecycleError> {
        let identity = ChainSubmissionIdentity::vote(round_id, bundle_index, proposal_id);
        let wallet_id = self.db.wallet_id();
        let lock = identity_operation_lock(&identity.lock_key(&wallet_id))?;
        let _guard = lock.lock().await;
        // `canonical_singleton_vote_payload` loads and validates the exact recovery
        // generation and refuses a member of a persisted atomic batch, so the
        // outer canonical_payload and the reservation rebuild share one implementation.
        let canonical_payload = {
            let conn = self.db.conn();
            canonical_singleton_vote_payload(
                &conn,
                &wallet_id,
                round_id,
                bundle_index,
                proposal_id,
            )?
        };
        let rebuild_wallet_id = wallet_id.clone();
        let owned_round_id = round_id.to_string();
        let rebuild = move |conn: &rusqlite::Connection| -> Result<Vec<u8>, VotingError> {
            canonical_singleton_vote_payload(
                conn,
                &rebuild_wallet_id,
                &owned_round_id,
                bundle_index,
                proposal_id,
            )
        };
        self.submit_canonical_payload_locked(
            &wallet_id,
            identity,
            canonical_payload,
            &rebuild,
            cancel,
        )
        .await
    }

    /// Recovers and submits one persisted atomic vote batch.
    ///
    /// `batch_member_proposal_id` locates the persisted batch; the batch digest
    /// recovered from storage is its durable submission identity.
    pub async fn submit_vote_batch(
        &self,
        round_id: &str,
        bundle_index: u32,
        batch_member_proposal_id: u32,
        cancel: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<ChainLifecycleOutcome, ChainLifecycleError> {
        let lock_identity = ChainSubmissionIdentity::vote_batch(round_id, bundle_index, [0; 32]);
        let wallet_id = self.db.wallet_id();
        let lock = identity_operation_lock(&lock_identity.lock_key(&wallet_id))?;
        let _guard = lock.lock().await;
        let batch = {
            let conn = self.db.conn();
            vote::recover_atomic_vote_batch_with_conn(
                &conn,
                &wallet_id,
                round_id,
                bundle_index,
                batch_member_proposal_id,
            )?
        };
        let canonical_payload = canonical_batch_payload(&batch.batch_json)?;
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
                batch_member_proposal_id,
            )?;
            canonical_batch_payload(&batch.batch_json)
        };
        self.submit_canonical_payload_locked(
            &wallet_id,
            identity,
            canonical_payload,
            &rebuild,
            cancel,
        )
        .await
    }

    /// Reconciles every known candidate for one submission without broadcasting.
    pub async fn reconcile(
        &self,
        identity: &ChainSubmissionIdentity,
        cancel: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<ChainLifecycleOutcome, ChainLifecycleError> {
        let wallet_id = self.db.wallet_id();
        let lock = identity_operation_lock(&identity.lock_key(&wallet_id))?;
        let _guard = lock.lock().await;
        self.reconcile_submission_locked(&wallet_id, identity, cancel)
            .await
    }

    /// Cancellation's outcome for this call.
    ///
    /// Cancellation observed after a broadcast completes does not replace that
    /// broadcast's result, so a dispatched attempt that may still commit is
    /// reported as `OutcomeUnknown`. `Cancelled` is reserved for calls with no
    /// completed ambiguous broadcast.
    fn outcome_after_cancellation(
        &self,
        wallet_id: &str,
        identity: &ChainSubmissionIdentity,
        last_unknown: &Option<String>,
    ) -> Result<ChainLifecycleOutcome, VotingError> {
        if last_unknown.is_some() || has_ambiguous_attempt(self.db, wallet_id, identity)? {
            return Ok(ChainLifecycleOutcome::OutcomeUnknown {
                // Supplementary detail. The decision to report ambiguity is
                // already made, so failing to list the candidates must not undo
                // it; see `ambiguity_overriding_failure`.
                known_tx_hashes: candidate_transaction_hashes(self.db, wallet_id, identity)
                    .unwrap_or_default(),
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
    pub(super) fn ambiguity_overriding_failure(
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
            let known_tx_hashes = candidate_transaction_hashes(self.db, wallet_id, identity)?;
            if known_tx_hashes.is_empty() && !has_ambiguous_attempt(self.db, wallet_id, identity)? {
                return Ok(None);
            }
            return Ok(Some(ChainLifecycleOutcome::OutcomeUnknown {
                known_tx_hashes,
                message: format!(
                    "an earlier attempt may still commit; this one did not settle: {cause}"
                ),
            }));
        }
        let known_tx_hashes =
            candidate_transaction_hashes(self.db, wallet_id, identity).unwrap_or_default();
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

    pub(super) async fn submit_canonical_payload_locked(
        &self,
        wallet_id: &str,
        identity: ChainSubmissionIdentity,
        canonical_payload: Vec<u8>,
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
            .run_submission_attempts(
                wallet_id,
                &identity,
                canonical_payload,
                rebuild,
                cancel,
                &mut last_unknown,
            )
            .await
        {
            // A durable confirmation another writer applied while this call was
            // in flight outranks every "still waiting" answer the loop can
            // reach, and there are several of those. Checked once here rather
            // than at each, for the same reason the ambiguity handling below is.
            Ok(outcome)
                if matches!(
                    outcome,
                    ChainLifecycleOutcome::OutcomeUnknown { .. }
                        | ChainLifecycleOutcome::Pending { .. }
                        // Acceptance is not commitment, so it too is weaker than
                        // a confirmation another writer has already applied.
                        // Reporting it would send the host polling a submission
                        // that is durably complete; the hash this call learned
                        // stays in the journal either way.
                        | ChainLifecycleOutcome::Accepted { .. }
                ) =>
            {
                // Supplementary, like the candidate list in
                // `ambiguity_overriding_failure`: this outcome is already established
                // in memory, so a database that has become unreadable must not
                // be able to discard an accepted hash or a completed
                // broadcast's ambiguity on the way out.
                match durable_confirmation_hash(self.db, wallet_id, &identity) {
                    Ok(Some(tx_hash)) => Ok(ChainLifecycleOutcome::AlreadyConfirmed { tx_hash }),
                    Ok(None) | Err(_) => Ok(outcome),
                }
            }
            Ok(outcome) => Ok(outcome),
            // The exception: this one already carries the accepted hash, which
            // is the only handle on a transaction in the mempool and strictly
            // more than the ambiguity it implies.
            Err(error @ ChainLifecycleError::AcceptedButUnjournaled { .. }) => Err(error),
            Err(error) => {
                match self.ambiguity_overriding_failure(
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
    async fn run_submission_attempts(
        &self,
        wallet_id: &str,
        identity: &ChainSubmissionIdentity,
        canonical_payload: Vec<u8>,
        rebuild: PayloadRebuild<'_>,
        cancel: &(dyn Fn() -> bool + Send + Sync),
        last_unknown: &mut Option<String>,
    ) -> Result<ChainLifecycleOutcome, ChainLifecycleError> {
        let existing = self
            .reconcile_submission_locked(wallet_id, identity, cancel)
            .await?;
        if outcome_blocks_dispatch(&existing) {
            return Ok(existing);
        }

        let digest: [u8; 32] = Sha256::digest(&canonical_payload).into();
        let attempts = self.client.retry_delays().len() + 1;
        for attempt_index in 0..attempts {
            if cancel() {
                return Ok(self.outcome_after_cancellation(wallet_id, identity, last_unknown)?);
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
                let reconciled = match self
                    .reconcile_submission_locked(wallet_id, identity, cancel)
                    .await
                {
                    Ok(reconciled) => reconciled,
                    Err(error) => {
                        if let Some(outcome) = self.ambiguity_overriding_failure(
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
                // result. `outcome_after_cancellation` reports the earlier ambiguity when
                // there is one and `Cancelled` only when there is not.
                if matches!(reconciled, ChainLifecycleOutcome::Cancelled) {
                    return Ok(self.outcome_after_cancellation(
                        wallet_id,
                        identity,
                        last_unknown,
                    )?);
                }
                if outcome_blocks_dispatch(&reconciled) {
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
            let in_flight = InFlightAttempt::register(wallet_id, identity);
            // A failure here is definite only for this attempt. An earlier one
            // in this call may still commit, and a concurrent change to the
            // now-uncovered generation is exactly what makes this rebuild fail
            // as stale — so reporting the persistence error alone would hide the
            // ambiguity and invite the host to treat the replacement as safe to
            // submit.
            let attempt_id =
                match reserve_dispatch_attempt(self.db, wallet_id, identity, &digest, rebuild) {
                    Ok(Some(attempt_id)) => attempt_id,
                    // A candidate appeared between the preflight and this
                    // reservation. Report what it settles rather than adding a
                    // duplicate broadcast to a submission that already has one — and
                    // as at the other reconciliation gates, a cancelled
                    // reconciliation settled nothing and must not replace an earlier
                    // broadcast's ambiguity.
                    Ok(None) => {
                        let reconciled = self
                            .reconcile_submission_locked(wallet_id, identity, cancel)
                            .await?;
                        if matches!(reconciled, ChainLifecycleOutcome::Cancelled) {
                            return Ok(self.outcome_after_cancellation(
                                wallet_id,
                                identity,
                                last_unknown,
                            )?);
                        }
                        return Ok(reconciled);
                    }
                    Err(error) => {
                        if let Some(outcome) = self.ambiguity_overriding_failure(
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
                delete_definitely_unsent_attempt(self.db, wallet_id, attempt_id)?;
                return Ok(self.outcome_after_cancellation(wallet_id, identity, last_unknown)?);
            }

            let result = {
                // While the POST is outstanding, refresh the reservation's
                // `updated_at`. Without this the column means "when this was
                // reserved", and the age test downstream would be reading it as
                // "the owner was alive at this time" — the two diverge exactly
                // when the wall clock steps forward, and a reader in another
                // process, which cannot see this process's in-memory registry,
                // has nothing else to go on.
                let post = self.client.post_once(
                    attempt_index,
                    identity.kind().endpoint(),
                    canonical_payload.clone(),
                );
                tokio::pin!(post);
                loop {
                    tokio::select! {
                        outcome = &mut post => break outcome,
                        _ = tokio::time::sleep(RESERVATION_HEARTBEAT) => {
                            refresh_attempt_reservation(self.db, wallet_id, attempt_id);
                        }
                    }
                }
            };
            match result {
                Ok(ChainBroadcastOutcome::Accepted(result)) => {
                    // A hash another submission already owns cannot become this
                    // one's candidate, and the check and the journaling share
                    // one transaction: read separately, two submissions handed
                    // the same hash could each find it free before either wrote.
                    //
                    // The hash must not go down with a storage failure either:
                    // it is the only handle anything will ever have on a
                    // transaction that is already in the mempool.
                    let conflict = match journal_accepted_hash(
                        self.db,
                        wallet_id,
                        identity,
                        attempt_id,
                        &result.tx_hash,
                    ) {
                        Ok(conflict) => conflict,
                        Err(error) => {
                            return Err(ChainLifecycleError::AcceptedButUnjournaled {
                                tx_hash: result.tx_hash,
                                source: error,
                            })
                        }
                    };
                    if let Some(conflict) = conflict {
                        // The POST still happened, so the attempt keeps its
                        // ambiguity rather than its answer: hashless
                        // `outcome_unknown` says "a transaction may be in flight
                        // and its hash is not known", which is exactly true.
                        let message = format!(
                            "vote chain accepted a transaction under a hash that belongs to another submission, so it cannot be a candidate here: {conflict}"
                        );
                        *last_unknown = Some(message.clone());
                        return Ok(ChainLifecycleOutcome::OutcomeUnknown {
                            known_tx_hashes: candidate_transaction_hashes(
                                self.db, wallet_id, identity,
                            )
                            .unwrap_or_default(),
                            message,
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
                    let journal_error = record_attempt_evidence(
                        self.db, wallet_id, attempt_id, "rejected", tx_hash,
                    )
                    .err();
                    if is_spent_nullifier_log(&result.log) {
                        let reconciled = self
                            .reconcile_submission_locked(wallet_id, identity, cancel)
                            .await?;
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
                                return Ok(self.outcome_after_cancellation(
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
                            known_tx_hashes: candidate_transaction_hashes(
                                self.db, wallet_id, identity,
                            )?,
                            log: result.log,
                        });
                    }
                    // A candidate hash another writer recorded — a concurrent
                    // operation's `accepted` attempt, or a legacy domain hash —
                    // may still commit, and `has_ambiguous_attempt` cannot see it:
                    // `accepted` is neither `attempting` nor `outcome_unknown`.
                    // Settle it on the network before reporting a rejection that
                    // is definite only for this attempt's own payload.
                    if !candidate_transaction_hashes(self.db, wallet_id, identity)?.is_empty() {
                        let reconciled = self
                            .reconcile_submission_locked(wallet_id, identity, cancel)
                            .await?;
                        // `Cancelled` is excluded deliberately: cancellation
                        // observed after this broadcast completed must not
                        // replace its result, and the checks below still have to
                        // classify it.
                        if !matches!(reconciled, ChainLifecycleOutcome::Cancelled)
                            && outcome_blocks_dispatch(&reconciled)
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
                    // an attempt too, and `has_ambiguous_attempt` cannot see it:
                    // `accepted` is neither `attempting` nor `outcome_unknown`.
                    // Re-read rather than reuse the set from before, because a
                    // committed failure there retires its candidates. This also
                    // covers a reconciliation that was cancelled and so reached
                    // no conclusion at all.
                    let known_tx_hashes =
                        candidate_transaction_hashes(self.db, wallet_id, identity)?;
                    if last_unknown.is_some()
                        || !known_tx_hashes.is_empty()
                        || has_ambiguous_attempt(self.db, wallet_id, identity)?
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
                    if let Err(error) = record_attempt_evidence(
                        self.db,
                        wallet_id,
                        attempt_id,
                        "outcome_unknown",
                        None,
                    ) {
                        return Ok(ChainLifecycleOutcome::OutcomeUnknown {
                            known_tx_hashes: candidate_transaction_hashes(self.db, wallet_id, identity)
                                .unwrap_or_default(),
                            message: format!(
                                "{message}; classifying that attempt also failed to persist: {error}"
                            ),
                        });
                    }
                }
                Ok(ChainBroadcastOutcome::Cancelled) => {
                    record_attempt_evidence(
                        self.db,
                        wallet_id,
                        attempt_id,
                        "outcome_unknown",
                        None,
                    )?;
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
                        delete_definitely_unsent_attempt(self.db, wallet_id, attempt_id)?;
                    } else if error.is_ambiguous() {
                        // As above: the reservation stays durably `attempting`,
                        // so a failed classification must not turn a possibly
                        // committed request into a definite error.
                        if let Err(persist) = record_attempt_evidence(
                            self.db,
                            wallet_id,
                            attempt_id,
                            "outcome_unknown",
                            None,
                        ) {
                            return Ok(ChainLifecycleOutcome::OutcomeUnknown {
                                known_tx_hashes: candidate_transaction_hashes(self.db, wallet_id, identity)
                                    .unwrap_or_default(),
                                message: format!(
                                    "{error}; classifying that attempt also failed to persist: {persist}"
                                ),
                            });
                        }
                    } else {
                        record_attempt_evidence(self.db, wallet_id, attempt_id, "rejected", None)?;
                    }
                    if !error.is_retryable() || attempt_index + 1 == attempts {
                        // As above: a definite failure here cannot disprove an
                        // earlier attempt that may still commit, and a candidate
                        // another writer recorded is such an attempt even though
                        // `has_ambiguous_attempt` cannot see its `accepted` state.
                        if error.is_ambiguous()
                            || last_unknown.is_some()
                            || has_ambiguous_attempt(self.db, wallet_id, identity)?
                            || !candidate_transaction_hashes(self.db, wallet_id, identity)?
                                .is_empty()
                        {
                            let message = match &*last_unknown {
                                Some(earlier) => format!(
                                    "an earlier attempt's outcome is unknown ({earlier}); a later attempt failed: {error}"
                                ),
                                None => error.to_string(),
                            };
                            return Ok(ChainLifecycleOutcome::OutcomeUnknown {
                                known_tx_hashes: candidate_transaction_hashes(
                                    self.db, wallet_id, identity,
                                )?,
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

            // This attempt's response is classified, so no POST of ours is
            // outstanding. Holding the guard across the backoff would keep
            // asserting coverage that cleanup, ballot-intent changes, and bundle
            // pruning all obey — for as long as the configured delay, which the
            // host chooses.
            drop(in_flight);
            if attempt_index + 1 < attempts {
                if cancel() {
                    return Ok(self.outcome_after_cancellation(
                        wallet_id,
                        identity,
                        last_unknown,
                    )?);
                }
                tokio::time::sleep(self.client.retry_delays()[attempt_index]).await;
            }
        }

        // Another writer can apply the confirmation while this call's last POST
        // is in flight. The lookup path rereads the durable state before
        // reporting anything weaker; this exit has to as well, or a completed
        // submission is handed back as unresolved and the host keeps recovering
        // it.
        if let Some(tx_hash) = durable_confirmation_hash(self.db, wallet_id, identity)? {
            return Ok(ChainLifecycleOutcome::AlreadyConfirmed { tx_hash });
        }
        Ok(ChainLifecycleOutcome::OutcomeUnknown {
            known_tx_hashes: candidate_transaction_hashes(self.db, wallet_id, identity)?,
            message: last_unknown
                .clone()
                .unwrap_or_else(|| "submission outcome is unknown".to_string()),
        })
    }
}
