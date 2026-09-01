use super::*;

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
        .submit_canonical_payload_locked(
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

#[test]
fn outcomes_are_journaled_under_the_wallet_that_reserved_them() {
    let db = test_db();
    let attempt_id = journal_attempt(&db, "attempting", None);
    // The host switches accounts while the request is in flight.
    db.set_wallet_id("wallet-2");

    record_attempt_evidence(&db, WALLET, attempt_id, "accepted", Some(TX_HASH)).unwrap();

    let identity = ChainSubmissionIdentity::delegation(ROUND_ID, 0);
    assert_eq!(
        candidate_transaction_hashes(&db, WALLET, &identity).unwrap(),
        vec![TX_HASH.to_string()],
        "an accepted hash must not be lost to an account switch"
    );
    // And the reservation's owner is what a definitely-unsent deletion uses.
    let unsent = journal_attempt(&db, "attempting", None);
    delete_definitely_unsent_attempt(&db, WALLET, unsent).unwrap();
    assert_eq!(attempt_states(&db), vec!["accepted".to_string()]);
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
        &|| false,
    )
    .unwrap()
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

#[tokio::test]
async fn cancellation_arriving_during_the_confirmation_wait_writes_nothing() {
    let db = test_db();
    journal_attempt(&db, "accepted", Some(TX_HASH));
    let transport = Arc::new(MockTransport::default());
    let events = serde_json::to_string(&delegate_vote_events("5")).unwrap();
    transport.responses.lock().unwrap().push_back(Ok(response(
        200,
        &format!(r#"{{"height":42,"code":0,"log":"","events":{events}}}"#),
    )));
    let client = accepted_client(transport.clone());
    let lifecycle = ChainSubmissionLifecycle::new(&db, &client);
    // Cancelled exactly when the database connection is already held, which
    // is true at one place only: inside the confirmation write, past every
    // checkpoint the caller performs. Every lifecycle checkpoint runs with
    // the connection free and so sees "not cancelled".
    let cancel = || db.try_conn().is_none();

    let outcome = lifecycle
        .reconcile(&ChainSubmissionIdentity::delegation(ROUND_ID, 0), &cancel)
        .await
        .unwrap();

    assert!(
        matches!(outcome, ChainLifecycleOutcome::Cancelled),
        "got {outcome:?}"
    );
    // The lookup ran, so this is the confirmation window and not an early
    // exit that never reached it.
    assert_eq!(*transport.gets.lock().unwrap(), 1);
    assert_eq!(db.get_delegation_tx_hash(ROUND_ID, 0).unwrap(), None);
    // The candidate stays journaled: the next reconciliation re-derives the
    // confirmation this one declined to apply.
    assert_eq!(attempt_states(&db), vec!["accepted".to_string()]);
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
        .submit_canonical_payload_locked(
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
        .submit_canonical_payload_locked(
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

#[test]
fn a_reservation_stamped_after_a_backward_clock_step_stops_covering() {
    let db = test_db();
    {
        let conn = db.conn();
        queries::store_vote(&conn, ROUND_ID, WALLET, 0, 4243, 1, &[0xCC; 32]).unwrap();
    }
    // A stamp far in the future, which is what a reservation made before the
    // clock stepped backward looks like. A proposal id no other test uses:
    // the in-flight registry is process-global.
    let far_ahead = now_seconds().unwrap() + 6 * 3600;
    db.conn()
        .execute(
            "INSERT INTO chain_submission_attempts
                 (round_id, wallet_id, kind, bundle_index, proposal_id, batch_digest,
                  payload_digest, state, created_at, updated_at)
                 VALUES (?1, ?2, 'vote', 0, 4243, X'', ?3, 'attempting', ?4, ?4)",
            rusqlite::params![ROUND_ID, WALLET, vec![0xCC_u8; 32], far_ahead],
        )
        .unwrap();

    let covered = {
        let conn = db.conn();
        attempt_protected_vote_rows(&conn, ROUND_ID, WALLET).unwrap()
    };

    // Believed as a lower bound alone, this row stays "fresh" until the
    // clock catches up to it and then runs the whole grace period again —
    // hours of frozen recovery replacement, ballot-intent changes, and
    // pruning, with no in-memory registry to rescue it once the process that
    // made it has exited. A stamp nothing legitimate could have written is
    // evidence of nothing.
    assert!(!covered.contains(&(0, 4243)), "{covered:?}");
}

#[test]
fn a_reservation_this_process_awaits_survives_a_wall_clock_jump() {
    let db = test_db();
    {
        let conn = db.conn();
        queries::store_vote(&conn, ROUND_ID, WALLET, 0, 4242, 1, &[0xCC; 32]).unwrap();
    }
    const ATTEMPT_ID: i64 = 9_000_001;
    // A proposal id no other test uses: the in-flight registry is
    // process-global, so two tests sharing an identity would see each
    // other's registrations.
    // Timestamps that look older than the grace period. A forward step of
    // the system clock while the POST is in flight produces exactly this:
    // the row is untouched, but "now" has moved past its deadline.
    let ancient = 1_i64;
    db.conn()
        .execute(
            "INSERT INTO chain_submission_attempts
                 (id, round_id, wallet_id, kind, bundle_index, proposal_id, batch_digest,
                  payload_digest, state, created_at, updated_at)
                 VALUES (?1, ?2, ?3, 'vote', 0, 4242, X'', ?4, 'attempting', ?5, ?5)",
            rusqlite::params![ATTEMPT_ID, ROUND_ID, WALLET, vec![0xCC_u8; 32], ancient],
        )
        .unwrap();

    let identity = ChainSubmissionIdentity::vote(ROUND_ID, 0, 4242);
    let in_flight = InFlightAttempt::register(WALLET, &identity);
    let covered = {
        let conn = db.conn();
        attempt_protected_vote_rows(&conn, ROUND_ID, WALLET).unwrap()
    };
    // The age test is the weaker of the two and never the only one: this
    // process knows it is waiting on the response, so the recovery that
    // response would be confirmed against must not become erasable just
    // because the clock moved.
    assert!(covered.contains(&(0, 4242)), "{covered:?}");

    drop(in_flight);
    let after = {
        let conn = db.conn();
        attempt_protected_vote_rows(&conn, ROUND_ID, WALLET).unwrap()
    };
    // Once nothing is waiting on it, the stale reservation is an
    // interrupted one and stops covering.
    assert!(!after.contains(&(0, 4242)), "{after:?}");
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

#[test]
fn a_heartbeat_marks_only_an_outstanding_reservation_as_still_owned() {
    let db = test_db();
    let outstanding = journal_attempt(&db, "attempting", None);
    let settled = journal_attempt(&db, "outcome_unknown", None);
    db.conn()
        .execute("UPDATE chain_submission_attempts SET updated_at=1", [])
        .unwrap();

    refresh_attempt_reservation(&db, WALLET, outstanding);
    refresh_attempt_reservation(&db, WALLET, settled);
    // A different account must not be able to refresh this reservation.
    refresh_attempt_reservation(&db, "someone-else", outstanding);

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
        .submit_canonical_payload_locked(
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
        .submit_canonical_payload_locked(
            WALLET,
            ChainSubmissionIdentity::delegation(ROUND_ID, 0),
            b"{}".to_vec(),
            &echo_rebuild,
            &cancel,
        )
        .await
        .unwrap();

    // The reconciliation was cancelled, so it settled nothing, and the
    // racing candidate is `accepted` — which `has_ambiguous_attempt` does not
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
        .submit_canonical_payload_locked(
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
        .submit_canonical_payload_locked(
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
async fn a_cancelled_reservation_race_preserves_earlier_ambiguity() {
    let db = test_db();
    let transport = Arc::new(MockTransport::default());
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
    let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = Arc::clone(&cancelled);
    let calls = Arc::new(Mutex::new(0usize));
    // The rebuild runs inside the reservation transaction, just before its
    // candidate check. On the retry it records a candidate — the race this
    // gate exists for — and the operation is cancelled at the same moment.
    let rebuild = move |conn: &rusqlite::Connection| -> Result<Vec<u8>, VotingError> {
        let mut seen = calls.lock().unwrap();
        *seen += 1;
        if *seen == 2 {
            queries::store_delegation_tx_hash(conn, ROUND_ID, WALLET, 0, TX_HASH)?;
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        echo_rebuild(conn)
    };
    let cancel = || cancelled.load(std::sync::atomic::Ordering::SeqCst);

    let outcome = lifecycle
        .submit_canonical_payload_locked(
            WALLET,
            ChainSubmissionIdentity::delegation(ROUND_ID, 0),
            b"{}".to_vec(),
            &rebuild,
            &cancel,
        )
        .await
        .unwrap();

    // The first attempt completed ambiguously and may still commit, so this
    // gate owes the caller that, not `Cancelled`.
    assert!(
        matches!(&outcome, ChainLifecycleOutcome::OutcomeUnknown { message, .. }
                if message.contains("timed out")),
        "got {outcome:?}"
    );
}

#[tokio::test]
async fn cancellation_during_the_no_candidate_reads_is_observed() {
    let db = test_db();
    // A hashless dispatched attempt: no candidate to look up, so this call
    // takes the fast path and never reaches the lookup loop's checks.
    journal_attempt(&db, "outcome_unknown", None);
    let transport = Arc::new(MockTransport::default());
    let client = accepted_client(transport);
    let lifecycle = ChainSubmissionLifecycle::new(&db, &client);
    // False on entry, true by the time the state reads have finished.
    let checks = std::sync::atomic::AtomicUsize::new(0);
    let cancel = || checks.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= 2;

    let outcome = lifecycle
        .reconcile(&ChainSubmissionIdentity::delegation(ROUND_ID, 0), &cancel)
        .await
        .unwrap();

    // Reporting the ambiguity would say the operation is active, which a
    // cancelled one is not.
    assert_eq!(outcome, ChainLifecycleOutcome::Cancelled);
}
