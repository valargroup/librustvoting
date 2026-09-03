//! Admission: abandoned reservations, cancelled entry, and candidate reuse.

use super::super::*;
use super::fixtures::*;
use crate::storage::queries;

#[test]
fn restart_normalizes_unclassified_reservation_without_redispatch() {
    let path = temporary_path("abandoned");
    {
        let db = open_prepared(&path);
        let store = SqliteChainSubmissionStore::new(db);
        assert!(matches!(
            store
                .admit(&StoreAdvancementRequest::vote(identity()), true, 1, 10)
                .unwrap(),
            StoreAdmission::Ready {
                fresh_reservation: true,
                ..
            }
        ));
    }
    {
        let db = open_prepared(&path);
        let store = SqliteChainSubmissionStore::new(Arc::clone(&db));
        let admission = store
            .admit(&StoreAdvancementRequest::vote(identity()), true, 1, 20)
            .unwrap();
        let StoreAdmission::Authoritative(record) = admission else {
            panic!("restart must not derive or reserve")
        };
        assert_eq!(record.durable_state(), ChainSubmissionState::Recovering);
        assert_eq!(record.committed_post_reservations(), 1);
        let recovery_count: u64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM votes
                  WHERE round_id=?1 AND wallet_id='wallet'
                    AND commitment_bundle_json IS NOT NULL",
                [ROUND],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(recovery_count, 1);
    }
    let _ = std::fs::remove_file(path);
}

#[test]
fn abandoned_batch_normalizes_before_roster_derivation() {
    let db = open_prepared_in_memory();
    let digest = store_two_vote_batch(&db);
    let store = SqliteChainSubmissionStore::new(Arc::clone(&db));
    let request = StoreAdvancementRequest::vote_batch(batch_identity(digest), vec![1, 2])
        .expect("valid batch request");
    assert!(matches!(
        store.admit(&request, true, 1, 10).unwrap(),
        StoreAdmission::Ready {
            fresh_reservation: true,
            ..
        }
    ));
    db.conn()
        .execute(
            "UPDATE votes SET commitment_bundle_json=NULL
              WHERE round_id=?1 AND wallet_id='wallet'
                AND bundle_index=0 AND proposal_id=2",
            [ROUND],
        )
        .unwrap();

    let StoreAdmission::Authoritative(record) = store.admit(&request, true, 1, 20).unwrap() else {
        panic!("abandoned batch reservation must normalize without derivation")
    };
    assert_eq!(record.durable_state(), ChainSubmissionState::Recovering);
    assert_eq!(record.committed_post_reservations(), 1);
}

#[test]
fn cancelled_batch_returns_authoritative_state_before_stale_member() {
    let db = open_prepared_in_memory();
    let digest = store_two_vote_batch(&db);
    let store = SqliteChainSubmissionStore::new(Arc::clone(&db));
    let batch = batch_identity(digest);
    let initial_request = StoreAdvancementRequest::vote_batch(batch.clone(), vec![1, 2]).unwrap();
    assert!(matches!(
        store.admit(&initial_request, true, 1, 10).unwrap(),
        StoreAdmission::Ready {
            fresh_reservation: true,
            ..
        }
    ));
    let stale_member = identity_for(0, 3);
    db.conn()
        .execute(
            "INSERT INTO chain_submissions
               (identity_key, round_id, wallet_id, network, bundle_index, kind,
                proposal_id, generation_digest, state, committed_post_reservations,
                diagnostic_kind, diagnostic, created_at, updated_at)
             VALUES (?1, ?2, 'wallet', 'testnet', 0, 'vote', 3, ?3, 'recovering', 0,
                     'reconciliation_pending', 'possible dispatch awaits tree recovery', 9, 9)",
            rusqlite::params![
                submission_identity_key(&stale_member),
                ROUND,
                vec![0x33_u8; 32]
            ],
        )
        .unwrap();
    let stale_request = StoreAdvancementRequest::vote_batch(batch, vec![1, 3]).unwrap();

    let StoreAdmission::Authoritative(record) = store.admit(&stale_request, false, 1, 20).unwrap()
    else {
        panic!("cancelled entry must return the authoritative batch")
    };
    assert_eq!(record.durable_state(), ChainSubmissionState::Recovering);
    assert_eq!(record.committed_post_reservations(), 1);
}

#[test]
fn failed_abandoned_normalization_reports_possible_dispatch() {
    let db = open_prepared_in_memory();
    let store = SqliteChainSubmissionStore::new(Arc::clone(&db));
    store
        .admit(&StoreAdvancementRequest::vote(identity()), true, 1, 10)
        .unwrap();
    db.conn()
        .execute_batch(
            "CREATE TRIGGER fail_chain_submission_update
             BEFORE UPDATE ON chain_submissions
             BEGIN SELECT RAISE(ABORT, 'injected normalization failure'); END;",
        )
        .unwrap();

    let failure = match store.admit(&StoreAdvancementRequest::vote(identity()), true, 1, 20) {
        Err(failure) => failure,
        Ok(_) => panic!("normalization must fail"),
    };
    assert_eq!(
        failure.strongest_state().unwrap().evidence(),
        crate::chain_submission::ChainSubmissionStateEvidence::KnownPossiblyDispatched
    );
    let state: String = db
        .conn()
        .query_row("SELECT state FROM chain_submissions", [], |row| row.get(0))
        .unwrap();
    assert_eq!(state, "submitting");
}

#[test]
fn duplicate_candidate_hash_becomes_hashless_recovery() {
    let db = open_prepared_in_memory();
    queries::insert_bundle(&db.conn(), ROUND, "wallet", 1, &[2]).unwrap();
    crate::vote::insert_recovery_fixture(&db, &recovery_for(1, 2)).unwrap();
    let store = SqliteChainSubmissionStore::new(db);
    let candidate = CandidateTransactionHash::from_bytes([0x55; 32]);

    let StoreAdmission::Ready { derived: first, .. } = store
        .admit(&StoreAdvancementRequest::vote(identity()), true, 1, 10)
        .unwrap()
    else {
        panic!("first admission")
    };
    store
        .classify_post(
            first.generation(),
            SubmissionObservation::UsableCandidateHash(candidate),
            11,
        )
        .unwrap();

    let StoreAdmission::Ready {
        derived: second, ..
    } = store
        .admit(
            &StoreAdvancementRequest::vote(identity_for(1, 2)),
            true,
            1,
            12,
        )
        .unwrap()
    else {
        panic!("second admission")
    };
    let record = store
        .classify_post(
            second.generation(),
            SubmissionObservation::UsableCandidateHash(candidate),
            13,
        )
        .unwrap();
    assert!(matches!(
        record.state(),
        SubmissionRecordState::Recovering {
            candidate_transaction_hash: None,
            ambiguity_diagnostic,
        } if ambiguity_diagnostic.kind() == ChainSubmissionDiagnosticKind::InvalidProtocolResponse
    ));
}
