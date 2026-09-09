//! Recovery ownership after replaced POST responses and wall-clock rollback.

use super::*;
use crate::chain_submission::{
    state::SubmissionObservation,
    store::{
        ChainSubmissionStore, SqliteChainSubmissionStore, StoreAdmission, StoreAdvancementRequest,
    },
};

#[tokio::test]
async fn replaced_post_response_keeps_combined_recovery_and_ballot_locked() {
    // The transport has received the complete POST when it returns this
    // response. A forwarding proxy can replace the response after the chain
    // accepted the envelope; neither HTML nor an error status proves absence.
    for (status, content_type) in [
        (200, "text/html"),
        (404, "application/json"),
        (405, "text/plain"),
    ] {
        let (db, request) = fixture(2);
        let before =
            crate::delegate_and_vote_batch::recover_delegate_and_vote_batch(&db, ROUND, 0, 1)
                .unwrap();
        let transport = Arc::new(ScriptedPosts {
            inner: Transport {
                request: request.clone(),
                calls: Mutex::new(Vec::new()),
            },
            posts: Mutex::new(vec![ChainHttpResponse::new(
                status,
                b"<html>replacement response</html>".to_vec(),
                Some(content_type.to_owned()),
                Vec::new(),
            )]),
        });
        let client = ChainSubmissionClient::with_transport(
            db.clone(),
            transport.clone(),
            ChainSubmissionClientConfig::for_network(
                crate::Network::Testnet,
                vec!["https://vote.example".to_owned()],
            )
            .with_post_attempts(1, Vec::new()),
        )
        .unwrap();
        let control = ChainSubmissionControl::new(1);
        let pending = client
            .advance_delegate_and_vote_batch(
                request.clone(),
                ChainRecoveryMode::StatusOnly,
                &control,
            )
            .await
            .unwrap();
        assert!(matches!(
            pending,
            ChainSubmissionResult::Pending(ChainSubmissionPending::Recovering {
                candidate_transaction_hash: None,
                ..
            })
        ));
        assert_eq!(transport.inner.calls.lock().unwrap().len(), 1);
        assert_eq!(count(&db, "chain_submissions"), 1);
        assert_eq!(count(&db, "delegate_cast_recovery"), 1);
        assert!(db
            .set_ballot_intent(ROUND, 1, crate::session::Decision::Skipped, 3)
            .is_err());
        assert!(db
            .retire_undispatched_votes_outside_roster(ROUND, 0, &[])
            .unwrap()
            .is_empty());
        let after =
            crate::delegate_and_vote_batch::recover_delegate_and_vote_batch(&db, ROUND, 0, 2)
                .unwrap();
        assert_eq!(after.batch_json, before.batch_json);

        // Reopen before retrying: the diagnostic must preserve both recovery
        // ownership and retry eligibility without invocation-local evidence.
        let path = std::env::temp_dir().join(format!(
            "combined-replaced-response-{}.sqlite",
            rand::random::<u64>()
        ));
        db.conn()
            .execute("VACUUM INTO ?1", [path.to_str().unwrap()])
            .unwrap();
        drop(client);
        drop(db);
        let db = Arc::new(VotingDb::open(path.to_str().unwrap()).unwrap());
        db.set_wallet_id("wallet-1");
        let restored =
            crate::delegate_and_vote_batch::recover_delegate_and_vote_batch(&db, ROUND, 0, 1)
                .unwrap();
        assert_eq!(restored.batch_json, before.batch_json);
        assert!(db
            .set_ballot_intent(ROUND, 1, crate::session::Decision::Skipped, 3)
            .is_err());
        let client = client_over(&db, transport.clone());
        // A later same-generation retry recovers a usable receipt and confirms
        // every member; the response replacement did not free a recast.
        let confirmed = client
            .advance_delegate_and_vote_batch(request, ChainRecoveryMode::StatusOnly, &control)
            .await
            .unwrap();
        assert!(matches!(confirmed, ChainSubmissionResult::Confirmed(_)));
        assert_eq!(
            db.delegation_phase(ROUND, 0).unwrap(),
            crate::phases::DelegationPhase::Confirmed
        );
        for proposal in 1..=2 {
            assert_eq!(
                db.get_vote_tx_hash(ROUND, 0, proposal).unwrap().as_deref(),
                Some(HASH)
            );
        }
        drop(client);
        drop(db);
        std::fs::remove_file(path).unwrap();
    }
}

