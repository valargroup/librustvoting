use super::*;

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
        .submit_canonical_payload_locked(
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
        .submit_canonical_payload_locked(
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

#[test]
fn the_accepted_hash_ownership_check_holds_the_write_lock() {
    // A file-backed database, because the point is two connections.
    let path = std::env::temp_dir().join(format!(
        "zv-accepted-carrier-{}-{:?}.sqlite",
        std::process::id(),
        std::thread::current().id()
    ));
    let path_string = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    let db = init_test_db(VotingDb::open(&path_string).unwrap());
    db.conn()
        .execute(
            "INSERT INTO bundles (round_id, wallet_id, bundle_index) VALUES (?1, ?2, 1)",
            rusqlite::params![ROUND_ID, WALLET],
        )
        .unwrap();
    let attempt_id = journal_attempt(&db, "attempting", None);

    // Another writer takes bundle 1's carrier and has not committed. In WAL
    // a reader outside the write lock still sees the old snapshot, so an
    // ownership check made there would find the hash free.
    let other = rusqlite::Connection::open(&path).unwrap();
    other
        .busy_timeout(crate::storage::SQLITE_BUSY_TIMEOUT)
        .unwrap();
    other.execute_batch("BEGIN IMMEDIATE").unwrap();
    other
        .execute(
            "UPDATE bundles SET delegation_tx_hash = ?1
                 WHERE round_id = ?2 AND wallet_id = ?3 AND bundle_index = 1",
            rusqlite::params![TX_HASH, ROUND_ID, WALLET],
        )
        .unwrap();
    let commit = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(400));
        other.execute_batch("COMMIT").unwrap();
    });

    let conflict = journal_accepted_hash(
        &db,
        WALLET,
        &ChainSubmissionIdentity::delegation(ROUND_ID, 0),
        attempt_id,
        TX_HASH,
    )
    .unwrap();

    commit.join().unwrap();
    // Checked outside the write lock, both writers find the hash free and
    // both journal it, which is the contradiction the rule exists to
    // prevent — with reconciliation order left to decide which identity
    // receives the confirmation.
    assert!(conflict.is_some(), "the carrier must be seen");
    assert_eq!(attempt_states(&db), vec!["outcome_unknown".to_string()]);
    assert_eq!(
        db.conn()
            .query_row(
                "SELECT COUNT(*) FROM chain_submission_attempts WHERE chain_tx_hash IS NOT NULL",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        0
    );

    drop(db);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path_string}-wal"));
    let _ = std::fs::remove_file(format!("{path_string}-shm"));
}

#[tokio::test]
async fn an_accepted_hash_owned_by_another_bundle_does_not_become_a_candidate() {
    let db = test_db();
    // Bundle 1 already carries this transaction. A stale or misbehaving
    // endpoint answers this bundle's POST with it anyway.
    db.conn()
        .execute(
            "INSERT INTO bundles (round_id, wallet_id, bundle_index, delegation_tx_hash)
                 VALUES (?1, ?2, 1, ?3)",
            rusqlite::params![ROUND_ID, WALLET, TX_HASH],
        )
        .unwrap();
    let transport = Arc::new(MockTransport::default());
    transport.responses.lock().unwrap().push_back(Ok(response(
        200,
        &format!(r#"{{"tx_hash":"{TX_HASH}","code":0,"log":""}}"#),
    )));
    let client = accepted_client(transport);
    let lifecycle = ChainSubmissionLifecycle::new(&db, &client);
    let identity = ChainSubmissionIdentity::delegation(ROUND_ID, 0);

    let outcome = lifecycle
        .submit_canonical_payload_locked(WALLET, identity, b"{}".to_vec(), &echo_rebuild, &|| false)
        .await
        .unwrap();

    // Journaling it would make it a candidate confirmation must refuse, and
    // a successful candidate is never retired, so every later submission
    // would rediscover it and exit before dispatch — the payload could
    // never be sent again.
    assert!(
        matches!(&outcome, ChainLifecycleOutcome::OutcomeUnknown { message, .. }
                if message.contains("belongs to another submission")),
        "got {outcome:?}"
    );
    assert_eq!(attempt_states(&db), vec!["outcome_unknown".to_string()]);
    // The POST happened, so the ambiguity is real and kept — but hashless,
    // because the hash this call was told is not this submission's.
    assert_eq!(
        db.conn()
            .query_row(
                "SELECT COUNT(*) FROM chain_submission_attempts WHERE chain_tx_hash IS NOT NULL",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        0
    );
    assert_eq!(db.get_delegation_tx_hash(ROUND_ID, 0).unwrap(), None);
}

#[tokio::test]
async fn a_hashless_unknown_attempt_stops_covering_its_vote_row() {
    const ISOLATED_ROUND_ID: &str =
        "0000000000000000000000000000000000000000000000000000000000000002";
    const ISOLATED_WALLET: &str = "hashless-coverage-wallet";
    const BUNDLE_INDEX: u32 = 0;
    const PROPOSAL_ID: u32 = 314_159;

    let db = test_db();
    db.set_wallet_id(ISOLATED_WALLET);
    db.create_round(
        Network::Testnet,
        &RoundParams {
            vote_round_id: ISOLATED_ROUND_ID.to_string(),
            snapshot_height: 100,
            ea_pk: vec![0xEA; 32],
            nc_root: vec![0xAA; 32],
            nullifier_imt_root: vec![0xBB; 32],
        },
        None,
    )
    .unwrap();
    {
        let conn = db.conn();
        conn.execute(
            "INSERT INTO bundles (round_id, wallet_id, bundle_index) VALUES (?1, ?2, ?3)",
            rusqlite::params![ISOLATED_ROUND_ID, ISOLATED_WALLET, i64::from(BUNDLE_INDEX)],
        )
        .unwrap();
        queries::store_vote(
            &conn,
            ISOLATED_ROUND_ID,
            ISOLATED_WALLET,
            BUNDLE_INDEX,
            PROPOSAL_ID,
            1,
            &[0xCC; 32],
        )
        .unwrap();
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
    let identity = ChainSubmissionIdentity::vote(ISOLATED_ROUND_ID, BUNDLE_INDEX, PROPOSAL_ID);

    let outcome = lifecycle
        .submit_canonical_payload_locked(
            ISOLATED_WALLET,
            identity.clone(),
            b"{}".to_vec(),
            &echo_rebuild,
            &|| false,
        )
        .await
        .unwrap();

    // The first attempt may still commit, so the call stays ambiguous and
    // `has_ambiguous_attempt` keeps saying so across later calls.
    assert!(matches!(
        outcome,
        ChainLifecycleOutcome::OutcomeUnknown { .. }
    ));
    assert!(has_ambiguous_attempt(&db, ISOLATED_WALLET, &identity).unwrap());

    // But it never learned a transaction hash and never can, so it cannot
    // be looked up or confirmed. Covering the row would only freeze this
    // proposal's recovery generation, ballot intent, and bundle pruning for
    // the life of the round, because nothing ever retires such an attempt.
    let protected = {
        let conn = db.conn();
        attempt_protected_vote_rows(&conn, ISOLATED_ROUND_ID, ISOLATED_WALLET).unwrap()
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
        .submit_canonical_payload_locked(WALLET, identity, b"{}".to_vec(), &echo_rebuild, &|| false)
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
