use crate::{
    delegate_and_vote_batch::delegate_and_vote_batch_sighash,
    vote_commitment::CastVoteBatchSighashAction,
};

fn actions() -> [CastVoteBatchSighashAction<'static>; 2] {
    [
        CastVoteBatchSighashAction {
            r_vpk: &[2; 32],
            van_nullifier: &[3; 32],
            vote_authority_note_new: &[4; 32],
            vote_commitment: &[5; 32],
            proposal_id: 1,
        },
        CastVoteBatchSighashAction {
            r_vpk: &[6; 32],
            van_nullifier: &[7; 32],
            vote_authority_note_new: &[8; 32],
            vote_commitment: &[9; 32],
            proposal_id: 2,
        },
    ]
}

#[test]
fn composite_authorization_matches_vote_sdk_vector() {
    assert_eq!(
        hex::encode(delegate_and_vote_batch_sighash(&[1; 32], &[9; 32], &actions()).unwrap()),
        "1b884143da3d43cd2834a1f347c60d76b2d9a5b0ba5da6f91a4b2b09511f6e23"
    );
}

#[test]
fn authorization_binds_round_initial_van_order_and_membership() {
    let mut actions = actions();
    let original = delegate_and_vote_batch_sighash(&[1; 32], &[9; 32], &actions).unwrap();
    assert_ne!(
        original,
        delegate_and_vote_batch_sighash(&[2; 32], &[9; 32], &actions).unwrap()
    );
    assert_ne!(
        original,
        delegate_and_vote_batch_sighash(&[1; 32], &[8; 32], &actions).unwrap()
    );
    assert_ne!(
        original,
        delegate_and_vote_batch_sighash(&[1; 32], &[9; 32], &actions[..1]).unwrap()
    );
    actions.swap(0, 1);
    assert_ne!(
        original,
        delegate_and_vote_batch_sighash(&[1; 32], &[9; 32], &actions).unwrap()
    );
    actions[0].vote_commitment = &[10; 32];
    assert_ne!(
        original,
        delegate_and_vote_batch_sighash(&[1; 32], &[9; 32], &actions).unwrap()
    );
}

#[test]
fn invalid_effects_are_rejected_without_padding_or_truncation() {
    assert!(delegate_and_vote_batch_sighash(&[1; 32], &[9; 32], &[]).is_err());
    let mut actions = actions();
    actions[0].r_vpk = &[2; 31];
    assert!(delegate_and_vote_batch_sighash(&[1; 32], &[9; 32], &actions).is_err());
    actions[0].r_vpk = &[2; 33];
    assert!(delegate_and_vote_batch_sighash(&[1; 32], &[9; 32], &actions).is_err());
    actions[0].r_vpk = &[2; 32];
    actions[0].proposal_id = 2;
    assert!(delegate_and_vote_batch_sighash(&[1; 32], &[9; 32], &actions).is_err());
}
