use super::*;

#[tokio::test]
async fn a_recovery_row_identity_mismatch_is_refused_before_dispatch() {
    use crate::types::EncryptedShare;
    use crate::vote::VoteRecoveryBundle;

    let db = test_db();
    // Recovery JSON whose embedded proposal disagrees with the row it is
    // stored on, as a migrated or inconsistent database could hold.
    let recovery = VoteRecoveryBundle {
        vote_round_id: ROUND_ID.to_string(),
        bundle_index: 0,
        proposal_id: 2,
        vote_decision: 0,
        anchor_height: 123,
        vc_tree_position: 0,
        single_share: false,
        num_options: 3,
        van_nullifier: [0x10; 32],
        vote_authority_note_new: [0x11; 32],
        vote_commitment: [0x12; 32],
        proof: vec![0x13; 96],
        shares_hash: [0x14; 32],
        r_vpk: [0x15; 32],
        alpha_v: [0x16; 32],
        vote_auth_sig: [0x17; 64],
        encrypted_shares: vec![EncryptedShare {
            c1: vec![0x21; 32],
            c2: vec![0x22; 32],
            share_index: 0,
            plaintext_value: 5,
            randomness: vec![0x23; 32],
        }],
        share_blinds: vec![[0x41; 32]],
        share_comms: vec![[0x51; 32]],
        batch: None,
    };
    store_vote_with_recovery(&db, 1, &recovery);
    let transport = Arc::new(MockTransport::default());
    let client = accepted_client(transport.clone());
    let lifecycle = ChainSubmissionLifecycle::new(&db, &client);

    let error = lifecycle
        .submit_vote(ROUND_ID, 0, 1, &|| false)
        .await
        .unwrap_err();

    // Serializing the embedded identity while journaling the requested one
    // would spend this bundle's VAN on the wrong proposal, and the
    // reservation rebuild would reproduce the mismatch rather than catch it.
    assert!(error.to_string().contains("mismatch"), "{error}");
    assert_eq!(*transport.posts.lock().unwrap(), 0);
}

#[test]
fn the_identity_lock_registry_is_bounded_by_live_operations() {
    let key = format!("registry-test/{ROUND_ID}/delegation/0/-1");
    for _ in 0..64 {
        let lock = identity_operation_lock(&key).unwrap();
        // Two live acquisitions of one identity must share one mutex, or
        // the lock would stop serializing that identity.
        let concurrent = identity_operation_lock(&key).unwrap();
        assert!(Arc::ptr_eq(&lock, &concurrent));
        // Only the registry's weak reference survives this scope.
        assert_eq!(Arc::strong_count(&lock), 2);
    }
    // A long-lived wallet moves through many identities; each must become
    // reclaimable once its operation ends rather than being retained for
    // the process lifetime.
    let after = identity_operation_lock(&key).unwrap();
    assert_eq!(Arc::strong_count(&after), 1);
}

#[test]
fn identities_cannot_pair_a_kind_with_the_wrong_key() {
    let delegation = ChainSubmissionIdentity::delegation(ROUND_ID, 0);
    assert_eq!(delegation.kind(), ChainSubmissionKind::Delegation);
    assert_eq!(delegation.proposal_id(), None);
    assert_eq!(delegation.batch_digest(), None);
    assert!(delegation.require_proposal_id().is_err());
    assert!(delegation.require_batch_digest().is_err());

    let vote = ChainSubmissionIdentity::vote(ROUND_ID, 1, 7);
    assert_eq!(vote.round_id(), ROUND_ID);
    assert_eq!(vote.bundle_index(), 1);
    assert_eq!(vote.proposal_id(), Some(7));
    assert_eq!(vote.batch_digest(), None);

    let batch = ChainSubmissionIdentity::vote_batch(ROUND_ID, 2, [9; 32]);
    assert_eq!(batch.proposal_id(), None);
    assert_eq!(batch.batch_digest(), Some([9; 32]));
    assert_eq!(batch.require_batch_digest().unwrap(), [9; 32]);
}

#[test]
fn confirmed_outcomes_have_one_authoritative_transaction_hash() {
    let confirmation = ChainConfirmation::Vote(VoteConfirmation {
        tx_hash: TX_HASH.to_string(),
        van_leaf_position: 4,
        vc_tree_position: 9,
    });
    let outcome = ChainLifecycleOutcome::Confirmed { confirmation };

    let ChainLifecycleOutcome::Confirmed { confirmation } = outcome else {
        unreachable!("constructed a confirmed outcome")
    };
    assert_eq!(confirmation.tx_hash(), TX_HASH);
}
