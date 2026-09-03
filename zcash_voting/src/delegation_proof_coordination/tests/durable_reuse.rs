use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Barrier, Condvar, Mutex,
    },
    thread,
};

use crate::{
    delegate::{ensure_proof, DelegationProgress, DelegationProofStatus},
    types::DelegationProgressBridge,
    Network, VotingError,
};

use super::{
    super::{coordinate, DelegationProofIdentity},
    fixtures::{
        db_with_persisted_proofs, keys, keys_for_hotkey, note, pir_client, ROUND_ID, WALLET_A,
        WALLET_A_PROOF_BYTE, WALLET_B,
    },
};

#[test]
fn reused_proof_rejects_mismatched_notes() {
    let db = db_with_persisted_proofs();
    let mut mismatched_note = note();
    mismatched_note.commitment[0] ^= 1;

    let error = ensure_proof(
        &db,
        ROUND_ID,
        0,
        &[mismatched_note],
        &keys(Network::Testnet, 1),
        &pir_client(),
        &crate::types::NoopProgressReporter,
    )
    .expect_err("a persisted proof must not bypass bundle-note validation");

    assert!(matches!(error, VotingError::InvalidInput { .. }));
    assert!(
        error
            .to_string()
            .contains("note identity mismatch at index 0"),
        "{error}"
    );
}

#[test]
fn reused_proof_rejects_mismatched_keys() {
    let db = db_with_persisted_proofs();
    let selected_note = note();

    for (mismatched_keys, expected_message) in [
        (
            keys(Network::Mainnet, 1),
            "delegation keys network Mainnet does not match stored round network Testnet",
        ),
        (
            keys(Network::Testnet, 2),
            "voting target round does not match delegation round",
        ),
        (
            keys_for_hotkey(Network::Testnet, 1, 0x22),
            "delegation keys hotkey target does not match stored bundle target",
        ),
    ] {
        let error = ensure_proof(
            &db,
            ROUND_ID,
            0,
            std::slice::from_ref(&selected_note),
            &mismatched_keys,
            &pir_client(),
            &crate::types::NoopProgressReporter,
        )
        .expect_err("a persisted proof must not bypass delegation-key validation");

        assert!(matches!(error, VotingError::InvalidInput { .. }), "{error}");
        assert!(error.to_string().contains(expected_message), "{error}");
    }
}

#[test]
fn reentrant_progress_reporter_reuses_after_lock_release() {
    let db = Arc::new(db_with_persisted_proofs());
    let reentrant_status = Arc::new(Mutex::new(None));
    let callback_status = Arc::clone(&reentrant_status);
    let callback_db = Arc::clone(&db);
    let progress = DelegationProgressBridge::new(move |event| {
        if event == DelegationProgress::ProofComplete {
            let completion = ensure_proof(
                &callback_db,
                ROUND_ID,
                0,
                &[note()],
                &keys(Network::Testnet, 1),
                &pir_client(),
                &crate::types::NoopProgressReporter,
            )
            .expect("deferred callback must run after the proof lock is released");
            *callback_status.lock().unwrap() = Some(completion.status);
        }
    });

    let completion = ensure_proof(
        &db,
        ROUND_ID,
        0,
        &[note()],
        &keys(Network::Testnet, 1),
        &pir_client(),
        &progress,
    )
    .unwrap();

    assert_eq!(completion.status, DelegationProofStatus::Reused);
    assert_eq!(
        reentrant_status.lock().unwrap().take(),
        Some(DelegationProofStatus::Reused)
    );
}

#[test]
fn cross_thread_reentrant_progress_reporter_reuses_after_lock_release() {
    let db = Arc::new(db_with_persisted_proofs());
    let reentrant_status = Arc::new(Mutex::new(None));
    let callback_status = Arc::clone(&reentrant_status);
    let callback_db = Arc::clone(&db);
    let progress = DelegationProgressBridge::new(move |event| {
        if event == DelegationProgress::ProofComplete {
            let worker_db = Arc::clone(&callback_db);
            let nested = thread::spawn(move || {
                ensure_proof(
                    &worker_db,
                    ROUND_ID,
                    0,
                    &[note()],
                    &keys(Network::Testnet, 1),
                    &pir_client(),
                    &crate::types::NoopProgressReporter,
                )
            })
            .join()
            .unwrap()
            .expect("deferred callback worker must not wait on the released proof lock");
            *callback_status.lock().unwrap() = Some(nested.status);
        }
    });

    let completion = ensure_proof(
        &db,
        ROUND_ID,
        0,
        &[note()],
        &keys(Network::Testnet, 1),
        &pir_client(),
        &progress,
    )
    .unwrap();

    assert_eq!(completion.status, DelegationProofStatus::Reused);
    assert_eq!(
        reentrant_status.lock().unwrap().take(),
        Some(DelegationProofStatus::Reused)
    );
}

