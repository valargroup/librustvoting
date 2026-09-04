use crate::{delegate::PreparedSigner, delegation_pipeline::KeystoneSignatureSource, VotingError};

use super::fixtures::{pipeline_with_round, ROUND_ID};

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
