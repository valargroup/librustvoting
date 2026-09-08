use super::{tests::prepared_wallet_delegation_fixture, *};
use crate::{storage::queries, types::NoopProgressReporter};
use std::sync::{Arc, Barrier};

#[test]
fn keystone_request_reuses_warmed_setup_and_survives_restart() {
    let (_, params, _, prepared) = prepared_wallet_delegation_fixture();
    let path =
        std::env::temp_dir().join(format!("keystone-warmup-{}.sqlite", uuid::Uuid::new_v4()));
    let path = path.to_str().unwrap();
    let db = VotingDb::open(path).unwrap();
    db.set_wallet_id("keystone");
    db.ensure_round(Network::Regtest, &params, None).unwrap();
    db.ensure_bundles(&prepared.round_id, &prepared.bundle_note_infos)
        .unwrap();
    let setup = prepared.setup(&db, &NoopProgressReporter).unwrap();
    // The actual proof coordination and generation are covered by the prover
    // suite. Here a durable proof row exercises post-proof request retrieval.
    queries::store_proof(&db.conn(), &prepared.round_id, "keystone", 0, &[0xAB; 96]).unwrap();
    let before = prepared
        .keystone_request(&db, &NoopProgressReporter)
        .unwrap();
    assert_eq!(before.pczt_bytes, setup.pczt_bytes);
    assert_eq!(before.pczt_sighash, setup.pczt_sighash);
    drop(db);
    let reopened = VotingDb::open(path).unwrap();
    reopened.set_wallet_id("keystone");
    let after = prepared
        .keystone_request(&reopened, &NoopProgressReporter)
        .unwrap();
    assert_eq!(before, after);
    assert!(reopened
        .delegation_phase(&prepared.round_id, 0)
        .unwrap()
        .has_persisted_proof());
    drop(reopened);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn concurrent_keystone_requests_reuse_setup_after_busy_retry() {
    let (db, _, _, prepared) = prepared_wallet_delegation_fixture();
    let db = Arc::new(db);
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let db = Arc::clone(&db);
            let barrier = Arc::clone(&barrier);
            let prepared = prepared.clone();
            std::thread::spawn(move || {
                barrier.wait();
                prepared.keystone_request(&db, &NoopProgressReporter)
            })
        })
        .collect();
    let attempts: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert!(attempts.iter().any(Result::is_ok));
    let requests: Vec<_> = attempts
        .into_iter()
        .map(|attempt| match attempt {
            Ok(request) => request,
            Err(VotingError::Busy { .. }) => prepared
                .keystone_request(&db, &NoopProgressReporter)
                .unwrap(),
            Err(error) => panic!("unexpected request failure: {error}"),
        })
        .collect();
    assert_eq!(requests[0], requests[1]);
}

#[test]
fn missing_keystone_transaction_fails_without_rebuilding() {
    let (db, _, _, prepared) = prepared_wallet_delegation_fixture();
    let original = prepared.setup(&db, &NoopProgressReporter).unwrap();
    db.conn()
        .execute("UPDATE bundles SET delegation_pczt = NULL", [])
        .unwrap();
    let error = prepared
        .keystone_request(&db, &NoopProgressReporter)
        .unwrap_err();
    assert!(matches!(
        error,
        VotingError::DelegationPcztUnavailable { .. }
    ));
    assert_eq!(
        queries::load_pczt_sighash(&db.conn(), &prepared.round_id, &db.wallet_id(), 0).unwrap(),
        original.pczt_sighash
    );
    assert!(!error.retryable());
}

#[test]
fn setup_replacement_persists_the_replacement_keystone_transaction() {
    let (db, _, _, prepared) = prepared_wallet_delegation_fixture();
    let original = prepared
        .keystone_request(&db, &NoopProgressReporter)
        .unwrap();
    let mut replacement = prepared.clone();
    replacement.delegation_keys.hotkey_raw_address =
        *crate::VotingHotkey::from_stored_secret(&[0x47; 64], Network::Regtest)
            .unwrap()
            .raw_orchard_address();
    let setup = replacement.setup(&db, &NoopProgressReporter).unwrap();
    let request = replacement
        .keystone_request(&db, &NoopProgressReporter)
        .unwrap();
    assert_ne!(request.pczt_bytes, original.pczt_bytes);
    assert_eq!(request.pczt_bytes, setup.pczt_bytes);
    assert_eq!(request.pczt_sighash, setup.pczt_sighash);
    assert!(prepared
        .keystone_request(&db, &NoopProgressReporter)
        .is_err());
}

#[test]
fn persisted_keystone_request_rejects_changed_notes_target_and_bytes() {
    let (db, _, _, prepared) = prepared_wallet_delegation_fixture();
    let original = prepared
        .keystone_request(&db, &NoopProgressReporter)
        .unwrap();
    let mut other_notes = prepared.clone();
    other_notes.bundle_note_infos[0].value += 1;
    assert!(other_notes
        .keystone_request(&db, &NoopProgressReporter)
        .is_err());
    let mut other_target = prepared.clone();
    other_target.delegation_keys.hotkey_raw_address =
        *crate::VotingHotkey::from_stored_secret(&[0x47; 64], Network::Regtest)
            .unwrap()
            .raw_orchard_address();
    assert!(other_target
        .keystone_request(&db, &NoopProgressReporter)
        .is_err());
    assert_eq!(
        prepared
            .keystone_request(&db, &NoopProgressReporter)
            .unwrap(),
        original
    );
    db.conn()
        .execute("UPDATE bundles SET delegation_pczt = X'00'", [])
        .unwrap();
    assert!(prepared
        .keystone_request(&db, &NoopProgressReporter)
        .is_err());
}