#[tokio::test]
async fn rejection_after_replaced_post_response_cannot_retire_combined_recovery() {
    for status in [200, 404, 405] {
        let (db, request) = fixture(2);
        let transport = Arc::new(ScriptedPosts {
            inner: Transport {
                request: request.clone(),
                calls: Mutex::new(Vec::new()),
            },
            // ScriptedPosts consumes from the end: replaced response, then rejection.
            posts: Mutex::new(vec![
                rejection(7, &request),
                ChainHttpResponse::new(
                    status,
                    b"<html>fallback</html>".to_vec(),
                    Some("text/html".to_owned()),
                    Vec::new(),
                ),
            ]),
        });
        let failure = client_over(&db, transport.clone())
            .advance_delegate_and_vote_batch(
                request,
                ChainRecoveryMode::StatusOnly,
                &ChainSubmissionControl::new(1),
            )
            .await
            .unwrap_err();
        assert_eq!(
            failure.strongest_state().unwrap().state(),
            ChainSubmissionState::Recovering
        );
        assert_eq!(transport.inner.calls.lock().unwrap().len(), 2);
        assert_eq!(count(&db, "chain_submissions"), 1);
        assert_eq!(count(&db, "delegate_cast_recovery"), 1);
        assert!(rejection_ledger(&db).is_none());
        for proposal in 1..=2 {
            assert!(crate::vote::recovery_bundle(&db, ROUND, 0, proposal)
                .unwrap()
                .is_some());
        }
    }
}

/// Drives the actual SQLite admission and rejection transaction at fixed times.
fn reject_at(db: &Arc<VotingDb>, request: &AdvanceVoteBatch, reserved_at: u64, rejected_at: u64) {
    let identity = ChainSubmissionIdentity::new(
        "wallet-1",
        crate::Network::Testnet,
        request.vote_round_id,
        request.bundle_index,
        ChainSubmissionTarget::DelegateAndVoteBatch {
            ordered_batch_digest: request.ordered_batch_digest,
        },
    )
    .unwrap();
    let store = SqliteChainSubmissionStore::new(db.clone());
    let StoreAdmission::Ready { derived, .. } = store
        .admit(
            &StoreAdvancementRequest::vote_batch(identity, request.ordered_proposal_ids.clone())
                .unwrap(),
            true,
            1,
            reserved_at,
        )
        .unwrap()
    else {
        panic!("fresh combined admission")
    };
    let rejected = store
        .classify_post(
            derived.generation(),
            SubmissionObservation::TerminalRejection(
                ChainSubmissionDiagnostic::from_redacted_message(
                    ChainSubmissionDiagnosticKind::ChainRejected,
                    "round closed",
                ),
            ),
            rejected_at,
        )
        .unwrap();
    assert_eq!(rejected.durable_state(), ChainSubmissionState::Rejected);
    assert_retired_to_proved(db, 2);
}

fn rejection_times(db: &VotingDb) -> (u64, u64) {
    db.conn()
        .query_row(
            "SELECT first_rejected_at, last_rejected_at FROM combined_cast_rejections",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
}

#[test]
fn combined_rejection_retirement_clamps_clock_rollback_and_advances_the_streak() {
    let (db, mut request) = fixture(2);
    let signature = stored_signature(&db);
    for (index, (reserved_at, rejected_at, expected_last)) in [
        (1000, 900, 1000),  // Clock rollback within the first submission.
        (800, 700, 1000),   // New submission predates the first rejection.
        (1200, 1200, 1200), // Forward movement advances the ledger.
        (1100, 1050, 1200), // Above the first rejection, below the latest.
        (1200, 1200, 1200), // Equal timestamps still count another rejection.
        (0, 0, 1200),       // Unix epoch is a valid clock boundary.
    ]
    .into_iter()
    .enumerate()
    {
        if index != 0 {
            request = persist_combined_batch(&db, signature, 2, index as u8);
        }
        reject_at(&db, &request, reserved_at, rejected_at);
        assert_eq!(rejection_times(&db), (1000, expected_last));
        assert_eq!(rejection_ledger(&db).unwrap().0, index as u32 + 1);
        assert_eq!(blocked_bundles(&db).contains_key(&0), index >= 1);
    }
}

#[test]
fn a_new_delegation_generation_restarts_rejection_timestamps_after_clock_rollback() {
    let (db, request) = fixture(2);
    let signature = stored_signature(&db);
    reject_at(&db, &request, 1000, 1000);
    let old_generation = rejection_ledger(&db).unwrap().1;
    // The fake proof bytes are part of the delegation generation. Replacing
    // the fixture proof models a newly prepared delegation without proving.
    db.conn()
        .execute("UPDATE proofs SET proof = ?1", [vec![0x75; 96]])
        .unwrap();
    let recast = persist_combined_batch(&db, signature, 2, 1);
    reject_at(&db, &recast, 800, 700);
    assert_eq!(rejection_times(&db), (800, 800));
    let (streak, generation, _) = rejection_ledger(&db).unwrap();
    assert_eq!(streak, 1);
    assert_ne!(generation, old_generation);
    assert!(blocked_bundles(&db).is_empty());
}
