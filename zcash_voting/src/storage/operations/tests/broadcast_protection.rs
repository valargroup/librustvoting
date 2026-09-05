//! What may be discarded, pruned or deleted once part of a round has reached
//! the network, and what must survive because it is the only thing that can
//! reproduce the round's voting weight.

use super::fixtures::*;
use super::*;

/// Nothing left the device, so the setup that cannot be used is cleared and
/// the bundle is ready for setup to run again.
#[test]
fn unbroadcast_delegation_setup_is_discardable() {
    let db = db_with_delegation_setup(1);
    queries::store_proof(&db.conn(), ROUND_ID, W, 0, &[0x01; 8]).unwrap();

    assert_eq!(
        db.delegation_broadcast_evidence(ROUND_ID, None).unwrap(),
        None
    );
    assert_eq!(
        db.discard_unbroadcast_delegation(ROUND_ID, None).unwrap(),
        1
    );

    assert_eq!(van_comm_rand_of(&db, 0), None);
    // The proof went with it: a proof over a target the wallet can no
    // longer sign for is not work worth keeping.
    let proofs: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM proofs WHERE round_id = ?1 AND wallet_id = ?2",
            rusqlite::params![ROUND_ID, W],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(proofs, 0);
    // The round and its note positions survive, so setup can rebuild.
    assert!(db.has_round(ROUND_ID).unwrap());
    assert_eq!(db.get_bundle_count(ROUND_ID).unwrap(), 1);
}

/// A failure in any discard statement rolls the entire transition back, so
/// setup and its derived artifacts cannot persist in different generations.
#[test]
fn delegation_setup_discard_is_atomic() {
    let db = db_with_delegation_setup(1);
    queries::store_proof(&db.conn(), ROUND_ID, W, 0, &[0x01; 8]).unwrap();
    db.conn()
        .execute_batch(
            "CREATE TEMP TRIGGER reject_delegation_proof_delete
             BEFORE DELETE ON proofs
             BEGIN
                 SELECT RAISE(ABORT, 'injected proof deletion failure');
             END;",
        )
        .unwrap();

    let error = db
        .discard_unbroadcast_delegation(ROUND_ID, None)
        .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("injected proof deletion failure"),
        "{error:?}"
    );
    assert_eq!(van_comm_rand_of(&db, 0), Some(vec![0x11; 32]));
    let proof_count: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM proofs
              WHERE round_id = ?1 AND wallet_id = ?2 AND bundle_index = 0",
            rusqlite::params![ROUND_ID, W],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(proof_count, 1);
}

/// The discard is safe to repeat: a host retrying recovery must not need to
/// know whether the first attempt landed.
#[test]
fn discarding_unbroadcast_delegation_twice_is_idempotent() {
    let db = db_with_delegation_setup(1);
    assert_eq!(
        db.discard_unbroadcast_delegation(ROUND_ID, None).unwrap(),
        1
    );
    assert_eq!(
        db.discard_unbroadcast_delegation(ROUND_ID, None).unwrap(),
        1
    );
    assert_eq!(van_comm_rand_of(&db, 0), None);
}

