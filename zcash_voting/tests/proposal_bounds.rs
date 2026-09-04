use zcash_voting::{validate_proposal_id, MAX_PROPOSAL_ID, MIN_PROPOSAL_ID};

#[test]
fn proposal_bounds_match_circuit_authority() {
    assert!(validate_proposal_id(MIN_PROPOSAL_ID).is_ok());
    assert!(validate_proposal_id(MAX_PROPOSAL_ID).is_ok());
    assert!(validate_proposal_id(MAX_PROPOSAL_ID + 1).is_err());
    assert_eq!(
        voting_circuits::MAX_PROPOSAL_AUTHORITY,
        (1u64 << (MAX_PROPOSAL_ID + 1)) - 1
    );
}
