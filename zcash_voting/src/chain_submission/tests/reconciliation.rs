use super::*;

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
        .submit_canonical_payload_locked(
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
        .submit_canonical_payload_locked(
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
        .submit_canonical_payload_locked(WALLET, identity, b"{}".to_vec(), &echo_rebuild, &|| false)
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
async fn a_committed_failure_is_retired_even_when_two_candidates_report_success() {
    let db = test_db();
    journal_attempt(&db, "accepted", Some(TX_HASH));
    journal_attempt(&db, "accepted", Some(TX_HASH_2));
    journal_attempt(&db, "accepted", Some(TX_HASH_3));
    let transport = Arc::new(MockTransport::default());
    let events = serde_json::to_string(&delegate_vote_events("5")).unwrap();
    transport.responses.lock().unwrap().extend([
        Ok(response(
            200,
            r#"{"height":41,"code":7,"log":"deliver failed","events":[]}"#,
        )),
        // Two successes for one submission is a chain-level impossibility,
        // so the call refuses to apply either. That refusal must not also
        // strand the failure it already proved.
        Ok(response(
            200,
            &format!(r#"{{"height":42,"code":0,"log":"","events":{events}}}"#),
        )),
        Ok(response(
            200,
            &format!(r#"{{"height":43,"code":0,"log":"","events":{events}}}"#),
        )),
    ]);
    let client = accepted_client(transport);
    let lifecycle = ChainSubmissionLifecycle::new(&db, &client);

    let error = lifecycle
        .reconcile(&ChainSubmissionIdentity::delegation(ROUND_ID, 0), &|| false)
        .await
        .unwrap_err();

    assert!(
        error.to_string().contains("multiple chain candidates"),
        "{error}"
    );
    // The failure can never confirm. Left `accepted`, it would be
    // rediscovered by every later submission and block a replacement, a
    // ballot-intent change, and pruning for good — on the one exit that
    // already knows something is badly wrong.
    assert_eq!(
        attempt_states(&db),
        vec![
            "rejected".to_string(),
            "accepted".to_string(),
            "accepted".to_string()
        ]
    );
    // Neither success was applied.
    assert_eq!(db.get_delegation_tx_hash(ROUND_ID, 0).unwrap(), None);
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
        matches!(&outcome, ChainLifecycleOutcome::Confirmed { confirmation }
                if confirmation.tx_hash() == TX_HASH),
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
        .submit_canonical_payload_locked(
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
        matches!(&outcome, ChainLifecycleOutcome::Confirmed { confirmation }
                if confirmation.tx_hash() == TX_HASH),
        "got {outcome:?}"
    );
    // The duplicate that failed must not stay live: it would keep
    // protecting recovery rows and blocking bundle pruning.
    assert_eq!(
        attempt_states(&db),
        vec!["accepted".to_string(), "rejected".to_string()]
    );
}

#[tokio::test]
async fn adopting_a_success_clears_a_conflicting_unconfirmed_domain_hash() {
    let db = test_db();
    // A pre-lifecycle host recorded an opaque identifier the v18 migration
    // deliberately preserves. `candidate_transaction_hashes` skips it as a candidate, but
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
        matches!(&outcome, ChainLifecycleOutcome::Confirmed { confirmation }
                if confirmation.tx_hash() == TX_HASH),
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
    // `candidate_transaction_hashes` reads the domain column before the journal, so the
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
        matches!(&outcome, ChainLifecycleOutcome::Confirmed { confirmation }
                if confirmation.tx_hash() == TX_HASH),
        "got {outcome:?}"
    );
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
        matches!(&outcome, ChainLifecycleOutcome::Confirmed { confirmation }
                if confirmation.tx_hash() == TX_HASH),
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
        .submit_canonical_payload_locked(
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
                if message.contains("may still commit")),
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
        .submit_canonical_payload_locked(
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
        .ambiguity_overriding_failure(
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
        .ambiguity_overriding_failure(WALLET, &identity, &None, "storage failed")
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
        .submit_canonical_payload_locked(
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

#[tokio::test]
async fn a_pending_candidate_outranks_another_candidates_lookup_error() {
    let db = test_db();
    journal_attempt(&db, "accepted", Some(TX_HASH));
    journal_attempt(&db, "accepted", Some(TX_HASH_2));
    let transport = Arc::new(MockTransport::default());
    transport.responses.lock().unwrap().extend([
        // One candidate cannot be looked up at all...
        Ok(response(401, r#"{"message":"unauthorized"}"#)),
        // ...while the other is genuinely not yet committed.
        Ok(response(404, r#"{"detail":"not found"}"#)),
    ]);
    let client = accepted_client(transport);
    let lifecycle = ChainSubmissionLifecycle::new(&db, &client);

    let outcome = lifecycle
        .reconcile(&ChainSubmissionIdentity::delegation(ROUND_ID, 0), &|| false)
        .await
        .unwrap();

    // Failing to learn about one candidate says nothing about the other,
    // which may still commit. Reporting the error would end the host's
    // recovery for a submission that is merely waiting.
    assert!(
        matches!(&outcome, ChainLifecycleOutcome::Pending { known_tx_hashes }
                if known_tx_hashes.contains(&TX_HASH_2.to_string())),
        "got {outcome:?}"
    );
}
