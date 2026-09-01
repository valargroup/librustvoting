//! Candidate lookup, confirmation binding, and evidence classification.

use rusqlite::OptionalExtension;

use super::{
    candidate_transaction_hashes, has_ambiguous_attempt, internal, retire_failed_candidate,
    ChainConfirmation, ChainLifecycleError, ChainLifecycleOutcome, ChainSubmissionIdentity,
    ChainSubmissionKind, ChainSubmissionLifecycle,
};
use crate::{
    chain::{ChainError, ChainTxConfirmation, ChainTxStatus},
    confirmation::{
        confirm_delegation_submission_for_wallet, confirm_vote_batch_submission_for_wallet,
        confirm_vote_submission_for_wallet, StoredHashConflict,
    },
    storage::{queries, VotingDb},
    types::VotingError,
    vote,
};

impl<'a> ChainSubmissionLifecycle<'a> {
    pub(super) async fn reconcile_submission_locked(
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
        let durable = durable_confirmation_hash(self.db, wallet_id, identity)?;
        // That read waits on the database mutex, and everything below it
        // classifies the submission. One check here covers the whole chain.
        if cancel() {
            return Ok(ChainLifecycleOutcome::Cancelled);
        }
        if let Some(tx_hash) = durable {
            return Ok(ChainLifecycleOutcome::AlreadyConfirmed { tx_hash });
        }
        let hashes = candidate_transaction_hashes(self.db, wallet_id, identity)?;
        if hashes.is_empty() {
            // A timeout or an unusable accepted response leaves an
            // `outcome_unknown` attempt with no hash, and an interruption leaves
            // an `attempting` one. Both mean a transaction may still commit, and
            // that evidence has to survive across calls: reporting `Pending`
            // would let a later call return a terminal rejection.
            let live = has_ambiguous_attempt(self.db, wallet_id, identity)?;
            // Both of those reads wait on the database, and this path has no
            // lookup loop after them to check again. Reporting either answer
            // says the operation is active, which a cancelled one is not.
            if cancel() {
                return Ok(ChainLifecycleOutcome::Cancelled);
            }
            if live {
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
        // Checked after retirement, not before: this exit applies neither
        // confirmation, so a proven failure left live here would block a
        // replacement forever on the one path that already knows the chain
        // returned something impossible.
        if successful.len() > 1 {
            return Err(VotingError::Internal {
                message: "multiple chain candidates committed successfully for one submission"
                    .to_string(),
            }
            .into());
        }
        // The set read before the lookups is stale in both directions by now:
        // retirement above marked the proven failures `rejected`, and another
        // writer can journal a candidate while the requests are running. Rebuild
        // it once here, so every answer below reports what the journal currently
        // holds rather than what it held before the network work.
        drop(hashes);
        let hashes = candidate_transaction_hashes(self.db, wallet_id, identity)?;
        // Whether a dispatched attempt may still commit is asked by three of the
        // branches below. Read it here, with the candidate set, so no
        // classification is preceded by a database wait of its own.
        let live_attempt = has_ambiguous_attempt(self.db, wallet_id, identity)?;
        // Those reads can wait on SQLite, so cancellation may have arrived since
        // the check above, and every classification below is a claim this
        // operation is no longer entitled to make.
        if cancel() {
            return Ok(ChainLifecycleOutcome::Cancelled);
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
            // The check above ran before `apply_confirmation` waited on the
            // database connection. It checks again once it holds it, so a
            // session invalidated during that wait cannot have hashes and tree
            // positions written underneath it.
            let Some(confirmation) =
                apply_confirmation(self.db, wallet_id, identity, &hash, &response, cancel)?
            else {
                return Ok(ChainLifecycleOutcome::Cancelled);
            };
            return Ok(ChainLifecycleOutcome::Confirmed { confirmation });
        }
        // The entry shortcut ran before these lookups. Another process can apply
        // the confirmation while they are in flight, and it outranks every
        // weaker answer below: a lagging 404 would otherwise report `Pending`,
        // and a different candidate's committed failure would report `Rejected`,
        // for a submission that is now durably confirmed.
        let durable = durable_confirmation_hash(self.db, wallet_id, identity)?;
        // That read waits on the database too, and every answer below it asserts
        // this operation is still active.
        if cancel() {
            return Ok(ChainLifecycleOutcome::Cancelled);
        }
        if let Some(tx_hash) = durable {
            return Ok(ChainLifecycleOutcome::AlreadyConfirmed { tx_hash });
        }
        if let Some(message) = unresolved {
            return Ok(ChainLifecycleOutcome::OutcomeUnknown {
                known_tx_hashes: hashes,
                message,
            });
        }
        if any_pending {
            // A hashless attempt may already have committed under a hash nothing
            // can locate, so reporting that the known candidates simply have not
            // committed yet would overstate what this call established — the same
            // rule the committed-failure and terminal-error branches follow.
            if live_attempt {
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
        if let Some(error) = terminal_error {
            // A lookup that could not be completed is an absence of evidence,
            // not evidence of absence, so it ranks below everything that is
            // evidence: a candidate another endpoint reported pending, one whose
            // own answer was unreadable, and a dispatched attempt still awaiting
            // a response. Only when nothing may still commit is the error this
            // call's answer. `reconcile` reaches this exit directly, without the
            // submission loop's ambiguity handling around it, so the check
            // belongs here.
            let remaining = hashes;
            if !remaining.is_empty() || live_attempt {
                return Ok(ChainLifecycleOutcome::OutcomeUnknown {
                    known_tx_hashes: remaining,
                    message: format!(
                        "a candidate for this submission may still commit; \
                         another candidate's lookup failed: {error}"
                    ),
                });
            }
            return Err(error.into());
        }
        if !committed_failures.is_empty() {
            // These rejections are definite only for the candidates they name.
            // A hashless attempt that may still commit outranks them, and so
            // does a candidate another writer journaled while this lookup was
            // running: retirement above marked the proven failures `rejected`,
            // so anything `candidate_transaction_hashes` still returns is a transaction nobody
            // has disproved. `has_ambiguous_attempt` cannot see it, because a
            // candidate arrives `accepted`.
            if !hashes.is_empty() || live_attempt {
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
pub(super) fn durable_confirmation_hash(
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
                identity.round_id(),
                wallet_id,
                i64::from(identity.bundle_index())
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
    match identity.kind() {
        ChainSubmissionKind::Delegation => Ok(delegation_tx_hash),
        ChainSubmissionKind::Vote => {
            let proposal_id = identity.require_proposal_id()?;
            let Some(state) = queries::load_vote_row_state(
                &conn,
                identity.round_id(),
                wallet_id,
                identity.bundle_index(),
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
                identity.round_id(),
                identity.bundle_index(),
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
                    identity.round_id(),
                    wallet_id,
                    identity.bundle_index(),
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

/// Records a confirmation, or reports that cancellation arrived first.
///
/// `Ok(None)` means the write was abandoned before it opened, because the
/// callback said so once the database connection was already held. The caller's
/// own check runs before that acquisition, and acquiring it can block for as
/// long as another writer holds the connection.
pub(super) fn apply_confirmation(
    db: &VotingDb,
    wallet_id: &str,
    identity: &ChainSubmissionIdentity,
    tx_hash: &str,
    response: &ChainTxConfirmation,
    cancel: &dyn Fn() -> bool,
) -> Result<Option<ChainConfirmation>, VotingError> {
    // The captured wallet is passed into the confirmation transaction rather
    // than checked here and re-read there: an account switch between the check
    // and the write would otherwise scope the confirmation to the new wallet.
    match identity.kind() {
        ChainSubmissionKind::Delegation => confirm_delegation_submission_for_wallet(
            db,
            wallet_id,
            identity.round_id(),
            identity.bundle_index(),
            tx_hash,
            &response.events,
            StoredHashConflict::ClearUnconfirmed,
            cancel,
        )
        .map(|confirmation| confirmation.map(ChainConfirmation::Delegation)),
        ChainSubmissionKind::Vote => confirm_vote_submission_for_wallet(
            db,
            wallet_id,
            identity.round_id(),
            identity.bundle_index(),
            identity.require_proposal_id()?,
            tx_hash,
            &response.events,
            StoredHashConflict::ClearUnconfirmed,
            cancel,
        )
        .map(|confirmation| confirmation.map(ChainConfirmation::Vote)),
        ChainSubmissionKind::VoteBatch => confirm_vote_batch_submission_for_wallet(
            db,
            wallet_id,
            identity.round_id(),
            identity.bundle_index(),
            &identity.require_batch_digest()?,
            tx_hash,
            &response.events,
            StoredHashConflict::ClearUnconfirmed,
            cancel,
        )
        .map(|confirmation| confirmation.map(ChainConfirmation::VoteBatch)),
    }
}

pub(super) fn events_bind_to(
    db: &VotingDb,
    wallet_id: &str,
    identity: &ChainSubmissionIdentity,
    tx_hash: &str,
    confirmation: &ChainTxConfirmation,
) -> bool {
    if confirmation.code != 0 {
        return true;
    }
    match identity.kind() {
        ChainSubmissionKind::Delegation => crate::confirmation::delegation_events_bind(
            tx_hash,
            identity.round_id(),
            &confirmation.events,
        ),
        ChainSubmissionKind::Vote => crate::confirmation::vote_events_bind(
            tx_hash,
            identity.round_id(),
            &confirmation.events,
        ),
        ChainSubmissionKind::VoteBatch => match identity.batch_digest() {
            Some(digest) => crate::confirmation::vote_batch_binds_to_recovery(
                &db.conn(),
                wallet_id,
                identity.round_id(),
                identity.bundle_index(),
                digest,
                tx_hash,
                &confirmation.events,
            ),
            None => false,
        },
    }
}