/// Every fact that says a delegation may exist on chain refuses the
/// discard, and refuses it without touching a single byte of setup.
///
/// This is the invariant the whole recovery story rests on: once the
/// governance nullifiers are spent, `van_comm_rand` and the hotkey target
/// it commits to are the only things that can reproduce the round's voting
/// weight, and no recovery path may clear them.
#[test]
fn broadcast_delegation_setup_is_never_discarded() {
    for (label, seed) in [
        (
            "delegation tx hash",
            (|db: &VotingDb| {
                queries::store_delegation_tx_hash(
                    &db.conn(),
                    ROUND_ID,
                    W,
                    0,
                    "ab".repeat(32).as_str(),
                )
                .unwrap();
            }) as fn(&VotingDb),
        ),
        ("van leaf position", |db: &VotingDb| {
            queries::store_van_position(&db.conn(), ROUND_ID, W, 0, 7).unwrap();
        }),
        ("chain submission", |db: &VotingDb| {
            insert_chain_submission(db, 0);
        }),
        ("vote", |db: &VotingDb| {
            insert_vote_row(db, 0);
        }),
    ] {
        let db = db_with_delegation_setup(1);
        seed(&db);

        let evidence = db.delegation_broadcast_evidence(ROUND_ID, None).unwrap();
        assert!(evidence.is_some(), "{label}: evidence must be reported");

        let error = db
            .discard_unbroadcast_delegation(ROUND_ID, None)
            .unwrap_err();
        assert!(
            matches!(error, VotingError::DelegationAlreadyBroadcast { .. }),
            "{label}: {error:?}"
        );
        assert_eq!(
            van_comm_rand_of(&db, 0),
            Some(vec![0x11; 32]),
            "{label}: setup must survive a refused discard"
        );

        // The blunt path is refused for the same reason.
        let error = db.clear_round(ROUND_ID).unwrap_err();
        assert!(
            matches!(error, VotingError::DelegationAlreadyBroadcast { .. }),
            "{label}: {error:?}"
        );
        assert!(db.has_round(ROUND_ID).unwrap(), "{label}");
    }
}

/// A round that is half committed still recovers the half that is not, and
/// the committed bundle keeps every byte it needs.
#[test]
fn discarding_one_bundle_leaves_a_broadcast_sibling_intact() {
    let db = db_with_delegation_setup(2);
    queries::store_van_position(&db.conn(), ROUND_ID, W, 1, 7).unwrap();

    assert_eq!(
        db.discard_unbroadcast_delegation(ROUND_ID, Some(0))
            .unwrap(),
        1
    );

    assert_eq!(van_comm_rand_of(&db, 0), None);
    assert_eq!(van_comm_rand_of(&db, 1), Some(vec![0x11; 32]));

    // And the broadcast bundle still refuses its own discard.
    let error = db
        .discard_unbroadcast_delegation(ROUND_ID, Some(1))
        .unwrap_err();
    assert!(
        matches!(error, VotingError::DelegationAlreadyBroadcast { .. }),
        "{error:?}"
    );
}

/// The round-wide discard refuses while any bundle is committed, rather
/// than clearing the rest and leaving the round half rebuilt.
#[test]
fn round_wide_discard_refuses_when_any_bundle_is_broadcast() {
    let db = db_with_delegation_setup(2);
    insert_chain_submission(&db, 1);

    let error = db
        .discard_unbroadcast_delegation(ROUND_ID, None)
        .unwrap_err();
    assert!(
        matches!(error, VotingError::DelegationAlreadyBroadcast { .. }),
        "{error:?}"
    );
    assert_eq!(van_comm_rand_of(&db, 0), Some(vec![0x11; 32]));
    assert_eq!(van_comm_rand_of(&db, 1), Some(vec![0x11; 32]));
}

/// The write carries its own guard, so a submission that lands between the
/// check and the delete still keeps its setup.
///
/// The pre-check exists to produce a good message, not to be the defence:
/// a host holding a stale answer, or a chain submission opening in another
/// thread, must not be able to widen what the delete touches.
#[test]
fn a_submission_racing_the_discard_keeps_its_setup() {
    let db = db_with_delegation_setup(1);
    // Stand in for the racing writer: evidence appears after a caller has
    // already decided the bundle looked discardable.
    assert_eq!(
        db.delegation_broadcast_evidence(ROUND_ID, None).unwrap(),
        None
    );
    insert_chain_submission(&db, 0);

    let cleared =
        queries::discard_unbroadcast_delegation_setup(&db.conn(), ROUND_ID, W, None).unwrap();

    assert_eq!(cleared, 0, "the guarded write must skip a broadcast bundle");
    assert_eq!(van_comm_rand_of(&db, 0), Some(vec![0x11; 32]));
}

