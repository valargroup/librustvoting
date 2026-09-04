//! Bundle causality: predecessors, successors, and superseded delegations.

use super::super::*;
use super::fixtures::*;

#[test]
fn confirmed_predecessor_allows_the_next_bundle_generation() {
    let db = open_prepared(":memory:");
    let store = SqliteChainSubmissionStore::new(Arc::clone(&db));
    let request = StoreAdvancementRequest::vote(identity());
    let StoreAdmission::Ready { derived, .. } = store.admit(&request, true, 1, 10).unwrap() else {
        panic!("fresh admission")
    };
    let candidate = CandidateTransactionHash::from_bytes([0x44; 32]);
    store
        .classify_post(
            derived.generation(),
            SubmissionObservation::UsableCandidateHash(candidate),
            11,
        )
        .unwrap();
    store
        .confirm_committed(
            &request,
            derived.generation(),
            candidate,
            &committed(),
            &|| true,
            12,
        )
        .unwrap();

    crate::vote::insert_recovery_fixture(&db, &recovery_for(0, 2)).unwrap();
    assert!(matches!(
        store
            .admit(
                &StoreAdvancementRequest::vote(identity_for(0, 2)),
                true,
                1,
                13,
            )
            .unwrap(),
        StoreAdmission::Ready {
            fresh_reservation: true,
            ..
        }
    ));
}

/// Bundle causality is a property of the wallet, round, and bundle alone.
///
/// Under the required single-ledger-per-network deployment topology, the
/// configured vote-chain id is not part of a submission identity, so
/// reconfiguring it within that ledger cannot reserve a second generation
/// against the same unresolved predecessor VAN.

#[test]
fn active_predecessor_blocks_the_next_bundle_generation() {
    let db = open_prepared(":memory:");
    crate::vote::insert_recovery_fixture(&db, &recovery_for(0, 2)).unwrap();
    let store = SqliteChainSubmissionStore::new(db);
    let first = StoreAdvancementRequest::vote(identity_for(0, 1));
    assert!(matches!(
        store.admit(&first, true, 1, 10).unwrap(),
        StoreAdmission::Ready {
            fresh_reservation: true,
            ..
        }
    ));

    let failure = match store.admit(
        &StoreAdvancementRequest::vote(identity_for(0, 2)),
        true,
        1,
        11,
    ) {
        Err(failure) => failure,
        Ok(_) => panic!("an unresolved predecessor must block the next bundle generation"),
    };
    assert_eq!(failure.kind(), ChainSubmissionFailureKind::InvalidInput);
}

#[test]
fn hashless_submission_blocks_same_bundle_but_not_unrelated_bundles() {
    let db = open_prepared(":memory:");
    crate::vote::insert_recovery_fixture(&db, &recovery_for(0, 2)).unwrap();
    crate::storage::queries::insert_bundle(&db.conn(), ROUND, "wallet", 1, &[1]).unwrap();
    crate::vote::insert_recovery_fixture(&db, &recovery_for(1, 1)).unwrap();
    let store = SqliteChainSubmissionStore::new(db);
    let first = StoreAdvancementRequest::vote(identity_for(0, 1));
    let StoreAdmission::Ready { derived, .. } = store.admit(&first, true, 1, 10).unwrap() else {
        panic!("fresh predecessor")
    };
    let ambiguity = ChainSubmissionDiagnostic::from_redacted_message(
        ChainSubmissionDiagnosticKind::AmbiguousDispatch,
        "response unavailable",
    );
    store
        .classify_post(
            derived.generation(),
            SubmissionObservation::PossiblyDispatched(ambiguity),
            11,
        )
        .unwrap();
    store
        .classify_post(
            derived.generation(),
            SubmissionObservation::SubmittedWithoutHash(
                ChainSubmissionDiagnostic::from_redacted_message(
                    ChainSubmissionDiagnosticKind::AmbiguousAttemptsExhausted,
                    "attempts exhausted",
                ),
            ),
            12,
        )
        .unwrap();

    assert!(store
        .admit(
            &StoreAdvancementRequest::vote(identity_for(0, 2)),
            true,
            1,
            13,
        )
        .is_err());
    assert!(matches!(
        store
            .admit(
                &StoreAdvancementRequest::vote(identity_for(1, 1)),
                true,
                1,
                13,
            )
            .unwrap(),
        StoreAdmission::Ready {
            fresh_reservation: true,
            ..
        }
    ));
}

/// A confirmed vote has already consumed the bundle's delegation output,
/// so a later delegation reservation is refused before derivation instead
/// of creating an unresolvable row beside the confirmed successor.

#[test]
fn confirmed_vote_refuses_a_later_delegation_reservation() {
    let db = open_prepared(":memory:");
    db.conn()
        .execute(
            "INSERT INTO chain_submissions
               (identity_key, round_id, wallet_id, network, bundle_index, kind,
                proposal_id, generation_digest, state, committed_post_reservations,
                confirmation_source, final_van_position, vote_commitment_positions,
                created_at, updated_at)
             VALUES (?1, ?2, 'wallet', 'testnet', 0, 'vote', 1, ?3, 'confirmed', 1,
                     'tree', 7, ?4, 9, 9)",
            rusqlite::params![
                submission_identity_key(&identity_for(0, 1)),
                ROUND,
                vec![0x61_u8; 32],
                [vec![1, 0, 0, 0, 1], 8_u64.to_be_bytes().to_vec()].concat(),
            ],
        )
        .unwrap();
    let store = SqliteChainSubmissionStore::new(Arc::clone(&db));
    let delegation = ChainSubmissionIdentity::new(
        "wallet",
        crate::Network::Testnet,
        [0x11; 32],
        0,
        ChainSubmissionTarget::Delegation,
    )
    .unwrap();

    let failure = store
        .admit(
            &StoreAdvancementRequest::delegation(delegation, [7; 64]),
            true,
            1,
            10,
        )
        .err()
        .expect("a confirmed successor must refuse a delegation reservation");

    assert_eq!(failure.kind(), ChainSubmissionFailureKind::InvalidInput);
    assert!(failure.strongest_state().is_none());
    let delegation_rows: i64 = db
        .conn()
        .query_row(
            "SELECT count(*) FROM chain_submissions WHERE kind='delegation'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(delegation_rows, 0);
}
