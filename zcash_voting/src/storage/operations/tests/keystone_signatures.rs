//! Signature retention must use the bundle context current at commit time.

use super::{fixtures::*, *};
use crate::delegate::{LightwalletdBranchIdProvider, PreparedDelegationBundle, PreparedSigner};

fn signature(bundle_index: u32) -> KeystoneSignatureInput {
    KeystoneSignatureInput {
        bundle_index,
        sig: vec![0x11; 64],
        sighash: vec![0x22; 32],
        rk: vec![0x33; 32],
    }
}

fn database_with_signing_context() -> VotingDb {
    let db = db_with_delegation_setup(2);
    db.conn()
        .execute(
            "UPDATE bundles SET pczt_sighash = ?1, rk = ?2",
            rusqlite::params![signature(0).sighash, signature(0).rk],
        )
        .unwrap();
    db
}

#[test]
fn keystone_signature_batch_rejects_changed_or_missing_setup_atomically() {
    for replacement in ["hash", "key", "cleared", "deleted"] {
        let db = database_with_signing_context();
        let sql = match replacement {
            "hash" => "UPDATE bundles SET pczt_sighash = zeroblob(32) WHERE bundle_index = 1",
            "key" => "UPDATE bundles SET rk = zeroblob(32) WHERE bundle_index = 1",
            "cleared" => "UPDATE bundles SET pczt_sighash = NULL, rk = NULL WHERE bundle_index = 1",
            _ => "DELETE FROM bundles WHERE bundle_index = 1",
        };
        db.conn().execute(sql, []).unwrap();
        let error = db
            .store_keystone_signatures_batch(ROUND_ID, &[signature(0), signature(1)])
            .expect_err("a stale tuple must roll back the whole batch");
        assert!(
            matches!(
                error,
                VotingError::KeystoneSignatureConflict { bundle_index: 1 }
            ),
            "{replacement}: {error}"
        );
        assert!(db.get_keystone_signatures(ROUND_ID).unwrap().is_empty());
    }
}

#[test]
fn keystone_signature_replay_rechecks_the_current_bundle_context() {
    let db = database_with_signing_context();
    db.store_keystone_signatures_batch(ROUND_ID, &[signature(0)])
        .unwrap();
    // Model a stale row left by a previous client: matching that row alone
    // must not make retention report success against a different bundle.
    db.conn()
        .execute(
            "UPDATE bundles SET rk = zeroblob(32) WHERE bundle_index = 0",
            [],
        )
        .unwrap();
    let error = db
        .store_keystone_signatures_batch(ROUND_ID, &[signature(1), signature(0)])
        .unwrap_err();
    assert!(matches!(
        error,
        VotingError::KeystoneSignatureConflict { bundle_index: 0 }
    ));
    let retained = db.get_keystone_signatures(ROUND_ID).unwrap();
    assert_eq!(retained.len(), 1);
    assert_eq!(retained[0].bundle_index, 0);
}

#[test]
fn keystone_signature_validated_before_replacement_cannot_poison_retention() {
    let (_, note, fvk_bytes) = ironwood_setup_fixture();
    let path = std::env::temp_dir().join(format!(
        "keystone-retention-{}.sqlite",
        uuid::Uuid::new_v4()
    ));
    let reader = VotingDb::open(path.to_str().unwrap()).unwrap();
    reader.set_wallet_id(W);
    let round_params = test_params_nu6_3();
    reader
        .init_round(Network::Regtest, &round_params, None)
        .unwrap();
    let notes = vec![note];
    let layout = reader.ensure_bundles(ROUND_ID, &notes).unwrap();
    let keys = keys_for_hotkey_byte(&fvk_bytes, 0x43);
    let setup = reader
        .build_governance_pczt(ROUND_ID, 0, &notes, &keys, nu6_3_branch_id())
        .unwrap();
    // Signature validation needs a stored proof, but this regression does not
    // exercise Halo2 proving. All transaction and signature inputs are real.
    queries::store_proof(&reader.conn(), ROUND_ID, W, 0, &[0x61; 96]).unwrap();
    let request = reader
        .get_delegation_signing_request(ROUND_ID, 0, &keys)
        .unwrap();
    let alpha = Option::<pallas::Scalar>::from(pallas::Scalar::from_repr(request.alpha)).unwrap();
    let (rk, sig) = test_randomized_spendauth_signature(&[0x42; 32], 0, &alpha, &request.sighash);
    assert_eq!(rk.as_slice(), setup.rk);
    let prepared = PreparedDelegationBundle {
        round_id: ROUND_ID.to_string(),
        round_params: round_params.clone(),
        bundle_index: 0,
        layout,
        bundle_note_infos: notes.clone(),
        delegation_keys: keys,
        branch_id_provider: LightwalletdBranchIdProvider::for_height(
            Network::Regtest,
            round_params.snapshot_height,
        )
        .unwrap(),
        anchor_tree_state_bytes: Vec::new(),
        network: Network::Regtest,
        round_name: "test-round".to_string(),
    };
    let signed = prepared
        .signed_bundle(
            &reader,
            Vec::new(),
            PreparedSigner::signature(sig, request.sighash),
        )
        .unwrap();

    let writer = VotingDb::open(path.to_str().unwrap()).unwrap();
    writer.set_wallet_id(W);
    assert!(!reader.shares_connection_with(&writer));
    let replacement_keys = keys_for_hotkey_byte(&fvk_bytes, 0x44);
    let replacement = writer
        .build_governance_pczt(ROUND_ID, 0, &notes, &replacement_keys, nu6_3_branch_id())
        .unwrap();
    assert_ne!(
        signed.submission.sighash.as_slice(),
        replacement.pczt_sighash
    );
    // The same storage boundary used by retain_provided_keystone_signature,
    // after signed_bundle validated A and the other connection committed B.
    let error = reader
        .store_keystone_signature(
            ROUND_ID,
            0,
            &signed.submission.spend_auth_sig,
            &signed.submission.sighash,
            &signed.submission.rk,
        )
        .unwrap_err();
    assert!(matches!(
        error,
        VotingError::KeystoneSignatureConflict { bundle_index: 0 }
    ));
    assert!(reader.get_keystone_signatures(ROUND_ID).unwrap().is_empty());

    // The failed write must leave B able to accept and recover its own key.
    let request = writer
        .get_delegation_signing_request(ROUND_ID, 0, &replacement_keys)
        .unwrap();
    let alpha = Option::<pallas::Scalar>::from(pallas::Scalar::from_repr(request.alpha)).unwrap();
    let (rk, sig) = test_randomized_spendauth_signature(&[0x42; 32], 0, &alpha, &request.sighash);
    reader
        .store_keystone_signature(ROUND_ID, 0, &sig, &request.sighash, &rk)
        .unwrap();
    let retained = writer.get_keystone_signatures(ROUND_ID).unwrap();
    assert_eq!(retained.len(), 1);
    assert_eq!(retained[0].sighash, replacement.pczt_sighash);
    assert_eq!(retained[0].rk, replacement.rk);
    drop(writer);
    drop(reader);
    std::fs::remove_file(path).unwrap();
}