#[test]
fn cross_bundle_reentrant_progress_reporter_enters_after_lock_release() {
    let db = Arc::new(db_with_persisted_proofs());
    let reentrant_error = Arc::new(Mutex::new(None));
    let callback_error = Arc::clone(&reentrant_error);
    let callback_db = Arc::clone(&db);
    let progress = DelegationProgressBridge::new(move |event| {
        if event == DelegationProgress::ProofComplete {
            let error = ensure_proof(
                &callback_db,
                ROUND_ID,
                1,
                &[note()],
                &keys(Network::Testnet, 1),
                &pir_client(),
                &crate::types::NoopProgressReporter,
            )
            .expect_err("the fixture has no second bundle");
            *callback_error.lock().unwrap() = Some(error);
        }
    });

    let completion = ensure_proof(
        &db,
        ROUND_ID,
        0,
        &[note()],
        &keys(Network::Testnet, 1),
        &pir_client(),
        &progress,
    )
    .unwrap();

    assert_eq!(completion.status, DelegationProofStatus::Reused);
    assert!(matches!(
        reentrant_error.lock().unwrap().take(),
        Some(VotingError::InvalidInput { .. })
    ));
}

#[test]
fn rejected_delegation_reuses_persisted_proof() {
    let db = db_with_persisted_proofs();
    db.conn()
        .execute(
            "INSERT INTO chain_submissions
             (identity_key, round_id, wallet_id, network, bundle_index, kind,
              generation_digest, state, committed_post_reservations, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'testnet', 0, 'delegation', ?4, 'rejected', 1, 10, 10)",
            rusqlite::params![vec![0x51u8; 32], ROUND_ID, WALLET_A, vec![0x52u8; 32]],
        )
        .unwrap();

    let completion = ensure_proof(
        &db,
        ROUND_ID,
        0,
        &[note()],
        &keys(Network::Testnet, 1),
        &pir_client(),
        &crate::types::NoopProgressReporter,
    )
    .unwrap();

    assert_eq!(completion.status, DelegationProofStatus::Reused);
    assert_eq!(completion.proof.bytes, vec![WALLET_A_PROOF_BYTE; 96]);
}

#[test]
fn wallet_switch_does_not_retarget_waiting_proof() {
    let db = Arc::new(db_with_persisted_proofs());
    let leader_started = Arc::new(Barrier::new(2));
    let release_leader = Arc::new((Mutex::new(false), Condvar::new()));

    let leader = {
        let leader_started = Arc::clone(&leader_started);
        let release_leader = Arc::clone(&release_leader);
        thread::spawn(move || {
            coordinate(
                DelegationProofIdentity::new(WALLET_A.to_string(), ROUND_ID, 0),
                || {},
                |_| {
                    leader_started.wait();
                    let (released, wake) = &*release_leader;
                    let mut released = released.lock().unwrap();
                    while !*released {
                        released = wake.wait(released).unwrap();
                    }
                    Ok(())
                },
            )
        })
    };
    leader_started.wait();

    let wallet_switched = Arc::new(AtomicBool::new(false));
    let follower = {
        let db = Arc::clone(&db);
        let wallet_switched = Arc::clone(&wallet_switched);
        thread::spawn(move || {
            let progress_db = Arc::clone(&db);
            let progress = DelegationProgressBridge::new(move |event| {
                if event == DelegationProgress::WaitingForExistingProof {
                    progress_db.set_wallet_id(WALLET_B);
                    wallet_switched.store(true, Ordering::SeqCst);
                }
            });
            ensure_proof(
                &db,
                ROUND_ID,
                0,
                &[note()],
                &keys(Network::Testnet, 1),
                &pir_client(),
                &progress,
            )
        })
    };

    while !wallet_switched.load(Ordering::SeqCst) {
        thread::yield_now();
    }
    let (released, wake) = &*release_leader;
    *released.lock().unwrap() = true;
    wake.notify_all();

    leader.join().unwrap().unwrap();
    let completion = follower.join().unwrap().unwrap();
    assert_eq!(completion.status, DelegationProofStatus::Reused);
    assert_eq!(completion.proof.bytes, vec![WALLET_A_PROOF_BYTE; 96]);
    assert_eq!(db.wallet_id(), WALLET_B);
}
