use super::*;

use pasta_curves::group::ff::PrimeField;
use voting_circuits::delegation::ImtProvider;

const CAPTURED_WALLET: &str = "captured-pir-wallet";
const SELECTED_WALLET: &str = "selected-pir-wallet";

#[test]
fn pir_fetch_persists_under_captured_wallet() {
    let db = VotingDb::open_in_memory().unwrap();
    db.set_wallet_id(CAPTURED_WALLET);
    let imt = voting_circuits::delegation::SpacedLeafImtProvider::new();
    let nullifier = pallas::Base::from(7).to_repr();
    let note = NoteInfo {
        commitment: vec![0x11; 32],
        nullifier: nullifier.to_vec(),
        value: 13_000_000,
        position: 7,
        diversifier: vec![0x12; 11],
        rho: vec![0x13; 32],
        rseed: vec![0x14; 32],
        scope: 0,
        ufvk_str: "uview1capturedpirwallet".to_string(),
    };

    db.precompute_pir_proof_cache_inner(
        CAPTURED_WALLET,
        &[note],
        &[],
        Network::Testnet,
        imt.root(),
        |nullifiers| {
            db.set_wallet_id(SELECTED_WALLET);
            nullifiers
                .iter()
                .map(|nullifier| {
                    imt.non_membership_proof(*nullifier)
                        .map(|proof| pir_client::ImtProofData {
                            root: proof.root,
                            nf_bounds: proof.nf_bounds,
                            leaf_pos: proof.leaf_pos,
                            path: proof.path,
                        })
                        .map_err(|error| VotingError::Internal {
                            message: format!("failed to build test PIR proof: {error}"),
                        })
                })
                .collect()
        },
    )
    .unwrap();

    let conn = db.conn();
    assert!(queries::load_pir_cache_proof(
        &conn,
        CAPTURED_WALLET,
        Network::Testnet,
        &imt.root().to_repr(),
        &nullifier,
    )
    .unwrap()
    .is_some());
    assert!(queries::load_pir_cache_proof(
        &conn,
        SELECTED_WALLET,
        Network::Testnet,
        &imt.root().to_repr(),
        &nullifier,
    )
    .unwrap()
    .is_none());
}
