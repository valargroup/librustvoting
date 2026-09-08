use std::sync::Arc;

use crate::{
    delegate::{DelegationSubmission, PreparedSigner, SignedDelegationBundle},
    delegation_pipeline::{DelegationSigner, KeystoneSignatureSource},
    governance::BUNDLE_NOTE_SLOTS,
    VotingError,
};

use super::fixtures::{pipeline_with_round, ROUND_ID};

fn store_signing_context(db: &crate::round::VotingDb, sighash: &[u8], rk: &[u8]) {
    db.conn()
        .execute(
            "UPDATE bundles SET pczt_sighash = ?1, rk = ?2 WHERE bundle_index = 0",
            rusqlite::params![sighash, rk],
        )
        .unwrap();
}

fn signed_bundle(sig: [u8; 64], sighash: [u8; 32], rk: [u8; 32]) -> SignedDelegationBundle {
    SignedDelegationBundle {
        submission: DelegationSubmission {
            proof: vec![0x61; 96],
            rk,
            nf_signed: [0x62; 32],
            cmx_new: [0x63; 32],
            gov_comm: [0x64; 32],
            gov_nullifiers: [[0x65; 32]; BUNDLE_NOTE_SLOTS],
            alpha: [0x66; 32],
            vote_round_id: ROUND_ID.to_string(),
            spend_auth_sig: sig,
            sighash,
            tx1_effects: Vec::new(),
        },
        pczt_bytes: Vec::new(),
        eligible_weight_zatoshi: 13_000_000,
        delegated_weight_zatoshi: 13_000_000,
        bundle_count: 1,
        bundle_index: 0,
    }
}

#[test]
fn a_provided_keystone_signature_must_be_64_bytes() {
    let pipeline = pipeline_with_round();

    let error = pipeline
        .keystone_signature(
            0,
            &KeystoneSignatureSource::Provided {
                sig: vec![0x11; 63],
                sighash: vec![0x22; 32],
            },
        )
        .expect_err("a 63-byte signature must be rejected");

    assert!(matches!(error, VotingError::InvalidInput { .. }), "{error}");
    assert!(error.to_string().contains("signature"), "{error}");
}

#[test]
fn a_provided_keystone_sighash_must_be_32_bytes() {
    let pipeline = pipeline_with_round();

    let error = pipeline
        .keystone_signature(
            0,
            &KeystoneSignatureSource::Provided {
                sig: vec![0x11; 64],
                sighash: vec![0x22; 31],
            },
        )
        .expect_err("a 31-byte sighash must be rejected");

    assert!(matches!(error, VotingError::InvalidInput { .. }), "{error}");
    assert!(error.to_string().contains("sighash"), "{error}");
}

#[test]
fn a_valid_provided_keystone_signature_is_resolved() {
    let pipeline = pipeline_with_round();

    let PreparedSigner::Signature { sig, sighash } = pipeline
        .keystone_signature(
            0,
            &KeystoneSignatureSource::Provided {
                sig: vec![0x11; 64],
                sighash: vec![0x22; 32],
            },
        )
        .unwrap();

    assert_eq!(sig, [0x11; 64]);
    assert_eq!(sighash, [0x22; 32]);
}

#[test]
fn a_stored_keystone_signature_is_resolved_only_for_its_bundle() {
    let pipeline = pipeline_with_round();
    store_signing_context(&pipeline.voting_db(), &[0x44; 32], &[0x55; 32]);
    pipeline
        .voting_db()
        .store_keystone_signature(ROUND_ID, 0, &[0x33; 64], &[0x44; 32], &[0x55; 32])
        .unwrap();

    let PreparedSigner::Signature { sig, sighash } = pipeline
        .keystone_signature(0, &KeystoneSignatureSource::Stored)
        .unwrap();
    assert_eq!(sig, [0x33; 64]);
    assert_eq!(sighash, [0x44; 32]);

    let error = pipeline
        .keystone_signature(1, &KeystoneSignatureSource::Stored)
        .expect_err("bundle 1 has no stored signature");
    assert!(matches!(error, VotingError::InvalidInput { .. }), "{error}");
    assert!(
        error
            .to_string()
            .contains("no stored Keystone signature for bundle 1"),
        "{error}"
    );
}

#[test]
fn a_provided_keystone_signature_is_retained_once_verified() {
    let pipeline = pipeline_with_round();
    store_signing_context(&pipeline.voting_db(), &[0x72; 32], &[0x73; 32]);
    let signed = signed_bundle([0x71; 64], [0x72; 32], [0x73; 32]);
    let provided = DelegationSigner::Keystone(KeystoneSignatureSource::Provided {
        sig: vec![0x71; 64],
        sighash: vec![0x72; 32],
    });

    pipeline
        .retain_provided_keystone_signature(&provided, &signed)
        .unwrap();

    let PreparedSigner::Signature { sig, sighash } = pipeline
        .keystone_signature(0, &KeystoneSignatureSource::Stored)
        .expect("a retained signature is recoverable through Stored");
    assert_eq!(sig, [0x71; 64]);
    assert_eq!(sighash, [0x72; 32]);

    // Replaying the same pass is idempotent.
    pipeline
        .retain_provided_keystone_signature(&provided, &signed)
        .unwrap();
}

#[test]
fn software_and_stored_signers_write_no_keystone_row() {
    let pipeline = pipeline_with_round();
    let signed = signed_bundle([0x71; 64], [0x72; 32], [0x73; 32]);
    let software = DelegationSigner::Software(Arc::new(|_| Ok([0x71; 64])));
    let stored = DelegationSigner::Keystone(KeystoneSignatureSource::Stored);

    pipeline
        .retain_provided_keystone_signature(&software, &signed)
        .unwrap();
    pipeline
        .retain_provided_keystone_signature(&stored, &signed)
        .unwrap();

    assert!(pipeline
        .voting_db()
        .get_keystone_signatures(ROUND_ID)
        .unwrap()
        .is_empty());
}

#[test]
fn re_scoping_a_handle_from_voting_db_does_not_reach_the_pipeline() {
    let pipeline = pipeline_with_round();
    store_signing_context(&pipeline.voting_db(), &[0x44; 32], &[0x55; 32]);
    pipeline
        .voting_db()
        .store_keystone_signature(ROUND_ID, 0, &[0x33; 64], &[0x44; 32], &[0x55; 32])
        .unwrap();

    let handle = pipeline.voting_db();
    handle.set_wallet_id("some-other-wallet");

    // The pipeline never hands out its own handle, so the re-scope is
    // confined to the caller's copy and every stage still runs under the
    // captured wallet.
    assert_eq!(pipeline.voting_db().wallet_id(), "pipeline-wallet");
    assert!(handle.shares_connection_with(&pipeline.voting_db()));
    assert!(pipeline
        .keystone_signature(0, &KeystoneSignatureSource::Stored)
        .is_ok());
    assert!(!pipeline.has_persisted_proof(0).unwrap());
    assert_eq!(pipeline.wallet_id(), "pipeline-wallet");
}
