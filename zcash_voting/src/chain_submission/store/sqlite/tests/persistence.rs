//! Persistence: durable state across reopen and clock movement.

use super::super::*;
use super::fixtures::*;

#[test]
fn tracking_and_atomic_confirmation_survive_reopen() {
    let path = temporary_path("confirmed");
    let candidate = CandidateTransactionHash::from_bytes([0x44; 32]);
    let generation = {
        let db = open_prepared(&path);
        let store = SqliteChainSubmissionStore::new(db);
        let StoreAdmission::Ready { derived, .. } = store
            .admit(&StoreAdvancementRequest::vote(identity()), true, 1, 10)
            .unwrap()
        else {
            panic!("fresh admission")
        };
        let generation = derived.generation().clone();
        let tracking = store
            .classify_post(
                &generation,
                SubmissionObservation::UsableCandidateHash(candidate),
                11,
            )
            .unwrap();
        assert_eq!(tracking.durable_state(), ChainSubmissionState::Tracking);
        let diagnostic = ChainSubmissionDiagnostic::from_redacted_message(
            ChainSubmissionDiagnosticKind::ReconciliationPending,
            "candidate lookup temporarily unavailable",
        );
        let tracking = store
            .reconcile(
                &generation,
                SubmissionObservation::CandidatePending,
                Some(diagnostic.clone()),
                12,
            )
            .unwrap();
        assert_eq!(tracking.diagnostic(), Some(&diagnostic));
        generation
    };
    {
        let db = open_prepared(&path);
        let store = SqliteChainSubmissionStore::new(Arc::clone(&db));
        let StoreAdmission::Ready { record, .. } = store
            .admit(&StoreAdvancementRequest::vote(identity()), true, 1, 13)
            .unwrap()
        else {
            panic!("tracking must reconcile")
        };
        assert_eq!(record.tracking_started_at(), Some(11));
        assert_eq!(
            record.diagnostic().map(ChainSubmissionDiagnostic::message),
            Some("candidate lookup temporarily unavailable")
        );
        let committed = committed();
        let result = store
            .confirm_committed(
                &StoreAdvancementRequest::vote(identity()),
                &generation,
                candidate,
                &committed,
                &|| true,
                14,
            )
            .unwrap();
        assert!(matches!(result, ConfirmationCommit::Confirmed(_)));
        let expected_hash = candidate.to_string();
        assert_eq!(
            db.get_vote_tx_hash(ROUND, 0, 1).unwrap().as_deref(),
            Some(expected_hash.as_str())
        );
    }
    {
        let db = open_prepared(&path);
        let store = SqliteChainSubmissionStore::new(db);
        let StoreAdmission::Authoritative(record) = store
            .admit(&StoreAdvancementRequest::vote(identity()), true, 1, 15)
            .unwrap()
        else {
            panic!("terminal state must be authoritative")
        };
        assert_eq!(record.durable_state(), ChainSubmissionState::Confirmed);
    }
    let _ = std::fs::remove_file(path);
}

#[test]
fn lifecycle_timestamps_clamp_when_wall_clock_moves_backward() {
    let db = open_prepared(":memory:");
    let store = SqliteChainSubmissionStore::new(db);
    let StoreAdmission::Ready { derived, .. } = store
        .admit(&StoreAdvancementRequest::vote(identity()), true, 1, 100)
        .unwrap()
    else {
        panic!("fresh admission")
    };
    let candidate = CandidateTransactionHash::from_bytes([0x66; 32]);
    let tracking = store
        .classify_post(
            derived.generation(),
            SubmissionObservation::UsableCandidateHash(candidate),
            90,
        )
        .unwrap();
    assert_eq!(tracking.tracking_started_at(), Some(100));
    assert_eq!(tracking.updated_at(), 100);

    let recovering = store
        .reconcile(
            derived.generation(),
            SubmissionObservation::TrackingWindowExpired(
                ChainSubmissionDiagnostic::from_redacted_message(
                    ChainSubmissionDiagnosticKind::TrackingWindowExpired,
                    "clock rollback expires tracking conservatively",
                ),
            ),
            None,
            80,
        )
        .unwrap();
    assert_eq!(recovering.durable_state(), ChainSubmissionState::Recovering);
    assert_eq!(recovering.updated_at(), 100);
}
