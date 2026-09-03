use crate::chain_submission::{AdvanceDelegation, ChainSubmissionFailureKind};

#[test]
fn delegation_advancement_accepts_exact_spend_auth_signature_bytes() {
    let request = AdvanceDelegation::from_signature_bytes([0x11; 32], 7, &[0x22; 64]).unwrap();

    assert_eq!(request.vote_round_id, [0x11; 32]);
    assert_eq!(request.bundle_index, 7);
    assert_eq!(request.spend_auth_signature, [0x22; 64]);
}

#[test]
fn delegation_advancement_rejects_malformed_spend_auth_signature_bytes() {
    for signature in [&[0x22; 63][..], &[0x22; 65][..]] {
        let failure = AdvanceDelegation::from_signature_bytes([0x11; 32], 7, signature)
            .expect_err("non-64-byte delegation signature must fail");

        assert_eq!(failure.kind(), ChainSubmissionFailureKind::InvalidInput);
        assert!(failure.message().contains("must be 64 bytes"));
    }
}