/// Pruning refuses a bundle whose delegation may be on chain, including one
/// delegated before the chain-submission lifecycle existed.
///
/// Those legacy rounds carry only a transaction hash or a VAN position, so
/// a guard that reads chain-submission rows alone sees nothing and prunes
/// away the `van_comm_rand` that their spent nullifiers make irreplaceable.
#[test]
fn pruning_refuses_every_bundle_that_may_be_on_chain() {
    for (label, seed) in [
        (
            "legacy delegation tx hash",
            (|db: &VotingDb| {
                queries::store_delegation_tx_hash(
                    &db.conn(),
                    ROUND_ID,
                    W,
                    1,
                    "ab".repeat(32).as_str(),
                )
                .unwrap();
            }) as fn(&VotingDb),
        ),
        ("legacy van position", |db: &VotingDb| {
            queries::store_van_position(&db.conn(), ROUND_ID, W, 1, 7).unwrap();
        }),
        ("chain submission", |db: &VotingDb| {
            insert_chain_submission(db, 1);
        }),
        ("vote", |db: &VotingDb| {
            insert_vote_row(db, 1);
        }),
    ] {
        let db = db_with_delegation_setup(2);
        seed(&db);

        let error = db.delete_skipped_bundles(ROUND_ID, 1).unwrap_err();
        assert!(
            matches!(error, VotingError::Busy { .. }),
            "{label}: {error:?}"
        );
        assert_eq!(db.get_bundle_count(ROUND_ID).unwrap(), 2, "{label}");
        assert_eq!(
            van_comm_rand_of(&db, 1),
            Some(vec![0x11; 32]),
            "{label}: setup must survive a refused prune"
        );
    }
}

/// A bundle that never left the device still prunes, so the guard above
/// cannot be satisfied by refusing everything.
#[test]
fn pruning_still_drops_an_unbroadcast_bundle() {
    let db = db_with_delegation_setup(2);
    assert_eq!(db.delete_skipped_bundles(ROUND_ID, 1).unwrap(), 1);
    assert_eq!(db.get_bundle_count(ROUND_ID).unwrap(), 1);
}

/// Deliberate destruction stays possible, and stays separately named.
#[test]
fn clear_round_discarding_recovery_still_deletes_a_broadcast_round() {
    let db = db_with_delegation_setup(1);
    queries::store_van_position(&db.conn(), ROUND_ID, W, 0, 7).unwrap();

    assert!(db.clear_round(ROUND_ID).is_err());
    db.clear_round_discarding_recovery(ROUND_ID).unwrap();
    assert!(!db.has_round(ROUND_ID).unwrap());
}

/// The gate is taken before anything is read or deleted, so a submission
/// already running keeps the round it depends on.
#[test]
fn clear_round_takes_the_submission_gate_before_it_deletes_anything() {
    let db = db_with_delegation_setup(1);
    let identity = db
        .chain_submission_round_identity(ROUND_ID, W)
        .unwrap()
        .expect("a round with a network has a submission identity");
    let _held = db
        .chain_submission_coordination()
        .try_acquire_round_exclusive(&identity)
        .map_err(|_| "the gate is free")
        .unwrap();

    let error = db.clear_round(ROUND_ID).unwrap_err();

    assert!(matches!(error, VotingError::Busy { .. }), "{error:?}");
    assert!(db.has_round(ROUND_ID).unwrap());
}

/// The evidence check lives in the transaction that deletes, not only in
/// the advisory check above it, so evidence written after that check still
/// refuses the delete.
#[test]
fn clear_round_rechecks_evidence_in_the_transaction_that_deletes() {
    let db = db_with_delegation_setup(1);
    queries::store_van_position(&db.conn(), ROUND_ID, W, 0, 7).unwrap();

    let refused = queries::clear_round_if_unbroadcast(&db.conn(), ROUND_ID, W)
        .unwrap()
        .expect("broadcast evidence refuses the delete");

    assert_eq!(refused.0, 0);
    assert_eq!(
        refused.1,
        queries::DelegationBroadcastEvidence::VanLeafPosition
    );
    assert!(db.has_round(ROUND_ID).unwrap());
    assert_eq!(van_comm_rand_of(&db, 0), Some(vec![0x11; 32]));
}
