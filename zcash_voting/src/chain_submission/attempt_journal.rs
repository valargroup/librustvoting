//! Durable attempt evidence, candidate discovery, and recovery coverage.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{named_params, TransactionBehavior};
use sha2::{Digest, Sha256};

use super::{
    in_flight_for_round, internal, now_seconds, stale_generation_error, ChainSubmissionIdentity,
    ChainSubmissionKind, PayloadRebuild, FUTURE_STAMP_TOLERANCE_SECS,
    INTERRUPTED_RESERVATION_GRACE_SECS,
};
use crate::{
    storage::{queries, VotingDb},
    types::VotingError,
    vote,
};

/// Journals one dispatch attempt, binding it to the state it was built from.
///
/// The payload was serialized from durable state before this call, and another
/// database connection can clear or replace that state in the interval. So the
/// identity's round, its owner, and the payload itself are all revalidated
/// inside this one immediate transaction: the canonical bytes are re-derived
/// from storage and must still hash to `payload_digest`. A mismatch fails here,
/// before any request is dispatched, rather than POSTing bytes that no longer
/// describe what is stored.
pub(super) fn reserve_dispatch_attempt(
    db: &VotingDb,
    wallet_id: &str,
    identity: &ChainSubmissionIdentity,
    payload_digest: &[u8; 32],
    rebuild: PayloadRebuild<'_>,
) -> Result<Option<i64>, VotingError> {
    let mut conn = db.conn();
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(internal("begin chain attempt reservation"))?;
    let round_exists: bool = tx
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM rounds WHERE round_id=?1 AND wallet_id=?2)",
            rusqlite::params![identity.round_id(), wallet_id],
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
    // The preflight ran before this transaction. A legacy recording call takes
    // no lifecycle lock, so a candidate it wrote in between is invisible until
    // here — and this transaction is immediate, so such a write either landed
    // before it and is seen now, or waits for it. Dispatching anyway would
    // broadcast a duplicate for a submission already known to have one.
    if !candidate_transaction_hashes_with_conn(&tx, wallet_id, identity)?.is_empty() {
        return Ok(None);
    }
    // Taken here, not on entry: acquiring the connection and rebuilding the
    // payload both block, and a reservation stamped before that wait is already
    // part-spent against the freshness grace another process reads it by.
    let now = now_seconds()?;
    tx.execute(
        "INSERT INTO chain_submission_attempts
         (round_id, wallet_id, kind, bundle_index, proposal_id, batch_digest,
          payload_digest, state, created_at, updated_at)
         VALUES (:round_id, :wallet_id, :kind, :bundle_index, :proposal_id,
                 :batch_digest, :payload_digest, 'attempting', :now, :now)",
        named_params! {
            ":round_id": identity.round_id(),
            ":wallet_id": wallet_id,
            ":kind": identity.kind().as_str(),
            ":bundle_index": i64::from(identity.bundle_index()),
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
    Ok(Some(id))
}

/// How often an outstanding reservation refreshes its `updated_at`.
///
/// Far below [`INTERRUPTED_RESERVATION_GRACE_SECS`], so a reservation whose
/// owner is alive stays comfortably inside the grace period however the wall
/// clock moves, and far above the cost of the write, which most calls never
/// perform at all: the default request deadline is ten seconds.
/// Records one attempt's classified outcome.
///
/// Scoped by the wallet captured when the attempt was reserved, not by the
/// database's current wallet. A host that switches accounts while a POST is in
/// flight must still be able to journal the response it already received;
/// re-reading the current wallet here would update zero rows and lose an
/// accepted transaction hash.
pub(super) fn record_attempt_evidence(
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
/// Wallet-scoped like [`record_attempt_evidence`], so an account switch cannot leave the
/// reservation behind.
pub(super) fn delete_definitely_unsent_attempt(
    db: &VotingDb,
    wallet_id: &str,
    attempt_id: i64,
) -> Result<(), VotingError> {
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
/// written and which `candidate_transaction_hashes` still reads. Clearing the domain hash is
/// scoped to an exact match on a row with no recorded confirmation position, so
/// it can only ever remove a hash this reconciliation just proved failed, never
/// a confirmed one.
pub(super) fn retire_failed_candidate(
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
            ":round_id": identity.round_id(),
            ":wallet_id": wallet_id,
            ":kind": identity.kind().as_str(),
            ":bundle_index": i64::from(identity.bundle_index()),
            ":proposal_id": identity.proposal_key(),
            ":batch_digest": identity.batch_key(),
            ":tx_hash": tx_hash,
        },
    )
    .map_err(internal("retire committed-failure chain attempt"))?;
    match identity.kind() {
        ChainSubmissionKind::Delegation => {
            tx.execute(
                "UPDATE bundles SET delegation_tx_hash=NULL
                  WHERE round_id=:round_id AND wallet_id=:wallet_id
                    AND bundle_index=:bundle_index
                    AND delegation_tx_hash=:tx_hash
                    AND van_leaf_position IS NULL",
                named_params! {
                    ":round_id": identity.round_id(),
                    ":wallet_id": wallet_id,
                    ":bundle_index": i64::from(identity.bundle_index()),
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
                    ":round_id": identity.round_id(),
                    ":wallet_id": wallet_id,
                    ":bundle_index": i64::from(identity.bundle_index()),
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

/// The proposal whose identity decides hash ownership for this submission.
///
/// Ownership is judged per submission, and the rule is expressed in terms of a
/// proposal: a singleton owns its own, a batch owns whatever its members share,
/// and a delegation has none. Any member answers for a batch — they all carry
/// the same digest, which is what the rule compares — so the first is taken.
fn ownership_proposal(
    conn: &rusqlite::Connection,
    wallet_id: &str,
    identity: &ChainSubmissionIdentity,
) -> Result<Option<u32>, VotingError> {
    match identity.kind() {
        ChainSubmissionKind::Delegation => Ok(None),
        ChainSubmissionKind::Vote => Ok(Some(identity.require_proposal_id()?)),
        ChainSubmissionKind::VoteBatch => {
            let digest = identity.require_batch_digest()?;
            let members = crate::vote::load_vote_batch_recoveries_with_conn(
                conn,
                wallet_id,
                identity.round_id(),
                identity.bundle_index(),
                digest,
            )?;
            Ok(members.first().map(|member| member.proposal_id))
        }
    }
}

/// Why an accepted hash may not become this submission's candidate, if it may not.
///
/// A hash another submission already owns must not be journaled here. Ownership
/// is checked at confirmation anyway, but by then it is too late to matter: the
/// candidate is journaled, a successful candidate is never retired, and every
/// later submission rediscovers it and exits before dispatch, so the real
/// payload can never be sent again.
///
/// Only a proven conflict counts. A check that could not be made — an
/// unreadable database, a missing table — is no evidence of one, and the hash
/// is the only handle anything will ever have on a transaction already in the
/// mempool. Refusing it on a failed read would trade a bounded ambiguity for a
/// permanently lost transaction, which is the worse of the two by far.
fn accepted_hash_is_foreign(
    conn: &rusqlite::Connection,
    wallet_id: &str,
    identity: &ChainSubmissionIdentity,
    tx_hash: &str,
) -> Option<String> {
    let proposal_id = ownership_proposal(conn, wallet_id, identity).ok()?;
    match crate::storage::queries::ensure_tx_hash_free_for_submission(
        conn,
        identity.round_id(),
        wallet_id,
        identity.bundle_index(),
        proposal_id,
        tx_hash,
    ) {
        Ok(()) => None,
        // The rule reports a conflict it proved as invalid input. Every other
        // error is the check failing, not the hash failing it.
        Err(VotingError::InvalidInput { message }) => Some(message),
        Err(_) => None,
    }
}

/// Journals an accepted hash, or refuses it as another submission's.
///
/// The ownership check and the write it guards share one immediate transaction.
/// Read separately they are two: two submissions handed the same hash could
/// each find it free before either journaled it, and both would then carry it —
/// exactly the contradiction the rule exists to prevent, with reconciliation
/// order left to decide which identity receives the confirmation.
///
/// Returns the conflict when there was one, in which case the attempt was
/// journaled `outcome_unknown` with no hash instead.
pub(super) fn journal_accepted_hash(
    db: &VotingDb,
    wallet_id: &str,
    identity: &ChainSubmissionIdentity,
    attempt_id: i64,
    tx_hash: &str,
) -> Result<Option<String>, VotingError> {
    let now = now_seconds()?;
    let mut conn = db.conn();
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(internal("begin accepted hash transaction"))?;
    let conflict = accepted_hash_is_foreign(&tx, wallet_id, identity, tx_hash);
    let (state, stored) = match &conflict {
        // The POST happened, so the ambiguity is real; the hash is not this
        // submission's, so it is not recorded.
        Some(_) => ("outcome_unknown", None),
        None => ("accepted", Some(tx_hash)),
    };
    let updated = tx
        .execute(
            "UPDATE chain_submission_attempts
                SET state=:state,
                    chain_tx_hash=COALESCE(:tx_hash, chain_tx_hash),
                    updated_at=:now
              WHERE id=:id AND wallet_id=:wallet_id",
            named_params! {
                ":state": state,
                ":tx_hash": stored,
                ":now": now,
                ":id": attempt_id,
                ":wallet_id": wallet_id,
            },
        )
        .map_err(internal("record accepted chain attempt"))?;
    if updated != 1 {
        return Err(VotingError::InvalidInput {
            message: "chain submission attempt no longer exists".to_string(),
        });
    }
    tx.commit()
        .map_err(internal("commit accepted chain attempt"))?;
    Ok(conflict)
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
/// Every chain transaction hash that could identify this submission.
///
/// Candidates come from two sources with different histories: the legacy domain
/// columns, which preserve whatever casing their caller passed, and the attempt
/// journal, which stores hashes as the chain client normalized them. They are
/// canonicalized before deduplication so one transaction recorded under two
/// casings is queried once, instead of being counted as two committed
/// candidates and reported as an invariant violation.
pub(super) fn candidate_transaction_hashes(
    db: &VotingDb,
    wallet_id: &str,
    identity: &ChainSubmissionIdentity,
) -> Result<Vec<String>, VotingError> {
    candidate_transaction_hashes_with_conn(&db.conn(), wallet_id, identity)
}

/// [`candidate_transaction_hashes`] against a caller-held connection.
///
/// The reservation needs this inside its own transaction: a legacy recording
/// call does not take the lifecycle's operation lock, so only SQLite's own
/// serialization can order it against the dispatch gate.
fn candidate_transaction_hashes_with_conn(
    conn: &rusqlite::Connection,
    wallet_id: &str,
    identity: &ChainSubmissionIdentity,
) -> Result<Vec<String>, VotingError> {
    let mut candidates = match identity.kind() {
        ChainSubmissionKind::Delegation => queries::get_delegation_tx_hash(
            conn,
            identity.round_id(),
            wallet_id,
            identity.bundle_index(),
        )?
        .into_iter()
        .collect(),
        ChainSubmissionKind::Vote => queries::get_vote_tx_hash(
            conn,
            identity.round_id(),
            wallet_id,
            identity.bundle_index(),
            identity.require_proposal_id()?,
        )?
        .into_iter()
        .collect(),
        ChainSubmissionKind::VoteBatch => {
            let recoveries = match vote::load_vote_batch_recoveries_with_conn(
                conn,
                wallet_id,
                identity.round_id(),
                identity.bundle_index(),
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
                    conn,
                    identity.round_id(),
                    wallet_id,
                    identity.bundle_index(),
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
                ":round_id": identity.round_id(),
                ":wallet_id": wallet_id,
                ":kind": identity.kind().as_str(),
                ":bundle_index": i64::from(identity.bundle_index()),
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
/// transaction hash: [`candidate_transaction_hashes`] is the only way an attempt ever reaches
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
/// ambiguity by [`has_ambiguous_attempt`], which is unchanged, and as
/// `AlreadySpentUnresolved` if a replacement is later refused on chain.
pub(crate) fn can_still_learn_a_hash() -> Result<String, VotingError> {
    let (floor, ceiling) = fresh_attempt_window()?;
    Ok(format!(
        "(chain_tx_hash IS NOT NULL \
          OR (state='attempting' AND updated_at >= {floor} AND updated_at <= {ceiling}))"
    ))
}

/// The window of stamps [`can_still_learn_a_hash`] believes, as of now.
///
/// A reservation touched at or after the floor may still be in flight; an older
/// one cannot be, because no configurable request deadline reaches back that far.
///
/// The ceiling is the other half of the same claim, and it exists because the
/// clock can step backward. Every writer stamps from this machine's clock, so a
/// stamp ahead of now is not a fresher reservation — it is a stamp made before
/// the clock moved back. Believed as a lower bound alone, such a row stays
/// "fresh" until the clock catches up to it and then runs the whole grace period
/// again, which for a large correction is hours rather than the documented ten
/// minutes, and the in-memory registry cannot rescue a process that has already
/// exited. Freezing recovery replacement, ballot-intent changes, and pruning for
/// that long is the worse outcome: a stamp outside the window is treated as
/// evidence of nothing, and the duplicate POST that may follow is the bounded
/// case this design already accepts, since consensus nullifiers let at most one
/// semantic action succeed and the loser is retired.
///
/// One heartbeat of tolerance absorbs second-granularity rounding between the
/// stamp and this read. Nothing legitimate stamps further ahead than that.
///
/// This reads the wall clock, which can also step forward. A forward jump larger
/// than the grace period would age out a POST that is genuinely in flight, which
/// is why this is the weaker of the two tests and never the only one: a
/// reservation this process is waiting on is named by [`in_flight_attempt_ids`]
/// and stays covered whatever the clock does.
pub(crate) fn fresh_attempt_window() -> Result<(i64, i64), VotingError> {
    let now = now_seconds()?;
    Ok((
        now.saturating_sub(INTERRUPTED_RESERVATION_GRACE_SECS),
        now.saturating_add(FUTURE_STAMP_TOLERANCE_SECS),
    ))
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
/// other. Later attempts win, matching the order `candidate_transaction_hashes` returns them in.
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
/// The union `candidate_transaction_hashes` reconciles: the legacy domain column and the attempt
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

/// Whether a dispatched attempt may still commit.
///
/// `attempting` and `outcome_unknown` are exactly the states that mean "a
/// request may have reached the chain without producing a usable response",
/// including the hashless case a timeout or an interruption leaves behind.
pub(super) fn has_ambiguous_attempt(
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
                ":round_id": identity.round_id(),
                ":wallet_id": wallet_id,
                ":kind": identity.kind().as_str(),
                ":bundle_index": i64::from(identity.bundle_index()),
                ":proposal_id": identity.proposal_key(),
                ":batch_digest": identity.batch_key(),
            },
            |row| row.get(0),
        )
        .map_err(internal("query live chain attempts"))
}
