//! `ensure_round`: an existing round is reused only for the exact
//! parameters it was stored with.

use crate::{round::VotingDb, Network, VotingError, VotingRoundParams};

const ROUND_ID: &str = "0101010101010101010101010101010101010101010101010101010101010101";

fn round_params() -> VotingRoundParams {
    VotingRoundParams {
        vote_round_id: ROUND_ID.to_string(),
        snapshot_height: 1000,
        ea_pk: vec![0xEA; 32],
        nc_root: vec![0xAA; 32],
        nullifier_imt_root: vec![0xBB; 32],
    }
}

fn db_with_round() -> VotingDb {
    let db = VotingDb::open_in_memory().unwrap();
    db.set_wallet_id("wallet-ensure-round");
    db.ensure_round(Network::Testnet, &round_params(), None)
        .unwrap();
    db
}

#[test]
fn identical_parameters_reuse_the_stored_round() {
    let db = db_with_round();
    db.ensure_round(Network::Testnet, &round_params(), None)
        .unwrap();
}

#[test]
fn a_different_snapshot_height_for_the_same_round_id_is_refused() {
    let db = db_with_round();
    let mut other_snapshot = round_params();
    other_snapshot.snapshot_height = 2000;

    let error = db
        .ensure_round(Network::Testnet, &other_snapshot, None)
        .expect_err("the stored round binds another snapshot");
    assert!(matches!(error, VotingError::InvalidInput { .. }), "{error}");
    assert!(
        error.to_string().contains("different parameters"),
        "{error}"
    );
    // The stored round is untouched.
    assert_eq!(db.get_round_state(ROUND_ID).unwrap().snapshot_height, 1000);
}

#[test]
fn a_different_root_for_the_same_round_id_is_refused() {
    let db = db_with_round();
    let mut other_root = round_params();
    other_root.nc_root = vec![0xCC; 32];

    let error = db
        .ensure_round(Network::Testnet, &other_root, None)
        .expect_err("the stored round binds another commitment root");
    assert!(matches!(error, VotingError::InvalidInput { .. }), "{error}");
}
