use super::*;

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
        .submit_canonical_payload_locked(WALLET, identity, b"{}".to_vec(), &echo_rebuild, &|| false)
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
        .submit_canonical_payload_locked(
            WALLET,
            identity.clone(),
            b"{}".to_vec(),
            &echo_rebuild,
            &|| false,
        )
        .await
        .unwrap();
    let outcome = lifecycle
        .submit_canonical_payload_locked(WALLET, identity, b"{}".to_vec(), &echo_rebuild, &|| false)
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
        .submit_canonical_payload_locked(
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
        .submit_canonical_payload_locked(WALLET, identity, b"{}".to_vec(), &echo_rebuild, &|| false)
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
        .submit_canonical_payload_locked(
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
        .submit_canonical_payload_locked(WALLET, identity, b"{}".to_vec(), &echo_rebuild, &|| false)
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
        .submit_canonical_payload_locked(
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
        .submit_canonical_payload_locked(
            WALLET,
            identity,
            b"{}".to_vec(),
            &echo_rebuild,
            &record_after_first_post,
        )
        .await
        .unwrap();

    // The first attempt was dispatched ambiguously, so the honest answer
    // names the candidate while saying the submission is unresolved rather
    // than merely uncommitted; either way it bars a further broadcast.
    assert!(
        matches!(&outcome, ChainLifecycleOutcome::OutcomeUnknown { known_tx_hashes, .. }
                if known_tx_hashes == &vec![TX_HASH.to_string()]),
        "got {outcome:?}"
    );
    assert!(outcome_blocks_dispatch(&outcome));
    assert_eq!(
        *transport.posts.lock().unwrap(),
        1,
        "a known candidate that may still commit stops further broadcasts"
    );
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
        .submit_canonical_payload_locked(WALLET, identity, b"{}".to_vec(), &echo_rebuild, &|| false)
        .await
        .unwrap();

    // The candidate is `accepted`, which is neither `attempting` nor
    // `outcome_unknown`, so `has_ambiguous_attempt` cannot see it. Reporting this
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
        .submit_canonical_payload_locked(WALLET, identity, b"{}".to_vec(), &echo_rebuild, &|| false)
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
        .submit_canonical_payload_locked(WALLET, identity, b"{}".to_vec(), &rebuild, &|| false)
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
        .submit_canonical_payload_locked(
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
        .submit_canonical_payload_locked(
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
                if message.contains("may still commit")),
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
        .submit_canonical_payload_locked(
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

#[tokio::test]
async fn a_committed_failure_yields_to_a_candidate_journaled_during_the_lookup() {
    let path = std::env::temp_dir().join(format!(
        "zcash_voting_failure_race_{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let db = file_test_db(path.to_str().unwrap());
    journal_attempt(&db, "accepted", Some(TX_HASH));

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
    // marked that one `rejected`, so what `candidate_transaction_hashes` still returns is a
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

    // The same set `candidate_transaction_hashes` reconciles.
    assert_eq!(candidates, vec![TX_HASH.to_string(), TX_HASH_2.to_string()]);
}

#[tokio::test]
async fn a_confirmation_applied_during_the_final_post_is_not_downgraded() {
    let path = std::env::temp_dir().join(format!(
        "zcash_voting_final_post_confirm_{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let db = file_test_db(path.to_str().unwrap());
    let inner = MockTransport::default();
    // The only attempt ends ambiguously.
    inner
        .responses
        .lock()
        .unwrap()
        .push_back(Err(ChainTransportError::Timeout));
    let client = ChainClient::with_config(
        Arc::new(ConfirmDuringPostTransport {
            inner,
            db_path: path.to_str().unwrap().to_string(),
            done: Mutex::new(false),
        }),
        ChainEndpointSet::new(&["https://vote.example".to_string()]).unwrap(),
        ChainClientConfig::default()
            .with_retry_delays(vec![])
            .unwrap(),
    );
    let lifecycle = ChainSubmissionLifecycle::new(&db, &client);

    let outcome = lifecycle
        .submit_canonical_payload_locked(
            WALLET,
            ChainSubmissionIdentity::delegation(ROUND_ID, 0),
            b"{}".to_vec(),
            &echo_rebuild,
            &|| false,
        )
        .await
        .unwrap();

    // The lookup path rereads durable state before reporting anything
    // weaker; this exit has to as well, or a completed submission is handed
    // back as unresolved and the host keeps recovering it.
    assert_eq!(
        outcome,
        ChainLifecycleOutcome::AlreadyConfirmed {
            tx_hash: TX_HASH.to_string()
        }
    );
    drop(db);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_candidate_recorded_before_the_reservation_stops_the_dispatch() {
    let db = test_db();
    let identity = ChainSubmissionIdentity::delegation(ROUND_ID, 0);
    // A legacy recording call takes no lifecycle lock, so it can land after
    // the preflight. The reservation transaction is immediate, so such a
    // write either committed before it and is seen here, or waits for it.
    queries::store_delegation_tx_hash(&db.conn(), ROUND_ID, WALLET, 0, TX_HASH).unwrap();

    let reserved = reserve_dispatch_attempt(
        &db,
        WALLET,
        &identity,
        &Sha256::digest(b"{}").into(),
        &echo_rebuild,
    )
    .unwrap();

    // Reserving would commit this call to a duplicate broadcast for a
    // submission already known to have a transaction outstanding.
    assert_eq!(reserved, None);
    let journaled: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM chain_submission_attempts",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(journaled, 0);
}

#[tokio::test]
async fn a_candidate_journaled_during_a_failing_lookup_outranks_its_error() {
    let path = std::env::temp_dir().join(format!(
        "zcash_voting_error_race_{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let db = file_test_db(path.to_str().unwrap());
    journal_attempt(&db, "accepted", Some(TX_HASH));

    /// Journals a second candidate while the first one's lookup runs.
    struct JournalDuringFailingLookup {
        inner: MockTransport,
        db_path: String,
        done: Mutex<bool>,
    }
    impl ChainTransport for JournalDuringFailingLookup {
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
    inner
        .responses
        .lock()
        .unwrap()
        .push_back(Ok(response(401, r#"{"message":"unauthorized"}"#)));
    let client = ChainClient::new(
        Arc::new(JournalDuringFailingLookup {
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

    // The candidate arrived `accepted`, which the live-attempt query does
    // not match, and the pre-lookup snapshot predates it. Returning the
    // error would end recovery for a transaction that may still commit.
    assert!(
        matches!(&outcome, ChainLifecycleOutcome::OutcomeUnknown { known_tx_hashes, .. }
                if known_tx_hashes.contains(&TX_HASH_2.to_string())),
        "got {outcome:?}"
    );
    drop(db);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn a_confirmation_applied_during_the_post_outranks_acceptance() {
    let path = std::env::temp_dir().join(format!(
        "zcash_voting_accept_confirm_{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let db = file_test_db(path.to_str().unwrap());
    let inner = MockTransport::default();
    inner.responses.lock().unwrap().push_back(Ok(response(
        200,
        &format!(r#"{{"tx_hash":"{TX_HASH_2}","code":0,"log":""}}"#),
    )));
    let client = ChainClient::new(
        Arc::new(ConfirmDuringPostTransport {
            inner,
            db_path: path.to_str().unwrap().to_string(),
            done: Mutex::new(false),
        }),
        ChainEndpointSet::new(&["https://vote.example".to_string()]).unwrap(),
    );
    let lifecycle = ChainSubmissionLifecycle::new(&db, &client);

    let outcome = lifecycle
        .submit_canonical_payload_locked(
            WALLET,
            ChainSubmissionIdentity::delegation(ROUND_ID, 0),
            b"{}".to_vec(),
            &echo_rebuild,
            &|| false,
        )
        .await
        .unwrap();

    // Acceptance is not commitment, so it is weaker than a confirmation
    // already applied. Reporting it would send the host polling a submission
    // that is durably complete; the hash this call learned stays journaled.
    assert_eq!(
        outcome,
        ChainLifecycleOutcome::AlreadyConfirmed {
            tx_hash: TX_HASH.to_string()
        }
    );
    drop(db);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_reservation_is_stamped_after_its_blocking_validation() {
    let db = test_db();
    let identity = ChainSubmissionIdentity::delegation(ROUND_ID, 0);
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    // The rebuild stands in for the blocking work between entry and the
    // insert: acquiring the connection, and re-deriving the payload.
    let rebuild = |conn: &rusqlite::Connection| -> Result<Vec<u8>, VotingError> {
        std::thread::sleep(Duration::from_millis(1100));
        echo_rebuild(conn)
    };

    let id = reserve_dispatch_attempt(
        &db,
        WALLET,
        &identity,
        &Sha256::digest(b"{}").into(),
        &rebuild,
    )
    .unwrap()
    .unwrap();

    let stamped: i64 = db
        .conn()
        .query_row(
            "SELECT created_at FROM chain_submission_attempts WHERE id=?1",
            rusqlite::params![id],
            |row| row.get(0),
        )
        .unwrap();
    // A reservation stamped on entry is already part-spent against the
    // freshness grace another process reads it by.
    assert!(stamped > before, "stamped={stamped} before={before}");
}

#[tokio::test]
async fn a_hashless_attempt_outranks_a_merely_pending_candidate() {
    let db = test_db();
    journal_attempt(&db, "outcome_unknown", None);
    journal_attempt(&db, "accepted", Some(TX_HASH));
    let transport = Arc::new(MockTransport::default());
    transport
        .responses
        .lock()
        .unwrap()
        .push_back(Ok(response(404, r#"{"detail":"not found"}"#)));
    let client = accepted_client(transport);
    let lifecycle = ChainSubmissionLifecycle::new(&db, &client);

    let outcome = lifecycle
        .reconcile(&ChainSubmissionIdentity::delegation(ROUND_ID, 0), &|| false)
        .await
        .unwrap();

    // `Pending` would assert the known candidates simply have not committed
    // yet. The hashless attempt may already have committed under a hash
    // nothing can locate, which is a different and weaker claim.
    assert!(
        matches!(&outcome, ChainLifecycleOutcome::OutcomeUnknown { known_tx_hashes, .. }
                if known_tx_hashes == &vec![TX_HASH.to_string()]),
        "got {outcome:?}"
    );
}

#[tokio::test]
async fn an_accepted_hash_survives_an_unreadable_final_reread() {
    let path = std::env::temp_dir().join(format!(
        "zcash_voting_unreadable_reread_{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let db = file_test_db(path.to_str().unwrap());
    let inner = MockTransport::default();
    inner.responses.lock().unwrap().push_back(Ok(response(
        200,
        &format!(r#"{{"tx_hash":"{TX_HASH}","code":0,"log":""}}"#),
    )));
    let client = ChainClient::new(
        Arc::new(BreakDurableReadTransport {
            inner,
            db_path: path.to_str().unwrap().to_string(),
            done: Mutex::new(false),
        }),
        ChainEndpointSet::new(&["https://vote.example".to_string()]).unwrap(),
    );
    let lifecycle = ChainSubmissionLifecycle::new(&db, &client);

    let outcome = lifecycle
        .submit_canonical_payload_locked(
            WALLET,
            ChainSubmissionIdentity::delegation(ROUND_ID, 0),
            b"{}".to_vec(),
            &echo_rebuild,
            &|| false,
        )
        .await
        .unwrap();

    // The hash is the only handle on a transaction now in the mempool, and
    // it was established before this supplementary read was attempted. A
    // database that has become unreadable must not take it away. That
    // covers the hash-ownership check on this path too: it reads the same
    // missing tables, and a check that could not be made is no evidence
    // that the hash belongs to another submission.
    assert_eq!(
        outcome,
        ChainLifecycleOutcome::Accepted {
            tx_hash: TX_HASH.to_string()
        }
    );
    drop(db);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn a_candidate_journaled_during_a_pending_lookup_is_reported() {
    let path = std::env::temp_dir().join(format!(
        "zcash_voting_pending_race_{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let db = file_test_db(path.to_str().unwrap());
    journal_attempt(&db, "accepted", Some(TX_HASH));
    let inner = MockTransport::default();
    // The known candidate is simply not committed yet.
    inner
        .responses
        .lock()
        .unwrap()
        .push_back(Ok(response(404, r#"{"detail":"not found"}"#)));
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

    // Reporting the pre-lookup snapshot would have the host poll only the
    // candidate it already knew, while the one journaled during the request
    // commits unnoticed.
    let reported = match &outcome {
        ChainLifecycleOutcome::Pending { known_tx_hashes } => known_tx_hashes.clone(),
        other => panic!("got {other:?}"),
    };
    assert!(reported.contains(&TX_HASH_2.to_string()), "{reported:?}");
    drop(db);
    let _ = std::fs::remove_file(&path);
}
